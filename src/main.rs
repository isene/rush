mod config;
mod execute;
mod input;
mod plugin;
mod prompt;

use config::{Config, State};
use execute::{build_exe_cache, now_secs, Job, Recording};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

static RELOAD_FLAG: AtomicBool = AtomicBool::new(false);

extern "C" fn sigusr1_handler(_sig: libc::c_int) {
    RELOAD_FLAG.store(true, Ordering::Relaxed);
}

/// Install SIGTSTP handler so Ctrl-Z does not suspend rush itself.
/// Child processes still receive the signal via their own process group.
fn setup_signal_handlers() {
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_IGN);   // Ctrl+C: ignore in shell, children restore SIG_DFL
        libc::signal(libc::SIGTSTP, libc::SIG_IGN);  // Ctrl+Z: ignore in shell
        libc::signal(libc::SIGHUP, libc::SIG_DFL);   // Terminal closed: exit
        libc::signal(libc::SIGUSR1, sigusr1_handler as libc::sighandler_t); // Config reload from crush
    }
}

const TIPS: &[&str] = &[
    "Use :nick to create command aliases",
    "Tab cycles completions, Shift-Tab searches history",
    "Ctrl-G edits the current line in your $EDITOR",
    "!! repeats the last command, !-2 the one before",
    ":bm name saves the current directory as a bookmark",
    ":theme dracula for a nice dark theme",
    "= expr sends math to xrpn calculator",
    "@ question asks AI for help",
    "@@ task asks AI to suggest a command",
    ":stats shows your most-used commands",
    "Right arrow accepts the grayed-out history suggestion",
    ":validate rm -rf = confirm adds a safety rule",
    "Ctrl-Y copies the current line to clipboard",
    ":record start name to record a command sequence",
    "cd 3 jumps to the 3rd entry in :dirs history",
    "r launches your file manager",
];

fn main() {
    // Handle -c flag (run command and exit)
    let args: Vec<String> = std::env::args().collect();
    if args.len() >= 3 && args[1] == "-c" {
        let cmd = args[2..].join(" ");
        let mut config = Config::load();
        let mut state = State::load();
        let exe_cache = build_exe_cache();
        let mut jobs: HashMap<u32, Job> = HashMap::new();
        let mut recording: Option<Recording> = None;
        let mut plugins = plugin::PluginManager::new();
        let code = execute::execute(&cmd, &mut config, &mut state, &exe_cache, &mut jobs, &mut recording, &mut plugins);
        std::process::exit(code);
    }

    // Handle --login / -l flag
    let is_login = args.iter().any(|a| a == "--login" || a == "-l");
    if is_login {
        source_login_files();
    }

    // Source LS_COLORS
    source_lscolors();

    // First-run welcome
    let first_run = !Config::config_path().exists();

    // Load config and state
    let mut config = Config::load();
    let mut state = State::load();

    if first_run {
        println!("\x1b[1mWelcome to rush!\x1b[0m");
        println!();
        println!("Quick start:");
        println!("  :help          Show all commands");
        println!("  :nick          List/set aliases");
        println!("  :bm            Manage bookmarks");
        println!("  :theme <name>  Set color theme (try: dracula, nord, gruvbox)");
        println!("  :calc <expr>   Calculator");
        println!("  :import_rsh    Import nicks/bookmarks from ~/.rshrc");
        println!("  Ctrl-G         Edit line in $EDITOR");
        println!("  Tab            Command/file completion");
        println!("  !!             Repeat last command");
        println!();
        config.save();
    } else if config.show_tips {
        // Show random tip ~30% of the time
        let rng = now_secs() % 10;
        if rng < 3 {
            let tip_idx = (now_secs() as usize) % TIPS.len();
            println!("\x1b[38;5;243mTip: {}\x1b[0m", TIPS[tip_idx]);
        }
    }

    // Build or refresh executable cache (60s TTL)
    let now = now_secs();
    let mut exe_cache = if now - state.exe_cache_time > 60 || state.exe_cache.is_empty() {
        let cache = build_exe_cache();
        state.exe_cache = cache.clone();
        state.exe_cache_time = now;
        cache
    } else {
        state.exe_cache.clone()
    };

    let mut jobs: HashMap<u32, Job> = HashMap::new();
    let mut recording: Option<Recording> = None;
    let mut last_autosave = now_secs();
    let mut last_cmd_duration: f64 = 0.0;

    // Load plugins
    let mut plugins = plugin::PluginManager::new();
    plugins.load_all();

    // Ignore SIGTSTP in rush itself; children handle it via process groups
    setup_signal_handlers();

    // Main loop
    loop {
        // Exit if terminal is gone
        if unsafe { libc::isatty(0) } == 0 {
            state.save();
            config.save();
            break;
        }

        // Session autosave
        if config.session_autosave > 0 {
            let now = now_secs();
            if now - last_autosave >= config.session_autosave {
                state.save();
                last_autosave = now;
            }
        }

        // Cleanup finished background jobs
        execute::cleanup_jobs(&mut jobs);

        // Refresh exe cache if stale (60s TTL)
        let now = now_secs();
        if now - state.exe_cache_time > 60 {
            exe_cache = build_exe_cache();
            state.exe_cache = exe_cache.clone();
            state.exe_cache_time = now;
        }

        let line = match input::getline(&config, &mut state, &exe_cache, last_cmd_duration) {
            Some(line) => line,
            None => {
                state.save();
                config.save();
                break;
            }
        };

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Add to history
        let should_add = match config.history_dedup.as_str() {
            "off" => true,
            "full" => !state.history.contains(&trimmed.to_string()),
            _ => state.history.last().map(|l| l != trimmed).unwrap_or(true),
        };
        if should_add {
            state.history.push(trimmed.to_string());
            state.history_times.push(now_secs());
            if state.history.len() > 200 {
                state.history.remove(0);
                if !state.history_times.is_empty() {
                    state.history_times.remove(0);
                }
            }
        }

        // Pre-command plugin hook
        let cwd = std::env::current_dir().unwrap_or_default().to_string_lossy().to_string();
        let ctx = plugin::PluginContext {
            hook: "pre_cmd".to_string(),
            command: Some(trimmed.to_string()),
            args: None,
            cwd: cwd.clone(),
            exit_code: None,
            word: None,
            line: None,
        };
        if let Some(resp) = plugins.run_hook("pre_cmd", &ctx) {
            if resp.action == "block" {
                if !resp.message.is_empty() {
                    eprintln!("[plugin] {}", resp.message);
                }
                continue;
            }
        }

        // Time the command
        let start = std::time::Instant::now();

        // Execute
        let exit_code = execute::execute(trimmed, &mut config, &mut state, &exe_cache, &mut jobs, &mut recording, &mut plugins);

        // Track command duration for right prompt
        last_cmd_duration = start.elapsed().as_secs_f64();

        // Post-command plugin hook
        let post_ctx = plugin::PluginContext {
            hook: "post_cmd".to_string(),
            command: Some(trimmed.to_string()),
            args: None,
            cwd: std::env::current_dir().unwrap_or_default().to_string_lossy().to_string(),
            exit_code: Some(exit_code),
            word: None,
            line: None,
        };
        if let Some(resp) = plugins.run_hook("post_cmd", &post_ctx) {
            if !resp.message.is_empty() {
                println!("{}", resp.message);
            }
        }

        // Set PIPESTATUS env var
        std::env::set_var("PIPESTATUS", exit_code.to_string());

        // Slow command alert
        let elapsed = start.elapsed().as_secs();
        if config.slow_command_threshold > 0 && elapsed >= config.slow_command_threshold {
            println!("\x1b[38;5;214m[rush] Command took {}s (threshold: {}s)\x1b[0m",
                elapsed, config.slow_command_threshold);
        }

        // Refresh exe cache if :rehash was called
        if trimmed.starts_with(":rehash") {
            exe_cache = state.exe_cache.clone();
        }
        // Refresh config if :reload was called
        if trimmed.starts_with(":reload") {
            // Config already reloaded by the command handler
        }
        // Check SIGUSR1 flag (sent by crush on exit)
        if RELOAD_FLAG.swap(false, Ordering::Relaxed) {
            config = Config::load();
        }
    }
}

fn source_lscolors() {
    // Source LS_COLORS from common locations
    for path in &[
        "/home/geir/.local/share/lscolors.sh",
        "~/.local/share/lscolors.sh",
        "/etc/dircolors",
    ] {
        let expanded = if path.starts_with('~') {
            let home = dirs::home_dir().unwrap_or_default().to_string_lossy().to_string();
            path.replacen('~', &home, 1)
        } else {
            path.to_string()
        };
        if std::path::Path::new(&expanded).exists() {
            if let Ok(content) = std::fs::read_to_string(&expanded) {
                // Extract LS_COLORS='...' value directly
                if let Some(start) = content.find("LS_COLORS='") {
                    let rest = &content[start + 11..];
                    if let Some(end) = rest.find('\'') {
                        std::env::set_var("LS_COLORS", &rest[..end]);
                        return;
                    }
                }
                // Fallback: source via bash
                if let Ok(output) = std::process::Command::new("bash")
                    .arg("-c")
                    .arg(format!("source {} 2>/dev/null && echo $LS_COLORS", expanded))
                    .output()
                {
                    let val = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    if !val.is_empty() {
                        std::env::set_var("LS_COLORS", &val);
                        return;
                    }
                }
            }
        }
    }
}

fn source_login_files() {
    // Source standard login files
    for path in &["/etc/profile", "~/.profile", "~/.bash_profile"] {
        let expanded = if path.starts_with('~') {
            let home = dirs::home_dir().unwrap_or_default().to_string_lossy().to_string();
            path.replacen('~', &home, 1)
        } else {
            path.to_string()
        };
        if std::path::Path::new(&expanded).exists() {
            // Source by running in a subshell and importing env changes
            let _ = std::process::Command::new("bash")
                .arg("-c")
                .arg(format!("source {} 2>/dev/null && env", expanded))
                .output()
                .ok()
                .map(|o| {
                    let output = String::from_utf8_lossy(&o.stdout);
                    for line in output.lines() {
                        if let Some((key, val)) = line.split_once('=') {
                            if !key.contains(' ') && !key.is_empty() {
                                std::env::set_var(key, val);
                            }
                        }
                    }
                });
        }
    }
}
