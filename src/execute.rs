use nix::sys::signal::{self, Signal};
use nix::sys::wait::{waitpid, WaitStatus};
use nix::unistd::{self, ForkResult, Pid};
use std::collections::HashMap;
use std::env;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::{Config, State};

pub struct Job {
    pub pid: i32,
    pub cmd: String,
}

/// Execute a command line, handling pipes, redirects, builtins, nicks
pub fn execute(
    line: &str,
    config: &mut Config,
    state: &mut State,
    exe_cache: &[String],
    jobs: &mut HashMap<u32, Job>,
) -> i32 {
    let line = line.trim();
    if line.is_empty() {
        return 0;
    }

    // Expand nicks
    let line = expand_nicks(line, &config.nick, &config.gnick);

    // Handle colon commands
    if line.starts_with(':') {
        return handle_colon_command(&line, config, state);
    }

    // Builtins
    let parts = shell_split(&line);
    if parts.is_empty() {
        return 0;
    }

    match parts[0].as_str() {
        "cd" => {
            let dir = if parts.len() > 1 {
                expand_tilde(&parts[1])
            } else {
                dirs::home_dir().unwrap_or_default().to_string_lossy().to_string()
            };
            if dir == "-" {
                if let Ok(old) = env::var("OLDPWD") {
                    let prev = env::current_dir().unwrap_or_default();
                    if env::set_current_dir(&old).is_ok() {
                        env::set_var("OLDPWD", prev);
                        println!("{}", old);
                    }
                }
            } else if let Err(e) = env::set_current_dir(&dir) {
                eprintln!("cd: {}: {}", dir, e);
                return 1;
            } else {
                let prev = env::var("OLDPWD").unwrap_or_default();
                env::set_var("OLDPWD", env::current_dir().unwrap_or_default());
                // Track directory history
                let cwd = env::current_dir().unwrap_or_default().to_string_lossy().to_string();
                state.dirs.retain(|d| d != &cwd);
                state.dirs.insert(0, cwd);
                state.dirs.truncate(10);
            }
            return 0;
        }
        "exit" | "quit" => {
            state.save();
            config.save();
            std::process::exit(0);
        }
        "export" => {
            if parts.len() >= 2 {
                if let Some((key, val)) = parts[1].split_once('=') {
                    env::set_var(key, val);
                }
            }
            return 0;
        }
        "unset" => {
            if parts.len() >= 2 {
                env::remove_var(&parts[1]);
            }
            return 0;
        }
        _ => {}
    }

    // Check if it's a directory (cd into it)
    let expanded = expand_tilde(&parts[0]);
    if Path::new(&expanded).is_dir() && parts.len() == 1 {
        let _ = env::set_current_dir(&expanded);
        let cwd = env::current_dir().unwrap_or_default().to_string_lossy().to_string();
        state.dirs.retain(|d| d != &cwd);
        state.dirs.insert(0, cwd);
        state.dirs.truncate(10);
        return 0;
    }

    // Check for bookmark
    if let Some(path) = config.bookmarks.get(parts[0].as_str()) {
        let path = path.clone();
        if Path::new(&path).is_dir() {
            let _ = env::set_current_dir(&path);
            return 0;
        }
    }

    // Track frequency
    *state.cmd_frequency.entry(parts[0].clone()).or_insert(0) += 1;

    // Has pipes or redirects? Use shell
    if line.contains('|') || line.contains('>') || line.contains('<')
        || line.contains("&&") || line.contains("||") || line.contains('`')
        || line.contains("$(")
    {
        return run_via_shell(&line, jobs);
    }

    // Background job?
    let (cmd_line, background) = if line.ends_with('&') {
        (line[..line.len() - 1].trim(), true)
    } else {
        (line.as_str(), false)
    };

    // Direct execution
    run_command(cmd_line, background, jobs)
}

fn run_via_shell(line: &str, jobs: &mut HashMap<u32, Job>) -> i32 {
    let status = Command::new("bash")
        .arg("-c")
        .arg(line)
        .status();

    match status {
        Ok(s) => s.code().unwrap_or(1),
        Err(e) => {
            eprintln!("rush: {}", e);
            127
        }
    }
}

fn run_command(line: &str, background: bool, jobs: &mut HashMap<u32, Job>) -> i32 {
    let parts = shell_split(line);
    if parts.is_empty() {
        return 0;
    }

    let cmd = &parts[0];
    let args: Vec<&str> = parts[1..].iter().map(|s| s.as_str()).collect();

    if background {
        match Command::new(cmd).args(&args).spawn() {
            Ok(child) => {
                let id = jobs.len() as u32 + 1;
                let pid = child.id() as i32;
                println!("[{}] {}", id, pid);
                jobs.insert(id, Job { pid, cmd: line.to_string() });
                0
            }
            Err(e) => {
                eprintln!("rush: {}: {}", cmd, e);
                127
            }
        }
    } else {
        match Command::new(cmd).args(&args).status() {
            Ok(s) => s.code().unwrap_or(1),
            Err(e) => {
                eprintln!("rush: {}: command not found", cmd);
                127
            }
        }
    }
}

fn handle_colon_command(line: &str, config: &mut Config, state: &mut State) -> i32 {
    let line = &line[1..]; // strip ':'
    let parts: Vec<&str> = line.splitn(2, ' ').collect();
    let cmd = parts[0];
    let args = if parts.len() > 1 { parts[1].trim() } else { "" };

    match cmd {
        "nick" => {
            if args.is_empty() {
                for (k, v) in &config.nick {
                    println!("  {} = {}", k, v);
                }
            } else if args.starts_with('-') {
                config.nick.remove(&args[1..]);
                config.save();
            } else if let Some((name, val)) = args.split_once('=') {
                config.nick.insert(name.trim().to_string(), val.trim().to_string());
                config.save();
            }
            0
        }
        "gnick" => {
            if args.is_empty() {
                for (k, v) in &config.gnick {
                    println!("  {} = {}", k, v);
                }
            } else if args.starts_with('-') {
                config.gnick.remove(&args[1..]);
                config.save();
            } else if let Some((name, val)) = args.split_once('=') {
                config.gnick.insert(name.trim().to_string(), val.trim().to_string());
                config.save();
            }
            0
        }
        "bm" | "bookmark" => {
            if args.is_empty() {
                for (k, v) in &config.bookmarks {
                    println!("  {} -> {}", k, v);
                }
            } else if args.starts_with('-') {
                config.bookmarks.remove(&args[1..]);
                config.save();
            } else {
                let bm_parts: Vec<&str> = args.splitn(2, ' ').collect();
                let name = bm_parts[0];
                let path = if bm_parts.len() > 1 {
                    bm_parts[1].to_string()
                } else {
                    env::current_dir().unwrap_or_default().to_string_lossy().to_string()
                };
                config.bookmarks.insert(name.to_string(), path);
                config.save();
            }
            0
        }
        "dirs" => {
            for (i, d) in state.dirs.iter().enumerate() {
                println!("  {} {}", i, d);
            }
            0
        }
        "history" => {
            let count = args.parse::<usize>().unwrap_or(50);
            let start = state.history.len().saturating_sub(count);
            for (i, cmd) in state.history[start..].iter().enumerate() {
                println!("  {:4} {}", start + i, cmd);
            }
            0
        }
        "rehash" => {
            state.exe_cache = build_exe_cache();
            state.exe_cache_time = now_secs();
            println!("Executable cache rebuilt: {} commands", state.exe_cache.len());
            0
        }
        "help" => {
            println!("\x1b[1mrush commands:\x1b[0m");
            println!("  :nick [name = val | -name]   Aliases");
            println!("  :gnick [name = val | -name]  Global aliases");
            println!("  :bm [name [path] | -name]    Bookmarks");
            println!("  :dirs                         Directory history");
            println!("  :history [n]                  Command history");
            println!("  :rehash                       Rebuild command cache");
            println!("  :help                         This help");
            0
        }
        _ => {
            eprintln!("rush: unknown command :{}", cmd);
            1
        }
    }
}

pub fn expand_nicks(line: &str, nicks: &HashMap<String, String>, gnicks: &HashMap<String, String>) -> String {
    let mut result = line.to_string();

    // Apply gnicks (global, anywhere in line)
    for (k, v) in gnicks {
        result = result.replace(k.as_str(), v.as_str());
    }

    // Apply nicks (only at command position)
    let parts: Vec<&str> = result.splitn(2, ' ').collect();
    if let Some(expanded) = nicks.get(parts[0]) {
        if parts.len() > 1 {
            result = format!("{} {}", expanded, parts[1]);
        } else {
            result = expanded.clone();
        }
    }

    result
}

fn expand_tilde(path: &str) -> String {
    if path.starts_with('~') {
        let home = dirs::home_dir().unwrap_or_default().to_string_lossy().to_string();
        path.replacen('~', &home, 1)
    } else {
        path.to_string()
    }
}

/// Simple shell-like word splitting (respects quotes)
pub fn shell_split(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut escape = false;

    for ch in line.chars() {
        if escape {
            current.push(ch);
            escape = false;
            continue;
        }
        if ch == '\\' && !in_single {
            escape = true;
            continue;
        }
        if ch == '\'' && !in_double {
            in_single = !in_single;
            continue;
        }
        if ch == '"' && !in_single {
            in_double = !in_double;
            continue;
        }
        if ch.is_whitespace() && !in_single && !in_double {
            if !current.is_empty() {
                words.push(expand_tilde(&current));
                current.clear();
            }
            continue;
        }
        current.push(ch);
    }
    if !current.is_empty() {
        words.push(expand_tilde(&current));
    }
    words
}

pub fn build_exe_cache() -> Vec<String> {
    let mut exes = Vec::new();
    if let Ok(path) = env::var("PATH") {
        for dir in path.split(':') {
            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries.flatten() {
                    if let Ok(ft) = entry.file_type() {
                        if ft.is_file() || ft.is_symlink() {
                            if let Some(name) = entry.file_name().to_str() {
                                exes.push(name.to_string());
                            }
                        }
                    }
                }
            }
        }
    }
    exes.sort();
    exes.dedup();
    exes
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
