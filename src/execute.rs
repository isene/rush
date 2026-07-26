use crust::style;
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::Pid;
use std::collections::HashMap;
use std::env;
use std::io::{self, Write};
use std::path::Path;
use std::process::Command;

use crate::config::{self, Bookmark, Config, State};

/// Status for a job (running or stopped)
pub enum JobStatus {
    Running,
    Stopped,
}

impl std::fmt::Display for JobStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JobStatus::Running => write!(f, "Running"),
            JobStatus::Stopped => write!(f, "Stopped"),
        }
    }
}

pub struct Job {
    pub pid: i32,
    pub cmd: String,
    pub status: JobStatus,
}

/// Active recording state (name, list of commands)
pub struct Recording {
    pub name: String,
    pub commands: Vec<String>,
}

/// Expand history references: !!, !-N, !N
fn expand_history(line: &str, history: &[String]) -> String {
    let mut result = line.to_string();

    // !! -> last command
    if result.contains("!!") {
        if let Some(last) = history.last() {
            result = result.replace("!!", last);
        }
    }

    // !-N -> Nth previous command
    let re_neg = regex::Regex::new(r"!-(\d+)").unwrap();
    let result_clone = result.clone();
    for cap in re_neg.captures_iter(&result_clone) {
        let n: usize = cap[1].parse().unwrap_or(0);
        if n > 0 && n <= history.len() {
            let cmd = &history[history.len() - n];
            result = result.replacen(&cap[0], cmd, 1);
        }
    }

    // !N -> command number N (index into history)
    let re_num = regex::Regex::new(r"!(\d+)").unwrap();
    let result_clone = result.clone();
    for cap in re_num.captures_iter(&result_clone) {
        let n: usize = cap[1].parse().unwrap_or(0);
        if n < history.len() {
            let cmd = &history[n];
            result = result.replacen(&cap[0], cmd, 1);
        }
    }

    result
}

/// Levenshtein distance between two strings
fn levenshtein(a: &str, b: &str) -> usize {
    let a_len = a.len();
    let b_len = b.len();
    if a_len == 0 { return b_len; }
    if b_len == 0 { return a_len; }

    let mut prev: Vec<usize> = (0..=b_len).collect();
    let mut curr = vec![0; b_len + 1];

    for (i, ca) in a.chars().enumerate() {
        curr[0] = i + 1;
        for (j, cb) in b.chars().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            curr[j + 1] = (prev[j + 1] + 1)
                .min(curr[j] + 1)
                .min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b_len]
}

/// Evaluate a math expression (simple recursive descent)
fn calc_eval(expr: &str) -> Result<f64, String> {
    let expr = expr.trim();
    if expr.is_empty() {
        return Err("empty expression".to_string());
    }

    // Replace constants
    let expr = expr.replace("pi", &std::f64::consts::PI.to_string())
                   .replace(" e ", &format!(" {} ", std::f64::consts::E))
                   .replace("(e)", &format!("({})", std::f64::consts::E))
                   .replace("(e,", &format!("({},", std::f64::consts::E));
    // Handle leading 'e'
    let expr = if expr.starts_with("e ") || expr.starts_with("e+") || expr.starts_with("e-")
        || expr.starts_with("e*") || expr.starts_with("e/") || expr == "e" {
        format!("{}{}", std::f64::consts::E, &expr[1..])
    } else { expr };

    calc_parse_expr(&expr, &mut 0)
}

fn calc_parse_expr(expr: &str, pos: &mut usize) -> Result<f64, String> {
    let mut left = calc_parse_term(expr, pos)?;
    while *pos < expr.len() {
        let ch = expr.as_bytes().get(*pos).copied();
        match ch {
            Some(b'+') => { *pos += 1; left += calc_parse_term(expr, pos)?; }
            Some(b'-') => { *pos += 1; left -= calc_parse_term(expr, pos)?; }
            _ => break,
        }
    }
    Ok(left)
}

fn calc_parse_term(expr: &str, pos: &mut usize) -> Result<f64, String> {
    let mut left = calc_parse_power(expr, pos)?;
    while *pos < expr.len() {
        let ch = expr.as_bytes().get(*pos).copied();
        match ch {
            Some(b'*') if expr.as_bytes().get(*pos + 1) != Some(&b'*') => {
                *pos += 1; left *= calc_parse_power(expr, pos)?;
            }
            Some(b'/') => { *pos += 1; let r = calc_parse_power(expr, pos)?; left /= r; }
            Some(b'%') => { *pos += 1; let r = calc_parse_power(expr, pos)?; left %= r; }
            _ => break,
        }
    }
    Ok(left)
}

fn calc_parse_power(expr: &str, pos: &mut usize) -> Result<f64, String> {
    let base = calc_parse_unary(expr, pos)?;
    calc_skip_ws(expr, pos);
    if *pos + 1 < expr.len() && expr.as_bytes()[*pos] == b'*' && expr.as_bytes()[*pos + 1] == b'*' {
        *pos += 2;
        let exp = calc_parse_power(expr, pos)?;
        Ok(base.powf(exp))
    } else {
        Ok(base)
    }
}

fn calc_parse_unary(expr: &str, pos: &mut usize) -> Result<f64, String> {
    calc_skip_ws(expr, pos);
    if *pos < expr.len() && expr.as_bytes()[*pos] == b'-' {
        *pos += 1;
        Ok(-calc_parse_atom(expr, pos)?)
    } else if *pos < expr.len() && expr.as_bytes()[*pos] == b'+' {
        *pos += 1;
        calc_parse_atom(expr, pos)
    } else {
        calc_parse_atom(expr, pos)
    }
}

fn calc_parse_atom(expr: &str, pos: &mut usize) -> Result<f64, String> {
    calc_skip_ws(expr, pos);
    if *pos >= expr.len() {
        return Err("unexpected end of expression".to_string());
    }

    // Parenthesized expression
    if expr.as_bytes()[*pos] == b'(' {
        *pos += 1;
        let val = calc_parse_expr(expr, pos)?;
        calc_skip_ws(expr, pos);
        if *pos < expr.len() && expr.as_bytes()[*pos] == b')' {
            *pos += 1;
        }
        return Ok(val);
    }

    // Functions: sqrt, sin, cos, tan, log
    for (fname, flen) in &[("sqrt", 4), ("sin", 3), ("cos", 3), ("tan", 3), ("log", 3)] {
        if expr[*pos..].starts_with(fname) {
            *pos += flen;
            calc_skip_ws(expr, pos);
            // Expect ( or just a number
            let arg = if *pos < expr.len() && expr.as_bytes()[*pos] == b'(' {
                *pos += 1;
                let v = calc_parse_expr(expr, pos)?;
                calc_skip_ws(expr, pos);
                if *pos < expr.len() && expr.as_bytes()[*pos] == b')' { *pos += 1; }
                v
            } else {
                calc_parse_atom(expr, pos)?
            };
            return Ok(match *fname {
                "sqrt" => arg.sqrt(),
                "sin" => arg.sin(),
                "cos" => arg.cos(),
                "tan" => arg.tan(),
                "log" => arg.ln(),
                _ => unreachable!(),
            });
        }
    }

    // Number
    let start = *pos;
    while *pos < expr.len() && (expr.as_bytes()[*pos].is_ascii_digit() || expr.as_bytes()[*pos] == b'.') {
        *pos += 1;
    }
    if start == *pos {
        return Err(format!("unexpected character '{}'", &expr[*pos..*pos+1]));
    }
    expr[start..*pos].parse::<f64>().map_err(|e| e.to_string())
}

fn calc_skip_ws(expr: &str, pos: &mut usize) {
    while *pos < expr.len() && expr.as_bytes()[*pos] == b' ' {
        *pos += 1;
    }
}

/// Check validation rules before executing a command.
/// Returns true if command should proceed, false if blocked.
fn check_validation_rules(line: &str, rules: &HashMap<String, String>) -> bool {
    for (pattern, action) in rules {
        let re = match regex::Regex::new(pattern) {
            Ok(r) => r,
            Err(_) => continue,
        };
        if re.is_match(line) {
            match action.as_str() {
                "block" => {
                    eprintln!("rush: command blocked by validation rule (pattern: {})", pattern);
                    return false;
                }
                "confirm" => {
                    eprint!("rush: validation rule matched ({}). Proceed? [y/N] ", pattern);
                    io::stderr().flush().ok();
                    let mut answer = String::new();
                    if io::stdin().read_line(&mut answer).is_ok() {
                        let a = answer.trim().to_lowercase();
                        if a != "y" && a != "yes" {
                            eprintln!("Aborted.");
                            return false;
                        }
                    } else {
                        return false;
                    }
                }
                "warn" => {
                    eprintln!("rush: warning, validation rule matched (pattern: {})", pattern);
                }
                _ => {}
            }
        }
    }
    true
}

/// Execute a command line, handling pipes, redirects, builtins, nicks
pub fn execute(
    line: &str,
    config: &mut Config,
    state: &mut State,
    exe_cache: &[String],
    jobs: &mut HashMap<u32, Job>,
    recording: &mut Option<Recording>,
    plugins: &mut crate::plugin::PluginManager,
) -> i32 {
    let line = line.trim();
    if line.is_empty() {
        return 0;
    }

    // Clean up completed background jobs
    cleanup_jobs(jobs);

    // Expand history (!!, !-N, !N) before nick expansion
    let line = expand_history(line, &state.history);

    // Expand nicks (with parametrized support)
    let line = expand_nicks(&line, &config.nick, &config.gnick);

    // Handle colon commands
    if line.starts_with(':') {
        return handle_colon_command(&line, config, state, jobs, recording, plugins);
    }

    // Handle AI prompts (@ and @@)
    if line.starts_with("@@") {
        return handle_ai_command(&line[2..].trim(), true);
    }
    if line.starts_with('@') {
        return handle_ai_command(&line[1..].trim(), false);
    }

    // Show timestamp + expanded command if enabled
    if config.show_cmd && !line.starts_with('=') {
        let now = chrono_time();
        println!("{}", style::styled(&format!("{now}: {line}"), Some(config.c_stamp), None, ""));
    }

    // Check validation rules
    if !check_validation_rules(&line, &config.validation_rules) {
        return 1;
    }

    // Record command if recording is active
    if let Some(ref mut rec) = recording {
        if !line.starts_with(":record") {
            rec.commands.push(line.clone());
        }
    }

    // xrpn integration: = expr
    if line.starts_with('=') {
        let expr = &line[1..].trim();
        let cmd = format!("echo \"{},prx,off\" | xrpn", expr);
        return run_via_shell(&cmd, jobs);
    }

    // Builtins
    let parts = shell_split(&line);
    if parts.is_empty() {
        return 0;
    }

    match parts[0].as_str() {
        "cd" => {
            let dir = if parts.len() > 1 {
                let arg = &parts[1];
                // cd N: jump to Nth directory from history
                if let Ok(n) = arg.parse::<usize>() {
                    if n < state.dirs.len() {
                        state.dirs[n].clone()
                    } else {
                        eprintln!("cd: no directory at index {}", n);
                        return 1;
                    }
                } else {
                    expand_tilde(arg)
                }
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
            } else {
                let prev = env::current_dir().unwrap_or_default();
                if let Err(e) = env::set_current_dir(&dir) {
                    eprintln!("cd: {}: {}", dir, e);
                    return 1;
                }
                env::set_var("OLDPWD", prev);
                // Track directory history
                let cwd = env::current_dir().unwrap_or_default().to_string_lossy().to_string();
                state.dirs.retain(|d| d != &cwd);
                state.dirs.insert(0, cwd);
                state.dirs.truncate(10);
            }
            return 0;
        }
        "pushd" => {
            let current = env::current_dir().unwrap_or_default().to_string_lossy().to_string();
            let dir = if parts.len() > 1 {
                expand_tilde(&parts[1])
            } else {
                // With no args, swap top of stack with cwd
                if let Some(top) = state.dir_stack.last().cloned() {
                    top
                } else {
                    eprintln!("pushd: no other directory");
                    return 1;
                }
            };
            if let Err(e) = env::set_current_dir(&dir) {
                eprintln!("pushd: {}: {}", dir, e);
                return 1;
            }
            state.dir_stack.push(current);
            let cwd = env::current_dir().unwrap_or_default().to_string_lossy().to_string();
            println!("{}", cwd);
            return 0;
        }
        "popd" => {
            if let Some(dir) = state.dir_stack.pop() {
                if let Err(e) = env::set_current_dir(&dir) {
                    eprintln!("popd: {}: {}", dir, e);
                    state.dir_stack.push(dir);
                    return 1;
                }
                println!("{}", dir);
            } else {
                eprintln!("popd: directory stack empty");
                return 1;
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
        // Bare `fzf` (also reached via the `f` nick): fuzzy-find then cd into selection
        "fzf" if parts.len() == 1 => {
            return handle_fzf();
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
    if let Some(bm) = config.bookmarks.get(parts[0].as_str()) {
        let path = bm.path.clone();
        if Path::new(&path).is_dir() {
            let _ = env::set_current_dir(&path);
            return 0;
        }
    }

    // File auto-open: if the "command" is a file (not executable), open it
    let file_path = Path::new(&expanded);
    if file_path.is_file() && parts.len() == 1 {
        // Check if it's executable
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = file_path.metadata() {
                let mode = meta.permissions().mode();
                if mode & 0o111 == 0 {
                    // Not executable; check MIME type
                    let mime = Command::new("file")
                        .args(["--mime-type", "-b", &expanded])
                        .output()
                        .ok()
                        .and_then(|o| String::from_utf8(o.stdout).ok())
                        .unwrap_or_default();
                    let mime = mime.trim();
                    if mime.starts_with("text/") || mime.contains("json") || mime.contains("xml") {
                        let editor = env::var("EDITOR").unwrap_or_else(|_| "vim".to_string());
                        return Command::new(&editor).arg(&expanded).status()
                            .map(|s| s.code().unwrap_or(1)).unwrap_or(127);
                    } else {
                        return Command::new("xdg-open")
                            .arg(&expanded)
                            .stdout(std::process::Stdio::null())
                            .stderr(std::process::Stdio::null())
                            .spawn()
                            .map(|_| 0).unwrap_or(127);
                    }
                }
            }
        }
    }

    // Track frequency
    *state.cmd_frequency.entry(parts[0].clone()).or_insert(0) += 1;

    // Inline env var assignment (VAR=val command): delegate to shell
    if let Some(eq_pos) = parts[0].find('=') {
        if eq_pos > 0 && parts[0][..eq_pos].chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && parts.len() > 1 {
            return run_via_shell(&line, jobs);
        }
    }

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
    let code = run_command(cmd_line, background, jobs);

    // Command not found: try package suggestion, then Levenshtein
    if code == 127 && !parts[0].contains('/') {
        let mut pkg_suggested = false;
        // Try command-not-found or pkgfile for package suggestions
        let helpers: Vec<&str> = vec![
            "/usr/lib/command-not-found",
            "command-not-found",
            "pkgfile",
        ];
        for helper in &helpers {
            let exists = if helper.starts_with('/') {
                Path::new(helper).exists()
            } else {
                Command::new("which").arg(helper).output()
                    .map(|o| o.status.success()).unwrap_or(false)
            };
            if exists {
                if let Ok(output) = Command::new(helper).arg(&parts[0]).output() {
                    let out = String::from_utf8_lossy(&output.stdout);
                    let err = String::from_utf8_lossy(&output.stderr);
                    let combined = format!("{}{}", out, err);
                    let trimmed = combined.trim();
                    if !trimmed.is_empty() {
                        eprintln!("{}", trimmed);
                        pkg_suggested = true;
                    }
                }
                break;
            }
        }
        // Fall back to Levenshtein suggestions
        if !pkg_suggested && config.auto_correct {
            let suggestions = find_similar_commands(&parts[0], exe_cache, 3);
            if !suggestions.is_empty() {
                eprintln!("Did you mean:");
                for (i, s) in suggestions.iter().enumerate() {
                    eprintln!("  {} {}", i + 1, s);
                }
            }
        }
    }

    code
}

fn find_similar_commands(cmd: &str, exe_cache: &[String], max: usize) -> Vec<String> {
    let mut scored: Vec<(usize, &String)> = exe_cache.iter()
        .map(|e| (levenshtein(cmd, e), e))
        .filter(|(d, _)| *d <= 3)
        .collect();
    scored.sort_by_key(|(d, _)| *d);
    scored.truncate(max);
    scored.into_iter().map(|(_, s)| s.clone()).collect()
}

/// Clean up completed background jobs
pub fn cleanup_jobs(jobs: &mut HashMap<u32, Job>) {
    let mut done = Vec::new();
    for (&id, job) in jobs.iter() {
        match waitpid(Pid::from_raw(job.pid), Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::Exited(_, _)) | Ok(WaitStatus::Signaled(_, _, _)) => {
                done.push(id);
            }
            _ => {}
        }
    }
    for id in done {
        if let Some(job) = jobs.remove(&id) {
            eprintln!("[{}] Done: {}", id, job.cmd);
        }
    }
}

fn run_via_shell(line: &str, _jobs: &mut HashMap<u32, Job>) -> i32 {
    // Run the command with inherited stdio (real-time output),
    // then query PIPESTATUS from a separate bash invocation trick.
    // We use bash -c with PIPESTATUS capture via a temp file.
    let tmpfile = format!("/tmp/rush_pipestatus_{}", std::process::id());
    let wrapped = format!(
        "{}; echo ${{PIPESTATUS[*]}} > {}",
        line, tmpfile
    );

    use std::os::unix::process::CommandExt;
    let mut bash_cmd = Command::new("bash");
    bash_cmd.arg("-c").arg(&wrapped);
    unsafe {
        bash_cmd.pre_exec(|| {
            libc::signal(libc::SIGTSTP, libc::SIG_DFL);
            libc::signal(libc::SIGINT, libc::SIG_DFL);
            libc::signal(libc::SIGQUIT, libc::SIG_DFL);
            Ok(())
        });
    }
    let status = bash_cmd.status();

    // Read PIPESTATUS from temp file
    if let Ok(ps) = std::fs::read_to_string(&tmpfile) {
        let ps = ps.trim().to_string();
        if !ps.is_empty() {
            env::set_var("PIPESTATUS", &ps);
        }
        let _ = std::fs::remove_file(&tmpfile);
    }

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
                let id = (jobs.keys().max().copied().unwrap_or(0)) + 1;
                let pid = child.id() as i32;
                println!("[{}] {}", id, pid);
                jobs.insert(id, Job { pid, cmd: line.to_string(), status: JobStatus::Running });
                0
            }
            Err(e) => {
                eprintln!("rush: {}: {}", cmd, e);
                127
            }
        }
    } else {
        use std::os::unix::process::CommandExt;
        let mut child_cmd = Command::new(cmd);
        child_cmd.args(&args);
        unsafe {
            child_cmd.pre_exec(|| {
                // Restore default signal handlers for child
                libc::signal(libc::SIGTSTP, libc::SIG_DFL);
                libc::signal(libc::SIGINT, libc::SIG_DFL);
                libc::signal(libc::SIGQUIT, libc::SIG_DFL);
                libc::signal(libc::SIGTTIN, libc::SIG_DFL);
                libc::signal(libc::SIGTTOU, libc::SIG_DFL);
                Ok(())
            });
        }
        match child_cmd.status() {
            Ok(s) => s.code().unwrap_or(1),
            Err(_e) => {
                eprintln!("rush: {}: command not found", cmd);
                127
            }
        }
    }
}

/// Handle fzf integration: run fzf and cd to selected directory
fn handle_fzf() -> i32 {
    // Check if fzf is in PATH
    if Command::new("which").arg("fzf").output()
        .map(|o| o.status.success()).unwrap_or(false)
    {
        let output = Command::new("fzf").output();
        match output {
            Ok(o) if o.status.success() => {
                let selected = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if !selected.is_empty() {
                    let path = Path::new(&selected);
                    let target = if path.is_dir() {
                        selected.clone()
                    } else if let Some(parent) = path.parent() {
                        parent.to_string_lossy().to_string()
                    } else {
                        return 0;
                    };
                    if let Err(e) = env::set_current_dir(&target) {
                        eprintln!("cd: {}: {}", target, e);
                        return 1;
                    }
                }
                0
            }
            _ => 1,
        }
    } else {
        eprintln!("rush: fzf not found in PATH");
        1
    }
}

/// Handle AI integration (@ and @@)
fn handle_ai_command(prompt: &str, suggest_cmd: bool) -> i32 {
    if prompt.is_empty() {
        eprintln!("Usage: @ <prompt> or @@ <prompt>");
        return 1;
    }

    // Read API key
    let key_path = "/home/.safe/openai.txt";
    let api_key = match std::fs::read_to_string(key_path) {
        Ok(k) => k.trim().to_string(),
        Err(_) => {
            eprintln!("rush: cannot read API key from {}", key_path);
            return 1;
        }
    };

    let system_msg = if suggest_cmd {
        "You are a shell command assistant. Given the user's request, suggest a single shell command. Output ONLY the command, nothing else."
    } else {
        "You are a helpful assistant. Be concise."
    };

    let body = serde_json::json!({
        "model": "gpt-4o-mini",
        "messages": [
            {"role": "system", "content": system_msg},
            {"role": "user", "content": prompt}
        ],
        "temperature": 0.3
    });

    let output = Command::new("curl")
        .args([
            "-s",
            "https://api.openai.com/v1/chat/completions",
            "-H", "Content-Type: application/json",
            "-H", &format!("Authorization: Bearer {}", api_key),
            "-d", &body.to_string(),
        ])
        .output();

    match output {
        Ok(o) => {
            let response = String::from_utf8_lossy(&o.stdout);
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&response) {
                if let Some(content) = val["choices"][0]["message"]["content"].as_str() {
                    println!("{}", content.trim());
                    return 0;
                }
                if let Some(err) = val["error"]["message"].as_str() {
                    eprintln!("AI error: {}", err);
                    return 1;
                }
            }
            eprintln!("rush: unexpected AI response");
            1
        }
        Err(e) => {
            eprintln!("rush: curl failed: {}", e);
            1
        }
    }
}

fn handle_colon_command(
    line: &str,
    config: &mut Config,
    state: &mut State,
    jobs: &mut HashMap<u32, Job>,
    recording: &mut Option<Recording>,
    plugins: &mut crate::plugin::PluginManager,
) -> i32 {
    let line = &line[1..]; // strip ':'
    let parts: Vec<&str> = line.splitn(2, ' ').collect();
    let cmd = parts[0];
    let args = if parts.len() > 1 { parts[1].trim() } else { "" };

    match cmd {
        "nick" => {
            // Strip surrounding quotes if present (e.g. :nick "r = rtfm")
            let args = args.trim_matches('"').trim_matches('\'');
            if args.is_empty() {
                for (k, v) in &config.nick {
                    println!("  {} = {}", k, v);
                }
            } else if args.starts_with("--export") {
                let file = args.strip_prefix("--export").unwrap().trim();
                let file = if file.is_empty() { "nicks.json" } else { file };
                if let Ok(data) = serde_json::to_string_pretty(&config.nick) {
                    if let Err(e) = std::fs::write(file, data) {
                        eprintln!("Error: {}", e);
                    } else {
                        println!("Exported {} nicks to {}", config.nick.len(), file);
                    }
                }
            } else if args.starts_with("--import") {
                let file = args.strip_prefix("--import").unwrap().trim();
                let file = if file.is_empty() { "nicks.json" } else { file };
                match std::fs::read_to_string(file) {
                    Ok(data) => match serde_json::from_str::<HashMap<String, String>>(&data) {
                        Ok(nicks) => {
                            let count = nicks.len();
                            config.nick.extend(nicks);
                            config.save();
                            println!("Imported {} nicks from {}", count, file);
                        }
                        Err(e) => eprintln!("Parse error: {}", e),
                    },
                    Err(e) => eprintln!("Read error: {}", e),
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
            let args = args.trim_matches('"').trim_matches('\'');
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
                for (k, bm) in &config.bookmarks {
                    if bm.tags.is_empty() {
                        println!("  {} -> {}", k, bm.path);
                    } else {
                        println!("  {} -> {} [{}]", k, bm.path, bm.tags.join(", "));
                    }
                }
            } else if args.starts_with("--export") {
                let file = args.strip_prefix("--export").unwrap().trim();
                let file = if file.is_empty() { "bookmarks.json" } else { file };
                if let Ok(data) = serde_json::to_string_pretty(&config.bookmarks) {
                    if let Err(e) = std::fs::write(file, data) {
                        eprintln!("Error: {}", e);
                    } else {
                        println!("Exported {} bookmarks to {}", config.bookmarks.len(), file);
                    }
                }
            } else if args.starts_with("--import") {
                let file = args.strip_prefix("--import").unwrap().trim();
                let file = if file.is_empty() { "bookmarks.json" } else { file };
                match std::fs::read_to_string(file) {
                    Ok(data) => match serde_json::from_str::<HashMap<String, crate::config::Bookmark>>(&data) {
                        Ok(bms) => {
                            let count = bms.len();
                            config.bookmarks.extend(bms);
                            config.save();
                            println!("Imported {} bookmarks from {}", count, file);
                        }
                        Err(e) => eprintln!("Parse error: {}", e),
                    },
                    Err(e) => eprintln!("Read error: {}", e),
                }
            } else if args.starts_with('-') {
                config.bookmarks.remove(&args[1..]);
                config.save();
            } else if args.starts_with('?') {
                // Search by tag
                let search_tag = &args[1..].trim();
                let mut found = false;
                for (k, bm) in &config.bookmarks {
                    if bm.tags.iter().any(|t| t == search_tag) {
                        println!("  {} -> {} [{}]", k, bm.path, bm.tags.join(", "));
                        found = true;
                    }
                }
                if !found {
                    println!("No bookmarks with tag '{}'", search_tag);
                }
            } else {
                let bm_parts: Vec<&str> = args.splitn(3, ' ').collect();
                let name = bm_parts[0];

                // Check if there's a #tags part
                let mut path_str = String::new();
                let mut tags = Vec::new();

                if bm_parts.len() > 1 {
                    // Parse path and optional tags
                    let rest = if bm_parts.len() == 3 {
                        format!("{} {}", bm_parts[1], bm_parts[2])
                    } else {
                        bm_parts[1].to_string()
                    };

                    if let Some(hash_pos) = rest.find('#') {
                        path_str = rest[..hash_pos].trim().to_string();
                        let tag_str = &rest[hash_pos + 1..];
                        tags = tag_str.split(',')
                            .map(|t| t.trim().to_string())
                            .filter(|t| !t.is_empty())
                            .collect();
                    } else {
                        path_str = rest.trim().to_string();
                    }
                }

                if path_str.is_empty() {
                    path_str = env::current_dir().unwrap_or_default().to_string_lossy().to_string();
                }

                config.bookmarks.insert(name.to_string(), Bookmark {
                    path: path_str,
                    tags,
                });
                config.save();
            }
            0
        }
        "dirs" => {
            if args == "-v" {
                // Show directory stack (pushd/popd)
                let cwd = env::current_dir().unwrap_or_default().to_string_lossy().to_string();
                println!("  0 {}", cwd);
                for (i, d) in state.dir_stack.iter().rev().enumerate() {
                    println!("  {} {}", i + 1, d);
                }
            } else {
                // Show cd history
                for (i, d) in state.dirs.iter().enumerate() {
                    println!("  {} {}", i, d);
                }
            }
            0
        }
        "abbrev" => {
            if args.is_empty() {
                if config.abbrev.is_empty() {
                    println!("No abbreviations");
                } else {
                    for (k, v) in &config.abbrev {
                        println!("  {} = {}", k, v);
                    }
                }
            } else if args.starts_with('-') {
                let name = &args[1..];
                config.abbrev.remove(name);
                config.save();
                println!("Abbreviation '{}' removed", name);
            } else if let Some((name, val)) = args.split_once('=') {
                config.abbrev.insert(name.trim().to_string(), val.trim().to_string());
                config.save();
                println!("Abbreviation '{}' set", name.trim());
            } else {
                eprintln!("Usage: :abbrev [name = expansion | -name]");
                return 1;
            }
            0
        }
        "history" => {
            let count = args.parse::<usize>().unwrap_or(50);
            let start = state.history.len().saturating_sub(count);
            for (i, cmd) in state.history[start..].iter().enumerate() {
                let idx = start + i;
                if idx < state.history_times.len() && state.history_times[idx] > 0 {
                    let ts = state.history_times[idx];
                    let offset: i64 = unsafe {
                        let mut tm: libc::tm = std::mem::zeroed();
                        let secs = ts as i64;
                        libc::localtime_r(&secs, &mut tm);
                        tm.tm_gmtoff
                    };
                    let local = (ts as i64 + offset) as u64;
                    let h = (local % 86400) / 3600;
                    let m = (local % 3600) / 60;
                    let s = local % 60;
                    // Also compute date
                    let _days = local / 86400;
                    // Approximate date from epoch days (good enough for display)
                    println!("  {:4} [{:02}:{:02}:{:02}] {}", idx, h, m, s, cmd);
                } else {
                    println!("  {:4} {}", idx, cmd);
                }
            }
            0
        }
        "rmhistory" => {
            state.history.clear();
            state.history_times.clear();
            state.save();
            println!("History cleared");
            0
        }
        "rehash" => {
            state.exe_cache = build_exe_cache();
            state.exe_cache_time = now_secs();
            println!("Executable cache rebuilt: {} commands", state.exe_cache.len());
            0
        }
        "reload" => {
            *config = Config::load();
            println!("Config reloaded from ~/.rushrc.json");
            0
        }
        "theme" => {
            if args.is_empty() {
                println!("Available themes: {}", config::theme_names().join(", "));
            } else if let Some(theme) = config::get_theme(args) {
                config::apply_theme(config, &theme);
                config.save();
                println!("Theme '{}' applied", args);
            } else {
                eprintln!("Unknown theme '{}'. Available: {}", args, config::theme_names().join(", "));
                return 1;
            }
            0
        }
        "calc" => {
            if args.is_empty() {
                eprintln!("Usage: :calc <expression>");
                return 1;
            }
            match calc_eval(args) {
                Ok(val) => {
                    if val.fract() == 0.0 && val.abs() < 1e15 {
                        println!("{}", val as i64);
                    } else {
                        println!("{}", val);
                    }
                    0
                }
                Err(e) => {
                    eprintln!("calc error: {}", e);
                    1
                }
            }
        }
        "stats" => {
            let mut entries: Vec<(&String, &usize)> = state.cmd_frequency.iter().collect();
            entries.sort_by(|a, b| b.1.cmp(a.1));
            entries.truncate(20);
            println!("{}", style::bold(&format!("  {:>6}  Command", "Count")));
            println!("  {:->6}  {:-<30}", "", "");
            for (cmd, count) in entries {
                println!("  {:>6}  {}", count, cmd);
            }
            0
        }
        "jobs" => {
            if jobs.is_empty() {
                println!("No background jobs");
            } else {
                for (id, job) in jobs.iter() {
                    println!("  [{}] {} PID {} : {}", id, job.status, job.pid, job.cmd);
                }
            }
            0
        }
        "fg" => {
            if args.is_empty() {
                eprintln!("Usage: :fg <job_number>");
                return 1;
            }
            let n: u32 = match args.parse() {
                Ok(n) => n,
                Err(_) => { eprintln!("Invalid job number"); return 1; }
            };
            if let Some(job) = jobs.remove(&n) {
                println!("Bringing to foreground: {}", job.cmd);
                // Send SIGCONT in case the job was stopped
                unsafe { libc::kill(job.pid, libc::SIGCONT); }
                match waitpid(Pid::from_raw(job.pid), None) {
                    Ok(WaitStatus::Exited(_, code)) => code,
                    Ok(WaitStatus::Stopped(_, _)) => {
                        // Re-stopped via Ctrl-Z; put back as stopped
                        let id = n;
                        eprintln!("\n[{}] Stopped  {}", id, job.cmd);
                        jobs.insert(id, Job { pid: job.pid, cmd: job.cmd, status: JobStatus::Stopped });
                        0
                    }
                    _ => 1,
                }
            } else {
                eprintln!("No such job: {}", n);
                1
            }
        }
        // Session management
        "save_session" => {
            if args.is_empty() {
                eprintln!("Usage: :save_session <name>");
                return 1;
            }
            let sessions_dir = dirs::home_dir().unwrap_or_default().join(".rush/sessions");
            let _ = std::fs::create_dir_all(&sessions_dir);
            let session = serde_json::json!({
                "pwd": env::current_dir().unwrap_or_default().to_string_lossy().to_string(),
                "history": state.history,
                "bookmarks": config.bookmarks,
                "nick": config.nick,
            });
            let path = sessions_dir.join(format!("{}.json", args));
            match std::fs::write(&path, serde_json::to_string_pretty(&session).unwrap_or_default()) {
                Ok(_) => { println!("Session '{}' saved", args); 0 }
                Err(e) => { eprintln!("Failed to save session: {}", e); 1 }
            }
        }
        "load_session" => {
            if args.is_empty() {
                eprintln!("Usage: :load_session <name>");
                return 1;
            }
            let path = dirs::home_dir().unwrap_or_default()
                .join(".rush/sessions")
                .join(format!("{}.json", args));
            match std::fs::read_to_string(&path) {
                Ok(data) => {
                    if let Ok(val) = serde_json::from_str::<serde_json::Value>(&data) {
                        if let Some(pwd) = val["pwd"].as_str() {
                            let _ = env::set_current_dir(pwd);
                        }
                        if let Some(hist) = val["history"].as_array() {
                            state.history = hist.iter()
                                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                                .collect();
                        }
                        if let Ok(bm) = serde_json::from_value(val["bookmarks"].clone()) {
                            config.bookmarks = bm;
                        }
                        if let Ok(nk) = serde_json::from_value(val["nick"].clone()) {
                            config.nick = nk;
                        }
                        println!("Session '{}' loaded", args);
                        config.save();
                        state.save();
                        0
                    } else {
                        eprintln!("Failed to parse session file");
                        1
                    }
                }
                Err(_) => {
                    eprintln!("Session '{}' not found", args);
                    1
                }
            }
        }
        "list_sessions" => {
            let sessions_dir = dirs::home_dir().unwrap_or_default().join(".rush/sessions");
            if sessions_dir.exists() {
                if let Ok(entries) = std::fs::read_dir(&sessions_dir) {
                    let mut found = false;
                    for entry in entries.flatten() {
                        if let Some(name) = entry.file_name().to_str() {
                            if name.ends_with(".json") {
                                println!("  {}", &name[..name.len() - 5]);
                                found = true;
                            }
                        }
                    }
                    if !found {
                        println!("No saved sessions");
                    }
                }
            } else {
                println!("No saved sessions");
            }
            0
        }
        "delete_session" => {
            if args.is_empty() {
                eprintln!("Usage: :delete_session <name>");
                return 1;
            }
            let path = dirs::home_dir().unwrap_or_default()
                .join(".rush/sessions")
                .join(format!("{}.json", args));
            if path.exists() {
                let _ = std::fs::remove_file(&path);
                println!("Session '{}' deleted", args);
                0
            } else {
                eprintln!("Session '{}' not found", args);
                1
            }
        }
        // Recording & Replay
        "record" => {
            let rec_parts: Vec<&str> = args.splitn(2, ' ').collect();
            let sub = if rec_parts.is_empty() { "" } else { rec_parts[0] };
            let rec_args = if rec_parts.len() > 1 { rec_parts[1].trim() } else { "" };
            match sub {
                "start" => {
                    if rec_args.is_empty() {
                        eprintln!("Usage: :record start <name>");
                        return 1;
                    }
                    *recording = Some(Recording {
                        name: rec_args.to_string(),
                        commands: Vec::new(),
                    });
                    println!("Recording started: {}", rec_args);
                    0
                }
                "stop" => {
                    if let Some(rec) = recording.take() {
                        let count = rec.commands.len();
                        state.recordings.insert(rec.name.clone(), rec.commands);
                        state.save();
                        println!("Recording '{}' stopped ({} commands)", rec.name, count);
                    } else {
                        eprintln!("No active recording");
                    }
                    0
                }
                "show" => {
                    if rec_args.is_empty() {
                        // List all recordings
                        if state.recordings.is_empty() {
                            println!("No recordings");
                        } else {
                            for (name, cmds) in &state.recordings {
                                println!("  {} ({} commands)", name, cmds.len());
                            }
                        }
                    } else if let Some(cmds) = state.recordings.get(rec_args) {
                        for (i, c) in cmds.iter().enumerate() {
                            println!("  {:3} {}", i + 1, c);
                        }
                    } else {
                        eprintln!("Recording '{}' not found", rec_args);
                        return 1;
                    }
                    0
                }
                _ => {
                    eprintln!("Usage: :record start|stop|show [name]");
                    1
                }
            }
        }
        "replay" => {
            if args.is_empty() {
                eprintln!("Usage: :replay <name>");
                return 1;
            }
            if let Some(cmds) = state.recordings.get(args).cloned() {
                println!("Replaying '{}' ({} commands)", args, cmds.len());
                let mut last_code = 0;
                for c in &cmds {
                    println!("$ {}", c);
                    last_code = execute(c, config, state, &[], jobs, recording, plugins);
                }
                last_code
            } else {
                eprintln!("Recording '{}' not found", args);
                1
            }
        }
        // Validation rules
        "validate" => {
            if args.is_empty() {
                if config.validation_rules.is_empty() {
                    println!("No validation rules");
                } else {
                    for (pattern, action) in &config.validation_rules {
                        println!("  {} = {}", pattern, action);
                    }
                }
            } else if args.starts_with('-') {
                let pattern = &args[1..];
                config.validation_rules.remove(pattern);
                config.save();
                println!("Validation rule removed: {}", pattern);
            } else if let Some((pattern, action)) = args.split_once('=') {
                let pattern = pattern.trim().to_string();
                let action = action.trim().to_string();
                if action != "block" && action != "confirm" && action != "warn" {
                    eprintln!("Action must be: block, confirm, or warn");
                    return 1;
                }
                config.validation_rules.insert(pattern, action);
                config.save();
                println!("Validation rule added");
            } else {
                eprintln!("Usage: :validate [pattern = action | -pattern]");
                eprintln!("  Actions: block, confirm, warn");
                return 1;
            }
            0
        }
        // Environment variables
        "env" => {
            let env_parts: Vec<&str> = args.splitn(3, ' ').collect();
            if args.is_empty() {
                // List all
                let mut vars: Vec<(String, String)> = env::vars().collect();
                vars.sort();
                for (k, v) in &vars {
                    println!("  {}={}", k, v);
                }
            } else if env_parts[0] == "set" && env_parts.len() >= 3 {
                env::set_var(env_parts[1], env_parts[2]);
                println!("{}={}", env_parts[1], env_parts[2]);
            } else if env_parts[0] == "unset" && env_parts.len() >= 2 {
                env::remove_var(env_parts[1]);
                println!("Unset {}", env_parts[1]);
            } else {
                // Show specific variable
                let var_name = args.trim();
                match env::var(var_name) {
                    Ok(val) => println!("  {}={}", var_name, val),
                    Err(_) => eprintln!("  {} not set", var_name),
                }
            }
            0
        }
        // Config command
        "config" => {
            if args.is_empty() {
                println!("  history_dedup = {}", config.history_dedup);
                println!("  auto_correct = {}", config.auto_correct);
                println!("  completion_fuzzy = {}", config.completion_fuzzy);
                println!("  completion_case_sensitive = {}", config.completion_case_sensitive);
                println!("  completion_limit = {}", config.completion_limit);
                println!("  show_tips = {}", config.show_tips);
                println!("  rprompt = {}", config.rprompt);
                println!("  auto_pair = {}", config.auto_pair);
                println!("  c_prompt = {}", config.c_prompt);
                println!("  c_cmd = {}", config.c_cmd);
                println!("  c_nick = {}", config.c_nick);
                println!("  c_gnick = {}", config.c_gnick);
                println!("  c_path = {}", config.c_path);
                println!("  c_switch = {}", config.c_switch);
                println!("  c_bookmark = {}", config.c_bookmark);
                println!("  c_colon = {}", config.c_colon);
                println!("  c_tabselect = {}", config.c_tabselect);
                println!("  c_taboption = {}", config.c_taboption);
                println!("  c_dir = {}", config.c_dir);
                println!("  c_exec = {}", config.c_exec);
                println!("  c_file = {}", config.c_file);
                println!("  c_suggestion = {}", config.c_suggestion);
            } else {
                let cfg_parts: Vec<&str> = args.splitn(2, ' ').collect();
                if cfg_parts.len() < 2 {
                    eprintln!("Usage: :config <key> <value>");
                    return 1;
                }
                let key = cfg_parts[0];
                let val = cfg_parts[1].trim();
                match key {
                    "history_dedup" => {
                        if val == "off" || val == "full" || val == "smart" {
                            config.history_dedup = val.to_string();
                        } else {
                            eprintln!("Valid values: off, full, smart");
                            return 1;
                        }
                    }
                    "auto_correct" => { config.auto_correct = val == "true"; }
                    "completion_fuzzy" => { config.completion_fuzzy = val == "true"; }
                    "completion_case_sensitive" => { config.completion_case_sensitive = val == "true"; }
                    "completion_limit" => {
                        if let Ok(n) = val.parse::<usize>() {
                            config.completion_limit = n;
                        } else {
                            eprintln!("Invalid number");
                            return 1;
                        }
                    }
                    "show_tips" => { config.show_tips = val == "true"; }
                    "rprompt" => { config.rprompt = val == "true"; }
                    "auto_pair" => { config.auto_pair = val == "true"; }
                    "c_prompt" | "c_cmd" | "c_nick" | "c_gnick" | "c_path" |
                    "c_switch" | "c_bookmark" | "c_colon" | "c_tabselect" |
                    "c_taboption" | "c_dir" | "c_exec" | "c_file" | "c_suggestion" => {
                        if let Ok(n) = val.parse::<u8>() {
                            match key {
                                "c_prompt" => config.c_prompt = n,
                                "c_cmd" => config.c_cmd = n,
                                "c_nick" => config.c_nick = n,
                                "c_gnick" => config.c_gnick = n,
                                "c_path" => config.c_path = n,
                                "c_switch" => config.c_switch = n,
                                "c_bookmark" => config.c_bookmark = n,
                                "c_colon" => config.c_colon = n,
                                "c_tabselect" => config.c_tabselect = n,
                                "c_taboption" => config.c_taboption = n,
                                "c_dir" => config.c_dir = n,
                                "c_exec" => config.c_exec = n,
                                "c_file" => config.c_file = n,
                                "c_suggestion" => config.c_suggestion = n,
                                _ => {}
                            }
                        } else {
                            eprintln!("Invalid color value (0-255)");
                            return 1;
                        }
                    }
                    _ => {
                        eprintln!("Unknown config key: {}", key);
                        return 1;
                    }
                }
                config.save();
                println!("  {} = {}", key, val);
            }
            0
        }
        // Version and info
        "version" => {
            println!("rush 0.1.0");
            0
        }
        "info" => {
            println!("{} - a fast terminal shell written in Rust", style::bold("rush"));
            println!();
            println!("Features:");
            println!("  - Nick aliases and global nicks (parametrized)");
            println!("  - Bookmarks with tags");
            println!("  - Tab completion (commands, files, smart subcommands)");
            println!("  - History suggestions (right arrow to accept)");
            println!("  - History search (Shift-Tab)");
            println!("  - Syntax highlighting");
            println!("  - Color themes (default, solarized, dracula, gruvbox, nord, monokai)");
            println!("  - Built-in calculator");
            println!("  - xrpn RPN calculator integration");
            println!("  - Background jobs");
            println!("  - File auto-open");
            println!("  - History expansion (!!, !N, !-N)");
            println!("  - Sessions (save/load shell state)");
            println!("  - Command recording and replay");
            println!("  - Validation rules (block/confirm/warn)");
            println!("  - Environment variable management");
            println!("  - AI integration (@ prompt, @@ for commands)");
            println!("  - fzf integration (type 'f' to fuzzy find)");
            println!("  - Completion learning (most-used completions ranked higher)");
            println!("  - Edit line in $EDITOR (Ctrl-G)");
            0
        }
        "help" => {
            println!("{}", style::bold("rush commands:"));
            println!("  :nick [name = val | -name]       Aliases");
            println!("  :gnick [name = val | -name]      Global aliases");
            println!("  :bm [name [path] [#tags] | -name | ?tag]  Bookmarks");
            println!("  :dirs                             Directory history");
            println!("  :history [n]                      Command history");
            println!("  :rehash                           Rebuild command cache");
            println!("  :reload                           Reload config from ~/.rushrc.json");
            println!("  :theme [name]                     Set color theme");
            println!("  :calc <expr>                      Calculator (+,-,*,/,%,**,sqrt,sin,cos,tan,log)");
            println!("  = <expr>                          xrpn RPN calculator");
            println!("  :stats                            Top 20 most-used commands");
            println!("  :jobs                             List background jobs");
            println!("  :fg <n>                           Bring job to foreground");
            println!("  :env [VAR | set VAR val | unset VAR]  Environment variables");
            println!("  :config [key value]               View/change settings");
            println!("  :validate [pattern = action | -pattern]  Validation rules");
            println!("  :save_session <name>              Save session state");
            println!("  :load_session <name>              Load session state");
            println!("  :list_sessions                    List saved sessions");
            println!("  :delete_session <name>            Delete a session");
            println!("  :record start|stop|show [name]    Record commands");
            println!("  :replay <name>                    Replay recorded commands");
            println!("  :abbrev [name = val | -name]      Fish-style abbreviations");
            println!("  :version                          Show version");
            println!("  :info                             Show feature overview");
            println!("  :help                             This help");
            println!();
            println!("{}", style::bold("History expansion:"));
            println!("  !!                                Last command");
            println!("  !N                                Command number N");
            println!("  !-N                               Nth previous command");
            println!();
            println!("{}", style::bold("AI integration:"));
            println!("  @ <prompt>                        Ask AI a question");
            println!("  @@ <prompt>                       Ask AI for a shell command");
            println!();
            println!("{}", style::bold("Special:"));
            println!("  f                                 Fuzzy find with fzf");
            println!("  r                                 Launch file manager");
            println!("  cd N                              Jump to Nth dir from :dirs");
            println!("  pushd [dir]                       Push dir to stack and cd");
            println!("  popd                              Pop dir from stack and cd");
            println!("  :dirs -v                          Show directory stack");
            println!();
            println!("{}", style::bold("Keys:"));
            println!("  Tab                               Completion (interactive cycling)");
            println!("  Shift-Tab                         History search");
            println!("  Ctrl-R                            Reverse incremental search");
            println!("  Ctrl-G                            Edit line in $EDITOR");
            println!("  Ctrl-Y                            Copy line to clipboard");
            println!("  Ctrl-Z                            Suspend foreground process");
            println!("  Ctrl-_ / Ctrl-Z (in edit)        Undo last edit");
            println!("  Right arrow                       Accept history suggestion");
            println!("  Space                             Expand abbreviations");
            println!();
            println!("{}", style::bold("Migration:"));
            println!("  :import_rsh                       Import nicks/bookmarks from ~/.rshrc");
            0
        }
        "import_rsh" => {
            import_rshrc(config);
            0
        }
        "plugins" => {
            if args.is_empty() {
                plugins.list();
            } else if args.starts_with("enable ") {
                let name = args.strip_prefix("enable ").unwrap().trim();
                if plugins.enable(name) {
                    println!("Enabled plugin: {}", name);
                } else {
                    println!("Plugin not found: {}", name);
                }
            } else if args.starts_with("disable ") {
                let name = args.strip_prefix("disable ").unwrap().trim();
                if plugins.disable(name) {
                    println!("Disabled plugin: {}", name);
                } else {
                    println!("Plugin not found: {}", name);
                }
            } else if args == "reload" {
                plugins.plugins.clear();
                plugins.load_all();
                println!("Reloaded {} plugins", plugins.plugins.len());
            } else {
                // Try as a plugin command
                if let Some(resp) = plugins.run_command(args, "") {
                    if !resp.output.is_empty() { println!("{}", resp.output); }
                } else {
                    println!("Usage: :plugins [enable|disable|reload] [name]");
                }
            }
            0
        }
        _ => {
            eprintln!("rush: unknown command :{}", cmd);
            1
        }
    }
}

fn import_rshrc(config: &mut Config) {
    let path = dirs::home_dir().unwrap_or_default().join(".rshrc");
    if !path.exists() {
        println!("No ~/.rshrc found");
        return;
    }
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => { println!("Error reading .rshrc: {}", e); return; }
    };
    let mut nicks = 0;
    let mut gnicks = 0;
    let mut bmarks = 0;
    // Parse :nick "name = value" patterns
    let nick_re = regex::Regex::new(r#"@nick\["([^"]+)"\]\s*=\s*"([^"]+)""#).unwrap();
    for cap in nick_re.captures_iter(&content) {
        config.nick.insert(cap[1].to_string(), cap[2].to_string());
        nicks += 1;
    }
    // Parse @gnick
    let gnick_re = regex::Regex::new(r#"@gnick\["([^"]+)"\]\s*=\s*"([^"]+)""#).unwrap();
    for cap in gnick_re.captures_iter(&content) {
        config.gnick.insert(cap[1].to_string(), cap[2].to_string());
        gnicks += 1;
    }
    // Parse @bookmarks
    let bm_re = regex::Regex::new(r#"@bookmarks\["([^"]+)"\]\s*=\s*"([^"]+)""#).unwrap();
    for cap in bm_re.captures_iter(&content) {
        config.bookmarks.insert(cap[1].to_string(), crate::config::Bookmark {
            path: cap[2].to_string(),
            tags: vec![],
        });
        bmarks += 1;
    }
    config.save();
    println!("Imported from .rshrc: {} nicks, {} gnicks, {} bookmarks", nicks, gnicks, bmarks);
}

pub fn expand_nicks(line: &str, nicks: &HashMap<String, String>, gnicks: &HashMap<String, String>) -> String {
    let mut result = line.to_string();

    // Apply gnicks (global, anywhere in line), max 3 passes to prevent loops
    for _ in 0..3 {
        let before = result.clone();
        for (k, v) in gnicks {
            result = result.replace(k.as_str(), v.as_str());
        }
        if result == before { break; }
    }

    // Apply nicks (only at command position), guard against recursion
    let parts: Vec<&str> = result.splitn(2, ' ').collect();
    if let Some(expanded) = nicks.get(parts[0]) {
        // Don't expand if nick expands to itself (e.g., "ls" -> "ls --color")
        let expanded_cmd = expanded.split_whitespace().next().unwrap_or("");
        if expanded_cmd == parts[0] {
            // Just prepend the extra args, don't recurse
            if parts.len() > 1 {
                return format!("{} {}", expanded, parts[1]);
            } else {
                return expanded.clone();
            }
        }
        let mut nick_val = expanded.clone();

        // Parametrized nicks: replace {{key}} with key=value from arguments
        if nick_val.contains("{{") {
            if let Some(args_str) = parts.get(1) {
                let args_parts = shell_split(args_str);
                let mut params: HashMap<String, String> = HashMap::new();
                let mut positional = Vec::new();
                for arg in &args_parts {
                    if let Some((k, v)) = arg.split_once('=') {
                        params.insert(k.to_string(), v.to_string());
                    } else {
                        positional.push(arg.clone());
                    }
                }
                // Replace {{key}} placeholders
                for (k, v) in &params {
                    nick_val = nick_val.replace(&format!("{{{{{}}}}}", k), v);
                }
                // Build remaining args (non-key=value)
                if !positional.is_empty() {
                    result = format!("{} {}", nick_val, positional.join(" "));
                } else {
                    result = nick_val;
                }
            } else {
                result = nick_val;
            }
        } else if parts.len() > 1 {
            result = format!("{} {}", nick_val, parts[1]);
        } else {
            result = nick_val;
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

fn chrono_time() -> String {
    let secs = now_secs();
    let _hours = (secs % 86400) / 3600;
    let _mins = (secs % 3600) / 60;
    let _s = secs % 60;
    // Adjust for local timezone offset
    let offset: i64 = {
        let now = std::time::SystemTime::now();
        let since_epoch = now.duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs() as i64;
        // Use libc to get local time
        unsafe {
            let mut tm: libc::tm = std::mem::zeroed();
            libc::localtime_r(&since_epoch, &mut tm);
            tm.tm_gmtoff
        }
    };
    let local_secs = (secs as i64 + offset) as u64;
    let h = (local_secs % 86400) / 3600;
    let m = (local_secs % 3600) / 60;
    let sec = local_secs % 60;
    format!("{:02}:{:02}:{:02}", h, m, sec)
}
