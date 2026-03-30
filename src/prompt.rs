use std::env;
use std::process::Command;
use std::sync::OnceLock;

use crate::config::Config;

static HOSTNAME: OnceLock<String> = OnceLock::new();
static USERNAME: OnceLock<String> = OnceLock::new();

/// Build the shell prompt string with colors from config
pub fn build_prompt(config: &Config) -> String {
    let user = USERNAME.get_or_init(|| {
        env::var("USER").unwrap_or_else(|_| "user".to_string())
    });
    let host = HOSTNAME.get_or_init(|| {
        Command::new("hostname")
            .arg("-s")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| "localhost".to_string())
    });

    let cwd_full = env::current_dir().unwrap_or_default().to_string_lossy().to_string();
    let cwd = {
        let home = dirs::home_dir().unwrap_or_default();
        let p = env::current_dir().unwrap_or_default();
        if p == home {
            "~".to_string()
        } else if p.starts_with(&home) {
            format!("~/{}", p.strip_prefix(&home).unwrap().display())
        } else {
            p.display().to_string()
        }
    };

    // Directory color: check dir_colors patterns, fall back to c_cwd
    let dir_color = get_dir_color(&cwd_full, config);

    let git = git_branch();
    let git_str = if git.is_empty() {
        String::new()
    } else {
        format!(" \x1b[38;5;{}m({})\x1b[0m", config.c_git, git)
    };

    // Set terminal window title via OSC
    let title = format!("\x1b]0;rush: {}\x07", cwd);

    // Prompt: user@host: cwd/ (git) >
    format!(
        "{}\x1b[38;5;{}m\x1b[1m{}\x1b[0m\x1b[38;5;{}m@{}\x1b[0m:\x1b[38;5;{}m {}/\x1b[0m{} \x1b[38;5;{}m>\x1b[0m ",
        title, config.c_user, user, config.c_host, host, dir_color, cwd, git_str, config.c_prompt
    )
}

/// Get directory-specific color from config patterns
fn get_dir_color(cwd: &str, config: &Config) -> u8 {
    for (pattern, color) in &config.dir_colors {
        if cwd.contains(pattern.as_str()) {
            return *color;
        }
    }
    config.c_cwd
}

/// Get current git branch (empty string if not in a repo)
fn git_branch() -> String {
    let mut dir = env::current_dir().unwrap_or_default();
    for _ in 0..10 {
        let head = dir.join(".git/HEAD");
        if head.exists() {
            if let Ok(content) = std::fs::read_to_string(head) {
                if let Some(branch) = content.strip_prefix("ref: refs/heads/") {
                    return branch.trim().to_string();
                }
            }
            return String::new(); // Detached HEAD
        }
        if !dir.pop() {
            break;
        }
    }
    String::new()
}
