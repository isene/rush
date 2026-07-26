use crust::style;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::process::Command;

/// Plugin manifest (plugin.json)
#[derive(Deserialize, Debug, Clone)]
pub struct PluginManifest {
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub hooks: Vec<String>,     // pre_cmd, post_cmd, on_prompt, on_cd
    #[serde(default)]
    pub commands: Vec<String>,  // custom :commands the plugin provides
    #[serde(default)]
    #[allow(dead_code)] // part of plugin manifest API; consumed by get_completions
    pub completions: bool,      // plugin can provide completions
}

/// Context sent to plugins as JSON on stdin
#[derive(Serialize)]
pub struct PluginContext {
    pub hook: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<String>,
    pub cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub word: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<String>,
}

/// Response from plugin (JSON on stdout)
#[derive(Deserialize, Default, Debug)]
#[allow(dead_code)] // command/completions/prompt are part of the plugin response API
pub struct PluginResponse {
    #[serde(default)]
    pub action: String,          // allow, block, modify
    #[serde(default)]
    pub message: String,         // message to display
    #[serde(default)]
    pub output: String,          // output for commands
    #[serde(default)]
    pub command: String,         // modified command (for pre_cmd modify)
    #[serde(default)]
    pub completions: Vec<String>, // completion candidates
    #[serde(default)]
    pub prompt: String,          // prompt addition (for on_prompt)
}

/// A loaded plugin
#[derive(Debug, Clone)]
pub struct Plugin {
    pub manifest: PluginManifest,
    pub path: PathBuf,  // path to the plugin directory
    pub enabled: bool,
}

impl Plugin {
    fn run_path(&self) -> PathBuf {
        self.path.join("run")
    }

    /// Call the plugin with a context, return the response
    pub fn call(&self, ctx: &PluginContext) -> Option<PluginResponse> {
        let run = self.run_path();
        if !run.exists() {
            return None;
        }
        let input = serde_json::to_string(ctx).ok()?;
        let output = Command::new(&run)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok()
            .and_then(|mut child| {
                use std::io::Write;
                if let Some(ref mut stdin) = child.stdin {
                    let _ = stdin.write_all(input.as_bytes());
                }
                child.wait_with_output().ok()
            })?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        serde_json::from_str(stdout.trim()).ok()
    }
}

/// Plugin manager
pub struct PluginManager {
    pub plugins: Vec<Plugin>,
}

impl PluginManager {
    pub fn new() -> Self {
        Self { plugins: Vec::new() }
    }

    /// Load all plugins from ~/.rush/plugins/
    pub fn load_all(&mut self) {
        let dir = plugin_dir();
        if !dir.exists() {
            return;
        }
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let manifest_path = path.join("plugin.json");
                if manifest_path.exists() {
                    if let Ok(data) = std::fs::read_to_string(&manifest_path) {
                        if let Ok(manifest) = serde_json::from_str::<PluginManifest>(&data) {
                            // Check run exists and is executable
                            let run = path.join("run");
                            if run.exists() {
                                self.plugins.push(Plugin {
                                    manifest,
                                    path: path.clone(),
                                    enabled: true,
                                });
                            }
                        }
                    }
                }
            }
        }
    }

    /// Run a hook on all plugins that subscribe to it
    /// Returns None if all allow, Some(response) if any blocks/modifies
    pub fn run_hook(&self, hook: &str, ctx: &PluginContext) -> Option<PluginResponse> {
        for plugin in &self.plugins {
            if !plugin.enabled {
                continue;
            }
            if !plugin.manifest.hooks.contains(&hook.to_string()) {
                continue;
            }
            if let Some(resp) = plugin.call(ctx) {
                match resp.action.as_str() {
                    "block" => return Some(resp),
                    "modify" => return Some(resp),
                    _ => {} // "allow" or empty: continue
                }
            }
        }
        None
    }

    /// Run a plugin command
    pub fn run_command(&self, cmd: &str, args: &str) -> Option<PluginResponse> {
        for plugin in &self.plugins {
            if !plugin.enabled {
                continue;
            }
            if plugin.manifest.commands.contains(&cmd.to_string()) {
                let ctx = PluginContext {
                    hook: "command".to_string(),
                    command: Some(cmd.to_string()),
                    args: Some(args.to_string()),
                    cwd: std::env::current_dir().unwrap_or_default().to_string_lossy().to_string(),
                    exit_code: None,
                    word: None,
                    line: None,
                };
                return plugin.call(&ctx);
            }
        }
        None
    }

    /// Get completions from all plugins
    #[allow(dead_code)] // plugin completion API; not yet wired to the completion path
    pub fn get_completions(&self, word: &str, line: &str) -> Vec<String> {
        let mut all = Vec::new();
        let ctx = PluginContext {
            hook: "complete".to_string(),
            command: None,
            args: None,
            cwd: std::env::current_dir().unwrap_or_default().to_string_lossy().to_string(),
            exit_code: None,
            word: Some(word.to_string()),
            line: Some(line.to_string()),
        };
        for plugin in &self.plugins {
            if !plugin.enabled || !plugin.manifest.completions {
                continue;
            }
            if let Some(resp) = plugin.call(&ctx) {
                all.extend(resp.completions);
            }
        }
        all
    }

    /// List plugins
    pub fn list(&self) {
        if self.plugins.is_empty() {
            println!("No plugins loaded. Add plugins to ~/.rush/plugins/");
            println!();
            println!("Plugin structure:");
            println!("  ~/.rush/plugins/myplugin/");
            println!("    plugin.json    # manifest");
            println!("    run            # executable (any language)");
            println!();
            println!("plugin.json format:");
            println!("  {{");
            println!("    \"name\": \"myplugin\",");
            println!("    \"hooks\": [\"pre_cmd\", \"post_cmd\"],");
            println!("    \"commands\": [\"mycommand\"],");
            println!("    \"completions\": true");
            println!("  }}");
            println!();
            println!("The 'run' executable receives JSON on stdin:");
            println!("  {{\"hook\": \"pre_cmd\", \"command\": \"rm -rf /\", \"cwd\": \"/home\"}}");
            println!();
            println!("And responds with JSON on stdout:");
            println!("  {{\"action\": \"block\", \"message\": \"Nope.\"}}");
            return;
        }
        for p in &self.plugins {
            let status = if p.enabled { "enabled" } else { "disabled" };
            println!("  {} {} [{}]", style::bold(&p.manifest.name), p.manifest.version, status);
            if !p.manifest.description.is_empty() {
                println!("    {}", p.manifest.description);
            }
            if !p.manifest.hooks.is_empty() {
                println!("    hooks: {}", p.manifest.hooks.join(", "));
            }
            if !p.manifest.commands.is_empty() {
                println!("    commands: {}", p.manifest.commands.join(", "));
            }
        }
    }

    pub fn enable(&mut self, name: &str) -> bool {
        for p in &mut self.plugins {
            if p.manifest.name == name {
                p.enabled = true;
                return true;
            }
        }
        false
    }

    pub fn disable(&mut self, name: &str) -> bool {
        for p in &mut self.plugins {
            if p.manifest.name == name {
                p.enabled = false;
                return true;
            }
        }
        false
    }
}

fn plugin_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_default().join(".rush").join("plugins")
}
