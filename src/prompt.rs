use std::env;
use std::path::Path;
use std::process::Command;

/// Build the shell prompt string with colors
pub fn build_prompt(c_prompt: u8) -> String {
    let user = env::var("USER").unwrap_or_else(|_| "user".to_string());
    let host = hostname();
    let cwd = env::current_dir()
        .map(|p| {
            let home = dirs::home_dir().unwrap_or_default();
            if p == home {
                "~".to_string()
            } else if p.starts_with(&home) {
                format!("~/{}", p.strip_prefix(&home).unwrap().display())
            } else {
                p.display().to_string()
            }
        })
        .unwrap_or_else(|_| "?".to_string());

    let git = git_branch();
    let git_str = if git.is_empty() {
        String::new()
    } else {
        format!(" \x1b[38;5;243m({})\x1b[0m", git)
    };

    format!(
        "\x1b[38;5;244m{}@{}\x1b[0m:\x1b[38;5;{}m{}\x1b[0m{} \x1b[38;5;{}m>\x1b[0m ",
        user, host, 81, cwd, git_str, c_prompt
    )
}

/// Get current git branch (empty string if not in a repo)
fn git_branch() -> String {
    let git_head = Path::new(".git/HEAD");
    if git_head.exists() {
        if let Ok(content) = std::fs::read_to_string(git_head) {
            if let Some(branch) = content.strip_prefix("ref: refs/heads/") {
                return branch.trim().to_string();
            }
        }
    }
    // Try parent directories
    let mut dir = env::current_dir().unwrap_or_default();
    for _ in 0..10 {
        let head = dir.join(".git/HEAD");
        if head.exists() {
            if let Ok(content) = std::fs::read_to_string(head) {
                if let Some(branch) = content.strip_prefix("ref: refs/heads/") {
                    return branch.trim().to_string();
                }
            }
        }
        if !dir.pop() {
            break;
        }
    }
    String::new()
}

fn hostname() -> String {
    Command::new("hostname")
        .arg("-s")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "localhost".to_string())
}
