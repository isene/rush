use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Bookmark {
    pub path: String,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Config {
    pub nick: HashMap<String, String>,
    pub gnick: HashMap<String, String>,
    pub bookmarks: HashMap<String, Bookmark>,
    pub history_dedup: String,       // off, full, smart
    pub auto_correct: bool,
    pub completion_fuzzy: bool,
    pub completion_case_sensitive: bool,
    pub completion_limit: usize,
    pub show_tips: bool,
    #[serde(default)]
    pub show_cmd: bool,
    #[serde(default)]
    pub slow_command_threshold: u64,  // seconds, 0 = disabled
    #[serde(default)]
    pub session_autosave: u64,  // seconds, 0 = disabled
    #[serde(default)]
    pub completion_show_metadata: bool,
    #[serde(default)]
    pub file_manager: String,  // rtfm, pointer, etc.
    #[serde(default)]
    pub validation_rules: HashMap<String, String>,
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
    #[serde(default = "default_suggestion_color")]
    pub c_suggestion: u8,
    // Prompt colors
    #[serde(default = "default_c_user")]
    pub c_user: u8,
    #[serde(default = "default_c_host")]
    pub c_host: u8,
    #[serde(default = "default_c_cwd")]
    pub c_cwd: u8,
    #[serde(default = "default_c_git")]
    pub c_git: u8,
    #[serde(default = "default_c_stamp")]
    pub c_stamp: u8,
    // Directory-specific colors: [["pattern", color], ...]
    #[serde(default)]
    pub dir_colors: Vec<(String, u8)>,
}

fn default_suggestion_color() -> u8 { 240 }
fn default_c_user() -> u8 { 2 }
fn default_c_host() -> u8 { 2 }
fn default_c_cwd() -> u8 { 81 }
fn default_c_git() -> u8 { 243 }
fn default_c_stamp() -> u8 { 240 }

/// Predefined color themes
pub struct Theme {
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
    pub c_suggestion: u8,
}

pub fn get_theme(name: &str) -> Option<Theme> {
    match name {
        "default" => Some(Theme {
            c_prompt: 208, c_cmd: 48, c_nick: 87, c_gnick: 87, c_path: 7,
            c_switch: 220, c_bookmark: 51, c_colon: 33, c_tabselect: 214,
            c_taboption: 244, c_dir: 12, c_exec: 9, c_file: 7, c_suggestion: 240,
        }),
        "solarized" => Some(Theme {
            c_prompt: 136, c_cmd: 64, c_nick: 37, c_gnick: 37, c_path: 246,
            c_switch: 166, c_bookmark: 33, c_colon: 61, c_tabselect: 136,
            c_taboption: 240, c_dir: 33, c_exec: 160, c_file: 246, c_suggestion: 240,
        }),
        "dracula" => Some(Theme {
            c_prompt: 141, c_cmd: 84, c_nick: 117, c_gnick: 117, c_path: 253,
            c_switch: 215, c_bookmark: 212, c_colon: 141, c_tabselect: 215,
            c_taboption: 244, c_dir: 117, c_exec: 212, c_file: 253, c_suggestion: 242,
        }),
        "gruvbox" => Some(Theme {
            c_prompt: 214, c_cmd: 142, c_nick: 108, c_gnick: 108, c_path: 223,
            c_switch: 167, c_bookmark: 109, c_colon: 175, c_tabselect: 214,
            c_taboption: 245, c_dir: 109, c_exec: 167, c_file: 223, c_suggestion: 241,
        }),
        "nord" => Some(Theme {
            c_prompt: 110, c_cmd: 150, c_nick: 116, c_gnick: 116, c_path: 253,
            c_switch: 173, c_bookmark: 110, c_colon: 139, c_tabselect: 110,
            c_taboption: 244, c_dir: 110, c_exec: 173, c_file: 253, c_suggestion: 243,
        }),
        "monokai" => Some(Theme {
            c_prompt: 197, c_cmd: 148, c_nick: 81, c_gnick: 81, c_path: 252,
            c_switch: 208, c_bookmark: 141, c_colon: 197, c_tabselect: 208,
            c_taboption: 244, c_dir: 81, c_exec: 197, c_file: 252, c_suggestion: 242,
        }),
        _ => None,
    }
}

pub fn apply_theme(config: &mut Config, theme: &Theme) {
    config.c_prompt = theme.c_prompt;
    config.c_cmd = theme.c_cmd;
    config.c_nick = theme.c_nick;
    config.c_gnick = theme.c_gnick;
    config.c_path = theme.c_path;
    config.c_switch = theme.c_switch;
    config.c_bookmark = theme.c_bookmark;
    config.c_colon = theme.c_colon;
    config.c_tabselect = theme.c_tabselect;
    config.c_taboption = theme.c_taboption;
    config.c_dir = theme.c_dir;
    config.c_exec = theme.c_exec;
    config.c_file = theme.c_file;
    config.c_suggestion = theme.c_suggestion;
}

pub fn theme_names() -> &'static [&'static str] {
    &["default", "solarized", "dracula", "gruvbox", "nord", "monokai"]
}

impl Default for Config {
    fn default() -> Self {
        Self {
            nick: HashMap::from([
                ("ls".to_string(), "ls --color -F".to_string()),
                ("ll".to_string(), "ls -la --color -F".to_string()),
                ("la".to_string(), "ls -a --color -F".to_string()),
                ("grep".to_string(), "grep --color=auto".to_string()),
            ]),
            gnick: HashMap::new(),
            bookmarks: HashMap::new(),
            history_dedup: "smart".to_string(),
            auto_correct: false,
            completion_fuzzy: false,
            completion_case_sensitive: false,
            completion_limit: 10,
            show_tips: true,
            show_cmd: true,
            slow_command_threshold: 0,
            session_autosave: 0,
            completion_show_metadata: false,
            file_manager: "rtfm".to_string(),
            validation_rules: HashMap::new(),
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
            c_suggestion: 240,
            c_user: 2,
            c_host: 2,
            c_cwd: 81,
            c_git: 243,
            c_stamp: 240,
            dir_colors: vec![],
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
    #[serde(default)]
    pub recordings: HashMap<String, Vec<String>>,
    #[serde(default)]
    pub completion_weights: HashMap<String, usize>,
}

impl Config {
    pub fn config_path() -> PathBuf {
        dirs::home_dir().unwrap_or_default().join(".rushrc.json")
    }

    pub fn load() -> Self {
        let path = Self::config_path();
        if path.exists() {
            if let Ok(data) = fs::read_to_string(&path) {
                // Try loading new format first
                if let Ok(mut cfg) = serde_json::from_str::<Config>(&data) {
                    let defaults = Self::default();
                    for (k, v) in &defaults.nick {
                        cfg.nick.entry(k.clone()).or_insert_with(|| v.clone());
                    }
                    return cfg;
                }
                // Try migrating from old format (bookmarks as HashMap<String, String>)
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(&data) {
                    if let Some(obj) = val.as_object() {
                        if let Some(bm) = obj.get("bookmarks") {
                            if let Some(bm_obj) = bm.as_object() {
                                // Check if bookmarks are plain strings (old format)
                                let needs_migrate = bm_obj.values().any(|v| v.is_string());
                                if needs_migrate {
                                    let mut migrated = val.clone();
                                    let new_bm: serde_json::Map<String, serde_json::Value> = bm_obj.iter().map(|(k, v)| {
                                        if v.is_string() {
                                            (k.clone(), serde_json::json!({
                                                "path": v.as_str().unwrap_or(""),
                                                "tags": []
                                            }))
                                        } else {
                                            (k.clone(), v.clone())
                                        }
                                    }).collect();
                                    migrated["bookmarks"] = serde_json::Value::Object(new_bm);
                                    if let Ok(mut cfg) = serde_json::from_value::<Config>(migrated) {
                                        let defaults = Self::default();
                                        for (k, v) in &defaults.nick {
                                            cfg.nick.entry(k.clone()).or_insert_with(|| v.clone());
                                        }
                                        // Save migrated config
                                        cfg.save();
                                        return cfg;
                                    }
                                }
                            }
                        }
                    }
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
