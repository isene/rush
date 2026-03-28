mod config;
mod execute;
mod input;
mod prompt;

use config::{Config, State};
use execute::{build_exe_cache, now_secs, Job};
use std::collections::HashMap;

fn main() {
    // Handle -c flag (run command and exit)
    let args: Vec<String> = std::env::args().collect();
    if args.len() >= 3 && args[1] == "-c" {
        let cmd = args[2..].join(" ");
        let mut config = Config::load();
        let mut state = State::load();
        let exe_cache = build_exe_cache();
        let mut jobs: HashMap<u32, Job> = HashMap::new();
        let code = execute::execute(&cmd, &mut config, &mut state, &exe_cache, &mut jobs);
        std::process::exit(code);
    }

    // Load config and state
    let mut config = Config::load();
    let mut state = State::load();

    // Build or refresh executable cache (60s TTL)
    let now = now_secs();
    let exe_cache = if now - state.exe_cache_time > 60 || state.exe_cache.is_empty() {
        let cache = build_exe_cache();
        state.exe_cache = cache.clone();
        state.exe_cache_time = now;
        cache
    } else {
        state.exe_cache.clone()
    };

    let mut jobs: HashMap<u32, Job> = HashMap::new();

    // Main loop
    loop {
        let line = match input::getline(&config, &mut state, &exe_cache) {
            Some(line) => line,
            None => {
                // Ctrl-D with empty line: exit
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
            _ => {
                // smart: don't add if same as last
                state.history.last().map(|l| l != trimmed).unwrap_or(true)
            }
        };
        if should_add {
            state.history.push(trimmed.to_string());
            if state.history.len() > 200 {
                state.history.remove(0);
            }
        }

        // Execute
        execute::execute(trimmed, &mut config, &mut state, &exe_cache, &mut jobs);
    }
}
