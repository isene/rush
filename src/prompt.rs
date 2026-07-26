use crust::style;
use std::env;
use std::process::Command;
use std::sync::{Mutex, OnceLock};

use crate::config::Config;

static HOSTNAME: OnceLock<String> = OnceLock::new();
static USERNAME: OnceLock<String> = OnceLock::new();
static GIT_CACHE: OnceLock<Mutex<(String, String)>> = OnceLock::new();

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
        format!(" {}", style::styled(&format!("({git})"), Some(config.c_git), None, ""))
    };

    // Set terminal window title via OSC
    let title = style::title_seq(&format!("rush: {cwd}"));

    // Use root colors when running as root
    let is_root = unsafe { libc::getuid() } == 0;
    let user_color = if is_root { config.c_user_root } else { config.c_user };
    let host_color = if is_root { config.c_host_root } else { config.c_host };

    // Prompt: user@host: cwd/ (git) >
    format!(
        "{}{}{}:{}{} {} ",
        title,
        style::styled(user, Some(user_color), None, "b"),
        style::styled(&format!("@{host}"), Some(host_color), None, ""),
        style::styled(&format!(" {cwd}/"), Some(dir_color), None, ""),
        git_str,
        style::styled(">", Some(config.c_prompt), None, "")
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

/// Get current git branch (empty string if not in a repo), cached per directory
fn git_branch() -> String {
    let cwd = env::current_dir().unwrap_or_default().to_string_lossy().to_string();
    let cache = GIT_CACHE.get_or_init(|| Mutex::new((String::new(), String::new())));
    let mut cached = cache.lock().unwrap();
    if cached.0 == cwd {
        return cached.1.clone();
    }
    let branch = git_branch_lookup();
    *cached = (cwd, branch.clone());
    branch
}

/// Actual git branch lookup via .git/HEAD
fn git_branch_lookup() -> String {
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
