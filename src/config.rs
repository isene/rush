use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone)]
pub struct Config {
    pub nick: HashMap<String, String>,
    pub gnick: HashMap<String, String>,
    pub bookmarks: HashMap<String, String>,
    pub history_dedup: String,       // off, full, smart
    pub auto_correct: bool,
    pub completion_fuzzy: bool,
    pub completion_case_sensitive: bool,
    pub completion_limit: usize,
    pub show_tips: bool,
    // Colors (xterm-256)
    pub c_prompt: u8,
    pub c_cmd: u8,
    pub c_nick: u8,
    pub c_gnick: u8,
    pub c_path: u8,
    pub c_switch: u8,
    pub c_bookmark: u8,
    pub c_colon: u8,
    pub c_tabselect: u8,
    pub c_taboption: u8,
    pub c_dir: u8,
    pub c_exec: u8,
    pub c_file: u8,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            nick: HashMap::new(),
            gnick: HashMap::new(),
            bookmarks: HashMap::new(),
            history_dedup: "smart".to_string(),
            auto_correct: false,
            completion_fuzzy: false,
            completion_case_sensitive: false,
            completion_limit: 10,
            show_tips: true,
            c_prompt: 208,
            c_cmd: 48,
            c_nick: 87,
            c_gnick: 87,
            c_path: 7,
            c_switch: 220,
            c_bookmark: 51,
            c_colon: 33,
            c_tabselect: 214,
            c_taboption: 244,
            c_dir: 12,
            c_exec: 9,
            c_file: 7,
        }
    }
}

#[derive(Serialize, Deserialize, Default)]
pub struct State {
    pub history: Vec<String>,
    pub cmd_frequency: HashMap<String, usize>,
    pub exe_cache: Vec<String>,
    pub exe_cache_time: u64,
    pub dirs: Vec<String>,
}

impl Config {
    pub fn config_path() -> PathBuf {
        dirs::home_dir().unwrap_or_default().join(".rushrc.json")
    }

    pub fn load() -> Self {
        let path = Self::config_path();
        if path.exists() {
            if let Ok(data) = fs::read_to_string(&path) {
                if let Ok(cfg) = serde_json::from_str(&data) {
                    return cfg;
                }
            }
        }
        Self::default()
    }

    pub fn save(&self) {
        let path = Self::config_path();
        if let Ok(data) = serde_json::to_string_pretty(self) {
            let _ = fs::write(path, data);
        }
    }
}

impl State {
    pub fn state_path() -> PathBuf {
        dirs::home_dir().unwrap_or_default().join(".rushstate.json")
    }

    pub fn load() -> Self {
        let path = Self::state_path();
        if path.exists() {
            if let Ok(data) = fs::read_to_string(&path) {
                if let Ok(state) = serde_json::from_str(&data) {
                    return state;
                }
            }
        }
        Self::default()
    }

    pub fn save(&self) {
        let path = Self::state_path();
        if let Ok(data) = serde_json::to_string_pretty(self) {
            let _ = fs::write(path, data);
        }
    }
}
