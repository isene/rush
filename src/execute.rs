use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::Pid;
use std::collections::HashMap;
use std::env;
use std::path::Path;
use std::process::Command;

use crate::config::{self, Config, State};

pub struct Job {
    pub pid: i32,
    pub cmd: String,
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

    // Clean up completed background jobs
    cleanup_jobs(jobs);

    // Expand history (!!, !-N, !N) before nick expansion
    let line = expand_history(line, &state.history);

    // Expand nicks (with parametrized support)
    let line = expand_nicks(&line, &config.nick, &config.gnick);

    // Handle colon commands
    if line.starts_with(':') {
        return handle_colon_command(&line, config, state, jobs);
    }

    // xrpn integration: = expr
    if line.starts_with('=') {
        let expr = &line[1..].trim();
        let cmd = format!("echo \"{},prx,off\" | xrpn", expr);
        return run_via_shell(&cmd, &mut HashMap::new());
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
                let _prev = env::var("OLDPWD").unwrap_or_default();
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
                        return Command::new("xdg-open").arg(&expanded).spawn()
                            .map(|_| 0).unwrap_or(127);
                    }
                }
            }
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
    let code = run_command(cmd_line, background, jobs);

    // Auto-correct: suggest similar commands when not found
    if code == 127 && config.auto_correct && !parts[0].contains('/') {
        let suggestions = find_similar_commands(&parts[0], exe_cache, 3);
        if !suggestions.is_empty() {
            eprintln!("Did you mean:");
            for (i, s) in suggestions.iter().enumerate() {
                eprintln!("  {} {}", i + 1, s);
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
fn cleanup_jobs(jobs: &mut HashMap<u32, Job>) {
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
            Err(_e) => {
                eprintln!("rush: {}: command not found", cmd);
                127
            }
        }
    }
}

fn handle_colon_command(line: &str, config: &mut Config, state: &mut State, jobs: &mut HashMap<u32, Job>) -> i32 {
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
                    // Display as integer if it's a whole number
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
            println!("\x1b[1m  {:>6}  Command\x1b[0m", "Count");
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
                    println!("  [{}] PID {} : {}", id, job.pid, job.cmd);
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
                match waitpid(Pid::from_raw(job.pid), None) {
                    Ok(WaitStatus::Exited(_, code)) => code,
                    _ => 1,
                }
            } else {
                eprintln!("No such job: {}", n);
                1
            }
        }
        "help" => {
            println!("\x1b[1mrush commands:\x1b[0m");
            println!("  :nick [name = val | -name]   Aliases");
            println!("  :gnick [name = val | -name]  Global aliases");
            println!("  :bm [name [path] | -name]    Bookmarks");
            println!("  :dirs                         Directory history");
            println!("  :history [n]                  Command history");
            println!("  :rehash                       Rebuild command cache");
            println!("  :theme [name]                 Set color theme");
            println!("  :calc <expr>                  Calculator (+,-,*,/,%,**,sqrt,sin,cos,tan,log)");
            println!("  = <expr>                      xrpn RPN calculator");
            println!("  :stats                        Top 20 most-used commands");
            println!("  :jobs                         List background jobs");
            println!("  :fg <n>                       Bring job to foreground");
            println!("  :help                         This help");
            println!();
            println!("\x1b[1mHistory expansion:\x1b[0m");
            println!("  !!                            Last command");
            println!("  !N                            Command number N");
            println!("  !-N                           Nth previous command");
            println!();
            println!("\x1b[1mKeys:\x1b[0m");
            println!("  Ctrl-G                        Edit line in $EDITOR");
            println!("  Right arrow                   Accept history suggestion");
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
