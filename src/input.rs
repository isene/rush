use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal;
use std::collections::HashMap;
use std::io::{self, Write};

use crate::config::{Config, State};
use crate::execute::shell_split;
use crate::prompt;

/// Parse LS_COLORS into a map of extension -> ANSI code
fn parse_ls_colors() -> HashMap<String, String> {
    let mut map = HashMap::new();
    if let Ok(val) = std::env::var("LS_COLORS") {
        for entry in val.split(':') {
            if let Some((key, code)) = entry.split_once('=') {
                map.insert(key.to_string(), code.to_string());
            }
        }
    }
    map
}

/// Get LS_COLORS code for a path
fn ls_color_for(path: &str, ls_colors: &HashMap<String, String>) -> String {
    let p = std::path::Path::new(path.trim_end_matches('/'));
    if path.ends_with('/') || p.is_dir() {
        if let Some(code) = ls_colors.get("di") {
            return format!("\x1b[{}m", code);
        }
        return "\x1b[38;5;12m".to_string(); // default blue
    }
    if p.is_symlink() {
        if let Some(code) = ls_colors.get("ln") {
            return format!("\x1b[{}m", code);
        }
    }
    // Check by extension
    if let Some(ext) = p.extension().and_then(|e| e.to_str()) {
        let key = format!("*.{}", ext);
        if let Some(code) = ls_colors.get(&key) {
            return format!("\x1b[{}m", code);
        }
    }
    // Executable
    if is_executable(path) {
        if let Some(code) = ls_colors.get("ex") {
            return format!("\x1b[{}m", code);
        }
        return "\x1b[38;5;10m".to_string(); // default green
    }
    String::new() // no special color
}

fn is_executable(path: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o111 != 0 && m.is_file())
        .unwrap_or(false)
}

use std::sync::Mutex;
use std::sync::OnceLock;

static SWITCH_CACHE: OnceLock<Mutex<HashMap<String, Vec<String>>>> = OnceLock::new();

/// Parse switches from `command --help` output, cached
fn get_switches(cmd: &str) -> Vec<String> {
    let cache = SWITCH_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cache = cache.lock().unwrap();
    if let Some(switches) = cache.get(cmd) {
        return switches.clone();
    }

    let mut switches = Vec::new();
    // Try --help first, then -h
    for flag in &["--help", "-h"] {
        if let Ok(output) = std::process::Command::new(cmd)
            .arg(flag)
            .output()
        {
            let text = String::from_utf8_lossy(&output.stdout).to_string()
                + &String::from_utf8_lossy(&output.stderr);
            if !text.is_empty() {
                // Extract switches: --word or -X patterns
                let re = regex::Regex::new(r"(?:^|\s)(--?[a-zA-Z][\w-]*)").unwrap();
                for cap in re.captures_iter(&text) {
                    let sw = cap[1].to_string();
                    if !switches.contains(&sw) {
                        switches.push(sw);
                    }
                }
                if !switches.is_empty() {
                    break;
                }
            }
        }
    }

    switches.sort();
    cache.insert(cmd.to_string(), switches.clone());
    switches
}

/// Find the best history match for the current prefix
fn find_history_suggestion<'a>(buf: &str, history: &'a [String]) -> Option<&'a str> {
    if buf.is_empty() {
        return None;
    }
    // Search from most recent backward
    for entry in history.iter().rev() {
        if entry.starts_with(buf) && entry != buf {
            return Some(entry.as_str());
        }
    }
    None
}

/// Smart completions for specific commands
fn smart_completions(cmd: &str, prefix: &str) -> Vec<String> {
    let subcommands: &[&str] = match cmd {
        "git" => &[
            "status", "log", "commit", "push", "pull", "checkout", "branch",
            "merge", "diff", "stash", "rebase", "fetch", "clone", "add",
            "reset", "tag",
        ],
        "cargo" => &[
            "build", "run", "test", "check", "clean", "doc", "new", "init",
            "publish", "update",
        ],
        "apt" => &[
            "install", "remove", "update", "upgrade", "search", "show", "list",
        ],
        _ => return Vec::new(),
    };
    subcommands.iter()
        .filter(|s| s.starts_with(prefix))
        .map(|s| s.to_string())
        .collect()
}

/// Check if a line is incomplete (needs continuation)
fn line_needs_continuation(line: &str) -> bool {
    let trimmed = line.trim_end();
    if trimmed.ends_with('\\') || trimmed.ends_with('|')
        || trimmed.ends_with("&&") || trimmed.ends_with("||") {
        return true;
    }
    // Check for unclosed quotes/brackets
    let mut single_q = false;
    let mut double_q = false;
    let mut parens = 0i32;
    let mut brackets = 0i32;
    let mut braces = 0i32;
    let mut escape = false;
    for ch in trimmed.chars() {
        if escape { escape = false; continue; }
        if ch == '\\' { escape = true; continue; }
        if ch == '\'' && !double_q { single_q = !single_q; continue; }
        if ch == '"' && !single_q { double_q = !double_q; continue; }
        if !single_q && !double_q {
            match ch {
                '(' => parens += 1,
                ')' => parens -= 1,
                '[' => brackets += 1,
                ']' => brackets -= 1,
                '{' => braces += 1,
                '}' => braces -= 1,
                _ => {}
            }
        }
    }
    single_q || double_q || parens > 0 || brackets > 0 || braces > 0
}

/// Read a line of input with editing, history, tab completion, syntax highlighting
pub fn getline(
    config: &Config,
    state: &mut State,
    exe_cache: &[String],
    last_cmd_duration: f64,
) -> Option<String> {
    let prompt_str = prompt::build_prompt(config);

    // Draw right prompt with git status and duration
    if config.rprompt {
        draw_right_prompt(config, last_cmd_duration);
    }

    print!("\r{}", prompt_str);
    io::stdout().flush().ok();

    let prompt_width = visible_len(&prompt_str);
    let mut buf = String::new();
    let mut cursor = 0usize; // byte position
    let mut hist_pos: Option<usize> = None;
    let mut saved_buf = String::new();

    // History search state (Shift-Tab)
    let mut history_search_active = false;
    let mut history_search_buf = String::new();
    let mut history_search_matches: Vec<String> = Vec::new();
    let mut history_search_index: usize = 0;

    // Reverse incremental search state (Ctrl-R)
    let mut reverse_search_active = false;
    let mut reverse_search_buf = String::new();
    let mut reverse_search_index: usize = 0;

    // Undo stack: (buffer, cursor) before each edit
    let mut undo_stack: Vec<(String, usize)> = Vec::new();
    let max_undo = 50;

    terminal::enable_raw_mode().ok();

    let result = loop {
        let ev = match event::read() {
            Ok(ev) => ev,
            Err(_) => {
                // Terminal is likely gone (wezterm killed, ssh dropped, etc.)
                if unsafe { libc::isatty(0) } == 0 {
                    terminal::disable_raw_mode().ok();
                    return None; // Exit cleanly
                }
                // Brief pause before retry for transient errors
                std::thread::sleep(std::time::Duration::from_millis(100));
                continue;
            }
        };

        // Handle history search mode
        if history_search_active {
            match ev {
                Event::Key(KeyEvent { code, modifiers, .. }) => {
                    match (code, modifiers) {
                        (KeyCode::Esc, _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                            // Cancel search
                            history_search_active = false;
                            // Clear search display and redraw
                            print!("\r\x1b[K");
                            print!("\r{}", prompt_str);
                            redraw_line(&prompt_str, &buf, cursor, config, exe_cache, &state.history);
                        }
                        (KeyCode::Enter, _) => {
                            // Accept selected match
                            history_search_active = false;
                            if !history_search_matches.is_empty() && history_search_index < history_search_matches.len() {
                                buf = history_search_matches[history_search_index].clone();
                                cursor = buf.len();
                            }
                            // Clear search display and redraw
                            print!("\r\x1b[K");
                            print!("\r{}", prompt_str);
                            redraw_line(&prompt_str, &buf, cursor, config, exe_cache, &state.history);
                        }
                        (KeyCode::Up, _) => {
                            if !history_search_matches.is_empty() && history_search_index + 1 < history_search_matches.len() {
                                history_search_index += 1;
                            }
                            draw_history_search(&history_search_buf, &history_search_matches, history_search_index, prompt_width);
                        }
                        (KeyCode::Down, _) => {
                            if history_search_index > 0 {
                                history_search_index -= 1;
                            }
                            draw_history_search(&history_search_buf, &history_search_matches, history_search_index, prompt_width);
                        }
                        (KeyCode::Backspace, _) => {
                            history_search_buf.pop();
                            history_search_matches = find_history_matches(&history_search_buf, &state.history);
                            history_search_index = 0;
                            draw_history_search(&history_search_buf, &history_search_matches, history_search_index, prompt_width);
                        }
                        (KeyCode::Char(c), _) if !modifiers.contains(KeyModifiers::CONTROL) => {
                            history_search_buf.push(c);
                            history_search_matches = find_history_matches(&history_search_buf, &state.history);
                            history_search_index = 0;
                            draw_history_search(&history_search_buf, &history_search_matches, history_search_index, prompt_width);
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
            continue;
        }

        // Handle reverse incremental search mode (Ctrl-R)
        if reverse_search_active {
            match ev {
                Event::Key(KeyEvent { code, modifiers, .. }) => {
                    match (code, modifiers) {
                        (KeyCode::Esc, _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                            reverse_search_active = false;
                            print!("\r\x1b[K");
                            print!("\r{}", prompt_str);
                            redraw_line(&prompt_str, &buf, cursor, config, exe_cache, &state.history);
                        }
                        (KeyCode::Enter, _) => {
                            reverse_search_active = false;
                            // Accept the match; redraw and let it fall through to return
                            print!("\r\x1b[K");
                            print!("\r{}", prompt_str);
                            cursor = buf.len();
                            redraw_line(&prompt_str, &buf, cursor, config, exe_cache, &state.history);
                            // Now simulate Enter: print newline and break
                            println!();
                            break Some(buf);
                        }
                        (KeyCode::Char('r'), KeyModifiers::CONTROL) => {
                            // Find next match
                            reverse_search_index += 1;
                            if let Some(m) = find_reverse_search(&reverse_search_buf, &state.history, reverse_search_index) {
                                buf = m.clone();
                                cursor = buf.len();
                            } else {
                                reverse_search_index = reverse_search_index.saturating_sub(1);
                            }
                            draw_reverse_search(&reverse_search_buf, &buf);
                        }
                        (KeyCode::Backspace, _) => {
                            reverse_search_buf.pop();
                            reverse_search_index = 0;
                            if let Some(m) = find_reverse_search(&reverse_search_buf, &state.history, 0) {
                                buf = m.clone();
                                cursor = buf.len();
                            }
                            draw_reverse_search(&reverse_search_buf, &buf);
                        }
                        (KeyCode::Char(c), _) if !modifiers.contains(KeyModifiers::CONTROL) => {
                            reverse_search_buf.push(c);
                            reverse_search_index = 0;
                            if let Some(m) = find_reverse_search(&reverse_search_buf, &state.history, 0) {
                                buf = m.clone();
                                cursor = buf.len();
                            }
                            draw_reverse_search(&reverse_search_buf, &buf);
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
            continue;
        }

        match ev {
            Event::Key(KeyEvent { code, modifiers, .. }) => {
                match (code, modifiers) {
                    // Ctrl-C: clear line and redraw prompt in place
                    (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                        buf.clear();
                        cursor = 0;
                        redraw_line(&prompt_str, &buf, cursor, config, exe_cache, &state.history);
                        continue;
                    }
                    // Ctrl-D: exit
                    (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
                        if buf.is_empty() {
                            println!();
                            break None; // Signal exit
                        }
                    }
                    // Ctrl-R: reverse incremental search
                    (KeyCode::Char('r'), KeyModifiers::CONTROL) => {
                        reverse_search_active = true;
                        reverse_search_buf.clear();
                        reverse_search_index = 0;
                        draw_reverse_search(&reverse_search_buf, &buf);
                    }
                    // Ctrl-L: clear screen
                    (KeyCode::Char('l'), KeyModifiers::CONTROL) => {
                        print!("\x1b[2J\x1b[H{}", prompt_str);
                        redraw_line(&prompt_str, &buf, cursor, config, exe_cache, &state.history);
                    }
                    // Enter: execute (or continue for multi-line)
                    (KeyCode::Enter, _) => {
                        println!();
                        if line_needs_continuation(&buf) {
                            terminal::disable_raw_mode().ok();
                            if let Some(full) = read_continuation(&buf) {
                                break Some(full);
                            } else {
                                break Some(buf);
                            }
                        }
                        break Some(buf);
                    }
                    // Shift-Tab: history search
                    (KeyCode::BackTab, _) => {
                        history_search_active = true;
                        history_search_buf.clear();
                        history_search_matches = find_history_matches("", &state.history);
                        history_search_index = 0;
                        draw_history_search(&history_search_buf, &history_search_matches, history_search_index, prompt_width);
                    }
                    // Tab: interactive completion
                    (KeyCode::Tab, _) => {
                        let (completions, word_start) = gather_completions(&buf, cursor, exe_cache, config, &state.completion_weights);
                        if completions.len() == 1 {
                            let mut new_buf = buf[..word_start].to_string();
                            new_buf.push_str(&completions[0]);
                            if !completions[0].ends_with('/') { new_buf.push(' '); }
                            if cursor < buf.len() { new_buf.push_str(&buf[cursor..]); }
                            let parts: Vec<&str> = new_buf.trim().split_whitespace().collect();
                            if let Some(first) = parts.first() {
                                *state.completion_weights.entry(first.to_string()).or_insert(0) += 1;
                            }
                            buf = new_buf;
                            cursor = buf.len();
                            redraw_line(&prompt_str, &buf, cursor, config, exe_cache, &state.history);
                        } else if completions.len() > 1 {
                            // Enter interactive completion mode
                            let ls_colors = parse_ls_colors();
                            let mut sel: usize = 0;
                            draw_completions(&completions, sel, &ls_colors, &prompt_str);
                            loop {
                                let cev = match event::read() {
                                    Ok(ev) => ev,
                                    Err(_) => break,
                                };
                                match cev {
                                    Event::Key(KeyEvent { code: KeyCode::Tab, .. }) => {
                                        sel = (sel + 1) % completions.len();
                                        draw_completions(&completions, sel, &ls_colors, &prompt_str);
                                    }
                                    Event::Key(KeyEvent { code: KeyCode::BackTab, .. }) => {
                                        sel = (sel + completions.len() - 1) % completions.len();
                                        draw_completions(&completions, sel, &ls_colors, &prompt_str);
                                    }
                                    Event::Key(KeyEvent { code: KeyCode::Enter, .. })
                                    | Event::Key(KeyEvent { code: KeyCode::Right, .. }) => {
                                        // Accept selection
                                        let mut new_buf = buf[..word_start].to_string();
                                        new_buf.push_str(&completions[sel]);
                                        if !completions[sel].ends_with('/') { new_buf.push(' '); }
                                        if cursor < buf.len() { new_buf.push_str(&buf[cursor..]); }
                                        let parts: Vec<&str> = new_buf.trim().split_whitespace().collect();
                                        if let Some(first) = parts.first() {
                                            *state.completion_weights.entry(first.to_string()).or_insert(0) += 1;
                                        }
                                        buf = new_buf;
                                        cursor = buf.len();
                                        // Clear completion display
                                        clear_completions(completions.len());
                                        redraw_line(&prompt_str, &buf, cursor, config, exe_cache, &state.history);
                                        break;
                                    }
                                    Event::Key(KeyEvent { code: KeyCode::Esc, .. }) => {
                                        // Cancel, fill common prefix
                                        let common = common_prefix(&completions);
                                        if common.len() > buf[word_start..cursor].len() {
                                            let mut new_buf = buf[..word_start].to_string();
                                            new_buf.push_str(&common);
                                            if cursor < buf.len() { new_buf.push_str(&buf[cursor..]); }
                                            buf = new_buf;
                                            cursor = buf.len();
                                        }
                                        clear_completions(completions.len());
                                        redraw_line(&prompt_str, &buf, cursor, config, exe_cache, &state.history);
                                        break;
                                    }
                                    _ => {
                                        // Any other key: cancel completion, keep common prefix
                                        let common = common_prefix(&completions);
                                        if common.len() > buf[word_start..cursor].len() {
                                            let mut new_buf = buf[..word_start].to_string();
                                            new_buf.push_str(&common);
                                            if cursor < buf.len() { new_buf.push_str(&buf[cursor..]); }
                                            buf = new_buf;
                                            cursor = buf.len();
                                        }
                                        clear_completions(completions.len());
                                        redraw_line(&prompt_str, &buf, cursor, config, exe_cache, &state.history);
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    // Backspace
                    (KeyCode::Backspace, _) => {
                        if cursor > 0 {
                            undo_stack.push((buf.clone(), cursor));
                            if undo_stack.len() > max_undo { undo_stack.remove(0); }
                            let prev = prev_char_boundary(&buf, cursor);
                            buf.drain(prev..cursor);
                            cursor = prev;
                            redraw_line(&prompt_str, &buf, cursor, config, exe_cache, &state.history);
                        }
                    }
                    // Delete
                    (KeyCode::Delete, _) => {
                        if cursor < buf.len() {
                            undo_stack.push((buf.clone(), cursor));
                            if undo_stack.len() > max_undo { undo_stack.remove(0); }
                            let next = next_char_boundary(&buf, cursor);
                            buf.drain(cursor..next);
                            redraw_line(&prompt_str, &buf, cursor, config, exe_cache, &state.history);
                        }
                    }
                    // Left
                    (KeyCode::Left, _) => {
                        if cursor > 0 {
                            cursor = prev_char_boundary(&buf, cursor);
                            set_cursor_col(prompt_width + display_width(&buf[..cursor]));
                        }
                    }
                    // Home
                    (KeyCode::Home, _) => {
                        cursor = 0;
                        set_cursor_col(prompt_width);
                    }
                    // End
                    (KeyCode::End, _) => {
                        cursor = buf.len();
                        set_cursor_col(prompt_width + display_width(&buf));
                    }
                    // Up: history
                    (KeyCode::Up, _) => {
                        if !state.history.is_empty() {
                            let pos = match hist_pos {
                                Some(p) => (p + 1).min(state.history.len() - 1),
                                None => {
                                    saved_buf = buf.clone();
                                    0
                                }
                            };
                            hist_pos = Some(pos);
                            buf = state.history[state.history.len() - 1 - pos].clone();
                            cursor = buf.len();
                            redraw_line(&prompt_str, &buf, cursor, config, exe_cache, &state.history);
                        }
                    }
                    // Down: history
                    (KeyCode::Down, _) => {
                        match hist_pos {
                            Some(0) => {
                                hist_pos = None;
                                buf = saved_buf.clone();
                                cursor = buf.len();
                                redraw_line(&prompt_str, &buf, cursor, config, exe_cache, &state.history);
                            }
                            Some(p) => {
                                hist_pos = Some(p - 1);
                                buf = state.history[state.history.len() - p].clone();
                                cursor = buf.len();
                                redraw_line(&prompt_str, &buf, cursor, config, exe_cache, &state.history);
                            }
                            None => {}
                        }
                    }
                    // Ctrl-W: delete word backward
                    (KeyCode::Char('w'), KeyModifiers::CONTROL) => {
                        if cursor > 0 {
                            undo_stack.push((buf.clone(), cursor));
                            if undo_stack.len() > max_undo { undo_stack.remove(0); }
                            let mut new_pos = cursor;
                            // Skip trailing spaces
                            while new_pos > 0 && buf.as_bytes()[new_pos - 1] == b' ' {
                                new_pos -= 1;
                            }
                            // Skip word chars
                            while new_pos > 0 && buf.as_bytes()[new_pos - 1] != b' ' {
                                new_pos -= 1;
                            }
                            buf.drain(new_pos..cursor);
                            cursor = new_pos;
                            redraw_line(&prompt_str, &buf, cursor, config, exe_cache, &state.history);
                        }
                    }
                    // Ctrl-A: beginning of line
                    (KeyCode::Char('a'), KeyModifiers::CONTROL) => {
                        cursor = 0;
                        set_cursor_col(prompt_width);
                    }
                    // Ctrl-E: end of line
                    (KeyCode::Char('e'), KeyModifiers::CONTROL) => {
                        cursor = buf.len();
                        set_cursor_col(prompt_width + display_width(&buf));
                    }
                    // Ctrl-K: kill to end of line
                    (KeyCode::Char('k'), KeyModifiers::CONTROL) => {
                        undo_stack.push((buf.clone(), cursor));
                        if undo_stack.len() > max_undo { undo_stack.remove(0); }
                        buf.truncate(cursor);
                        redraw_line(&prompt_str, &buf, cursor, config, exe_cache, &state.history);
                    }
                    // Ctrl-Y: copy line to clipboard
                    (KeyCode::Char('y'), KeyModifiers::CONTROL) => {
                        let _ = std::process::Command::new("sh")
                            .arg("-c")
                            .arg(format!("echo -n {} | xclip -selection clipboard 2>/dev/null || echo -n {} | xsel --clipboard 2>/dev/null || echo -n {} | wl-copy 2>/dev/null",
                                shell_quote(&buf), shell_quote(&buf), shell_quote(&buf)))
                            .status();
                        // Brief flash to confirm
                        print!("\r\x1b[K\x1b[38;5;243mCopied to clipboard\x1b[0m");
                        io::stdout().flush().ok();
                        std::thread::sleep(std::time::Duration::from_millis(300));
                        redraw_line(&prompt_str, &buf, cursor, config, exe_cache, &state.history);
                    }
                    // Ctrl-U: kill to beginning
                    (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
                        undo_stack.push((buf.clone(), cursor));
                        if undo_stack.len() > max_undo { undo_stack.remove(0); }
                        buf.drain(..cursor);
                        cursor = 0;
                        redraw_line(&prompt_str, &buf, cursor, config, exe_cache, &state.history);
                    }
                    // Ctrl-_ : undo last edit
                    (KeyCode::Char('_'), KeyModifiers::CONTROL) => {
                        if let Some((prev_buf, prev_cursor)) = undo_stack.pop() {
                            buf = prev_buf;
                            cursor = prev_cursor;
                            redraw_line(&prompt_str, &buf, cursor, config, exe_cache, &state.history);
                        }
                    }
                    // Ctrl-G: edit in $EDITOR
                    (KeyCode::Char('g'), KeyModifiers::CONTROL) => {
                        terminal::disable_raw_mode().ok();
                        let tmpdir = std::env::temp_dir();
                        let tmpfile = tmpdir.join("rush_edit.tmp");
                        let _ = std::fs::write(&tmpfile, &buf);
                        let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vim".to_string());
                        let _ = std::process::Command::new(&editor)
                            .arg(&tmpfile)
                            .status();
                        if let Ok(contents) = std::fs::read_to_string(&tmpfile) {
                            buf = contents.trim_end_matches('\n').to_string();
                            cursor = buf.len();
                        }
                        let _ = std::fs::remove_file(&tmpfile);
                        terminal::enable_raw_mode().ok();
                        // Redraw prompt and buffer
                        print!("\r\x1b[K{}", prompt_str);
                        redraw_line(&prompt_str, &buf, cursor, config, exe_cache, &state.history);
                    }
                    // Right arrow: accept suggestion or move cursor
                    (KeyCode::Right, _) => {
                        // If cursor is at end and there's a suggestion, accept it
                        if cursor == buf.len() {
                            if let Some(suggestion) = find_history_suggestion(&buf, &state.history) {
                                buf = suggestion.to_string();
                                cursor = buf.len();
                                redraw_line(&prompt_str, &buf, cursor, config, exe_cache, &state.history);
                            }
                        } else if cursor < buf.len() {
                            cursor = next_char_boundary(&buf, cursor);
                            set_cursor_col(prompt_width + display_width(&buf[..cursor]));
                        }
                    }
                    // Regular char (but never insert tab)
                    (KeyCode::Char(c), _) if c != '\t' => {
                        undo_stack.push((buf.clone(), cursor));
                        if undo_stack.len() > max_undo { undo_stack.remove(0); }

                        // Abbreviation expansion on Space (feature 11)
                        if c == ' ' && !config.abbrev.is_empty() {
                            // Extract the current word (from last space or start)
                            let word_start = buf[..cursor].rfind(' ').map(|i| i + 1).unwrap_or(0);
                            let word = &buf[word_start..cursor];
                            if let Some(expansion) = config.abbrev.get(word).cloned() {
                                // Briefly underline the abbreviation
                                let before = buf[..word_start].to_string();
                                let after = buf[cursor..].to_string();
                                let underlined = format!("{}\x1b[4m{}\x1b[0m", before, word);
                                print!("\r\x1b[K{}{}{}", prompt_str, underlined, after);
                                io::stdout().flush().ok();
                                std::thread::sleep(std::time::Duration::from_millis(150));
                                // Replace abbreviation with expansion + space
                                buf = format!("{}{} {}", before, expansion, after);
                                cursor = before.len() + expansion.len() + 1;
                                redraw_line(&prompt_str, &buf, cursor, config, exe_cache, &state.history);
                                continue;
                            }
                        }

                        // Auto-closing pairs (feature 9)
                        if config.auto_pair {
                            // Closing chars: skip over if already at cursor
                            let skip_close = matches!(c, ')' | ']' | '}' | '"' | '\'')
                                && cursor < buf.len()
                                && buf.as_bytes().get(cursor) == Some(&(c as u8));
                            if skip_close {
                                cursor = next_char_boundary(&buf, cursor);
                                redraw_line(&prompt_str, &buf, cursor, config, exe_cache, &state.history);
                                continue;
                            }
                            // Opening chars: insert pair
                            let close_char = match c {
                                '(' => Some(')'),
                                '[' => Some(']'),
                                '{' => Some('}'),
                                '"' => Some('"'),
                                '\'' => Some('\''),
                                _ => None,
                            };
                            if let Some(close) = close_char {
                                let next_is_space_or_end = cursor >= buf.len()
                                    || buf.as_bytes().get(cursor) == Some(&b' ');
                                if next_is_space_or_end {
                                    buf.insert(cursor, c);
                                    buf.insert(cursor + c.len_utf8(), close);
                                    cursor += c.len_utf8();
                                    redraw_line(&prompt_str, &buf, cursor, config, exe_cache, &state.history);
                                    continue;
                                }
                            }
                        }

                        buf.insert(cursor, c);
                        cursor += c.len_utf8();
                        redraw_line(&prompt_str, &buf, cursor, config, exe_cache, &state.history);
                    }
                    _ => {}
                }
            }
            Event::Resize(_, _) => {
                redraw_line(&prompt_str, &buf, cursor, config, exe_cache, &state.history);
            }
            _ => {}
        }
    };

    terminal::disable_raw_mode().ok();
    // Ensure cursor is at column 0 for command output
    print!("\x1b[G");
    io::stdout().flush().ok();
    result
}

/// Find history entries matching a search string (substring match)
fn find_history_matches(query: &str, history: &[String]) -> Vec<String> {
    let mut matches: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for entry in history.iter().rev() {
        if (query.is_empty() || entry.contains(query)) && seen.insert(entry.clone()) {
            matches.push(entry.clone());
            if matches.len() >= 10 {
                break;
            }
        }
    }
    matches
}

/// Draw the history search UI below the prompt
fn draw_history_search(query: &str, matches: &[String], selected: usize, _prompt_width: usize) {
    // Save cursor, move to line below, clear everything below
    let display_count = matches.len().min(5);

    // Move to next line and draw search UI
    print!("\r\n\x1b[K\x1b[38;5;243m(search): \x1b[0m{}", query);

    for (i, m) in matches.iter().take(5).enumerate() {
        print!("\r\n\x1b[K");
        if i == selected {
            print!("\x1b[7m  {}\x1b[0m", m); // Reverse video for selected
        } else {
            print!("  \x1b[38;5;244m{}\x1b[0m", m);
        }
    }

    // Move cursor back up to the search line
    let lines_down = display_count + 1;
    print!("\x1b[{}A", lines_down);
    // Position cursor at end of search query
    print!("\r\x1b[{}C", 10 + query.len()); // "(search): " is 10 chars
    io::stdout().flush().ok();
}

/// Find the Nth match for reverse incremental search
fn find_reverse_search(query: &str, history: &[String], skip: usize) -> Option<String> {
    if query.is_empty() {
        return history.last().cloned();
    }
    let mut count = 0;
    for entry in history.iter().rev() {
        if entry.contains(query) {
            if count == skip {
                return Some(entry.clone());
            }
            count += 1;
        }
    }
    None
}

/// Draw the reverse-i-search prompt
fn draw_reverse_search(query: &str, current_match: &str) {
    print!("\r\x1b[K(reverse-i-search)`{}': {}", query, current_match);
    io::stdout().flush().ok();
}

/// Draw right-aligned prompt info (git dirty/clean, duration)
fn draw_right_prompt(config: &Config, last_cmd_duration: f64) {
    let cols = terminal::size().map(|(c, _)| c as usize).unwrap_or(80);

    let mut parts: Vec<String> = Vec::new();

    // Git dirty/clean indicator
    let git_dir = find_git_dir();
    if !git_dir.is_empty() {
        let is_clean = std::process::Command::new("git")
            .args(["status", "--porcelain"])
            .output()
            .map(|o| o.stdout.is_empty())
            .unwrap_or(true);
        if is_clean {
            parts.push("\x1b[32m●\x1b[0m".to_string()); // green
        } else {
            parts.push("\x1b[31m●\x1b[0m".to_string()); // red
        }
    }

    // Command duration if > 1s
    if last_cmd_duration >= 1.0 {
        if last_cmd_duration >= 60.0 {
            let mins = (last_cmd_duration / 60.0).floor() as u64;
            let secs = (last_cmd_duration % 60.0) as u64;
            parts.push(format!("\x1b[38;5;243m{}m{}s\x1b[0m", mins, secs));
        } else {
            parts.push(format!("\x1b[38;5;243m{:.1}s\x1b[0m", last_cmd_duration));
        }
    }

    if parts.is_empty() {
        return;
    }

    let rprompt = parts.join(" ");
    let visible = strip_ansi_simple(&rprompt);
    let visible_len_rp = visible.len();

    if cols > visible_len_rp + 2 {
        // Save cursor, move to right edge, print, restore cursor
        print!("\x1b[s\x1b[{};{}H{}\x1b[u",
            cursor_row(), cols - visible_len_rp, rprompt);
        io::stdout().flush().ok();
    }
}

/// Get current cursor row (approximate, using terminal query)
fn cursor_row() -> usize {
    // Use crossterm to query position
    if let Ok((_, row)) = crossterm::cursor::position() {
        return (row + 1) as usize;
    }
    1
}

/// Simple ANSI strip for length calculation
fn strip_ansi_simple(s: &str) -> String {
    let mut result = String::new();
    let mut in_escape = false;
    for ch in s.chars() {
        if in_escape {
            if ch.is_ascii_alphabetic() { in_escape = false; }
            continue;
        }
        if ch == '\x1b' { in_escape = true; continue; }
        result.push(ch);
    }
    result
}

/// Find .git directory from cwd upward
fn find_git_dir() -> String {
    let mut dir = std::env::current_dir().unwrap_or_default();
    for _ in 0..10 {
        if dir.join(".git").exists() {
            return dir.to_string_lossy().to_string();
        }
        if !dir.pop() { break; }
    }
    String::new()
}

/// Read continuation lines for multi-line input
fn read_continuation(first_line: &str) -> Option<String> {
    let mut full = first_line.to_string();
    loop {
        print!(" > ");
        io::stdout().flush().ok();
        let mut line = String::new();
        if io::stdin().read_line(&mut line).is_err() {
            return Some(full);
        }
        let trimmed = line.trim_end_matches('\n').trim_end_matches('\r');
        // If previous line ended with \, remove the backslash
        if full.trim_end().ends_with('\\') {
            let len = full.trim_end().len();
            full.truncate(len - 1);
            full.push(' ');
        } else {
            full.push(' ');
        }
        full.push_str(trimmed);
        if !line_needs_continuation(&full) {
            return Some(full);
        }
    }
}

fn redraw_line(prompt: &str, buf: &str, cursor: usize, config: &Config, exe_cache: &[String], history: &[String]) {
    let prompt_width = visible_len(prompt);
    let highlighted = syntax_highlight(buf, config, exe_cache);

    // Show grayed-out history suggestion when cursor is at end
    let suggestion_suffix = if cursor == buf.len() {
        if let Some(suggestion) = find_history_suggestion(buf, history) {
            let rest = &suggestion[buf.len()..];
            format!("\x1b[38;5;{}m{}\x1b[0m", config.c_suggestion, rest)
        } else {
            String::new()
        }
    } else {
        String::new()
    };

    print!("\r\x1b[K{}{}{}", prompt, highlighted, suggestion_suffix);
    // Position cursor
    let col = prompt_width + display_width(&buf[..cursor]);
    set_cursor_col(col);
    io::stdout().flush().ok();
}

fn set_cursor_col(col: usize) {
    print!("\r\x1b[{}C", col);
    io::stdout().flush().ok();
}

/// Syntax highlight the command line (with pipe/operator awareness)
fn syntax_highlight(line: &str, config: &Config, exe_cache: &[String]) -> String {
    if line.is_empty() {
        return String::new();
    }

    // Split on pipe/logical operators, preserving operators
    let segments = split_on_operators(line);
    let mut result = String::new();

    for (segment, operator) in &segments {
        result.push_str(&highlight_segment(segment, config, exe_cache));
        if !operator.is_empty() {
            result.push_str(operator);
        }
    }

    result
}

/// Split line on |, &&, || preserving operators
fn split_on_operators(line: &str) -> Vec<(String, String)> {
    let mut segments: Vec<(String, String)> = Vec::new();
    let mut current = String::new();
    let mut chars = line.chars().peekable();
    let mut in_single = false;
    let mut in_double = false;
    let mut escape = false;

    while let Some(ch) = chars.next() {
        if escape { current.push(ch); escape = false; continue; }
        if ch == '\\' { current.push(ch); escape = true; continue; }
        if ch == '\'' && !in_double { in_single = !in_single; current.push(ch); continue; }
        if ch == '"' && !in_single { in_double = !in_double; current.push(ch); continue; }
        if in_single || in_double { current.push(ch); continue; }

        if ch == '|' {
            if chars.peek() == Some(&'|') {
                chars.next();
                segments.push((current.clone(), "||".to_string()));
                current.clear();
                continue;
            }
            segments.push((current.clone(), "|".to_string()));
            current.clear();
            continue;
        }
        if ch == '&' {
            if chars.peek() == Some(&'&') {
                chars.next();
                segments.push((current.clone(), "&&".to_string()));
                current.clear();
                continue;
            }
        }
        current.push(ch);
    }
    segments.push((current, String::new()));
    segments
}

/// Highlight a single command segment
fn highlight_segment(segment: &str, config: &Config, exe_cache: &[String]) -> String {
    let trimmed = segment.trim_start();
    if trimmed.is_empty() {
        return segment.to_string();
    }

    let leading_ws = &segment[..segment.len() - trimmed.len()];
    let parts = shell_split(trimmed);
    if parts.is_empty() {
        return segment.to_string();
    }

    let cmd = &parts[0];
    let ls_colors = parse_ls_colors();
    const BUILTINS: &[&str] = &["cd", "exit", "quit", "export", "unset", "pushd", "popd"];

    let cmd_color = if cmd.starts_with(':') {
        config.c_colon
    } else if cmd.starts_with('@') {
        config.c_colon
    } else if config.nick.contains_key(cmd.as_str()) {
        config.c_nick
    } else if config.gnick.contains_key(cmd.as_str()) {
        config.c_gnick
    } else if config.bookmarks.contains_key(cmd.as_str()) {
        config.c_bookmark
    } else if BUILTINS.contains(&cmd.as_str()) {
        config.c_cmd
    } else if exe_cache.binary_search(cmd).is_ok() || std::path::Path::new(cmd).exists() {
        config.c_cmd
    } else {
        196
    };

    let cmd_end = trimmed.find(' ').unwrap_or(trimmed.len());
    let mut result = format!("{}\x1b[38;5;{}m{}\x1b[0m", leading_ws, cmd_color, &trimmed[..cmd_end]);

    if cmd_end < trimmed.len() {
        let rest = &trimmed[cmd_end..];
        let mut colored_rest = String::new();
        for word in rest.split(' ') {
            if word.is_empty() {
                colored_rest.push(' ');
                continue;
            }
            if word.starts_with('-') {
                colored_rest.push_str(&format!("\x1b[38;5;{}m{}\x1b[0m", config.c_switch, word));
            } else {
                let expanded = if word.starts_with('~') {
                    let home = dirs::home_dir().unwrap_or_default().to_string_lossy().to_string();
                    word.replacen('~', &home, 1)
                } else {
                    word.to_string()
                };
                let p = std::path::Path::new(&expanded);
                if p.exists() || p.is_symlink() {
                    let color_code = ls_color_for(&expanded, &ls_colors);
                    if !color_code.is_empty() {
                        colored_rest.push_str(&format!("{}{}\x1b[0m", color_code, word));
                    } else {
                        colored_rest.push_str(&format!("\x1b[38;5;{}m{}\x1b[0m", config.c_path, word));
                    }
                } else {
                    colored_rest.push_str(word);
                }
            }
            colored_rest.push(' ');
        }
        if colored_rest.ends_with(' ') && !rest.ends_with(' ') {
            colored_rest.pop();
        }
        result.push_str(&colored_rest);
    }

    result
}

/// Tab completion with learning weights
/// Draw completions below the prompt with LS_COLORS, selected item in reverse
fn draw_completions(completions: &[String], selected: usize, ls_colors: &HashMap<String, String>, prompt: &str) {
    let cols = terminal::size().map(|(c, _)| c as usize).unwrap_or(80);
    // Move to line below prompt and clear
    print!("\r\n\x1b[K");
    let mut col = 0;
    for (i, m) in completions.iter().enumerate() {
        let color = ls_color_for(m, ls_colors);
        let display = if i == selected {
            format!("\x1b[7m{}{}\x1b[0m", color, m)
        } else {
            format!("{}{}\x1b[0m", color, m)
        };
        let width = m.len() + 2;
        if col + width > cols && col > 0 {
            print!("\r\n\x1b[K");
            col = 0;
        }
        print!("{}  ", display);
        col += width;
    }
    // Move cursor back up to prompt line
    let lines_used = 1 + col / cols.max(1);
    let total_items_width: usize = completions.iter().map(|m| m.len() + 2).sum();
    let display_lines = (total_items_width + cols - 1) / cols.max(1);
    let display_lines = display_lines.max(1);
    print!("\x1b[{}A\r", display_lines);
    io::stdout().flush().ok();
}

/// Clear the completion display area
fn clear_completions(count: usize) {
    let cols = terminal::size().map(|(c, _)| c as usize).unwrap_or(80);
    let total_width: usize = count * 15; // rough estimate
    let lines = (total_width / cols.max(1)).max(1) + 1;
    print!("\r\n");
    for _ in 0..lines {
        print!("\x1b[K\r\n");
    }
    // Move back up
    print!("\x1b[{}A", lines + 1);
    io::stdout().flush().ok();
}

/// Gather completion candidates (returns matches + word_start position)
fn gather_completions(buf: &str, cursor: usize, exe_cache: &[String], config: &Config, weights: &HashMap<String, usize>) -> (Vec<String>, usize) {
    let prefix = &buf[..cursor];
    let parts: Vec<&str> = prefix.split_whitespace().collect();

    // Environment variable completion: $PREFIX<TAB>
    let current_word = if prefix.ends_with(' ') { "" } else { parts.last().copied().unwrap_or("") };
    if current_word.starts_with('$') {
        let var_prefix = &current_word[1..]; // strip $
        let ws = prefix.rfind(' ').map(|i| i + 1).unwrap_or(0);
        let mut matches: Vec<String> = std::env::vars()
            .filter(|(k, _)| k.starts_with(var_prefix))
            .map(|(k, _)| format!("${}", k))
            .collect();
        matches.sort();
        matches.truncate(config.completion_limit);
        return (matches, ws);
    }

    if parts.is_empty() || (parts.len() == 1 && !prefix.ends_with(' ')) {
        // Complete command
        let word = parts.first().copied().unwrap_or("");
        let mut matches: Vec<String> = exe_cache
            .iter()
            .filter(|e| e.starts_with(word))
            .map(|s| s.to_string())
            .collect();
        // Also check nicks, bookmarks, and colon commands
        for k in config.nick.keys() {
            if k.starts_with(word) && !matches.contains(k) {
                matches.push(k.clone());
            }
        }
        for k in config.bookmarks.keys() {
            if k.starts_with(word) && !matches.contains(k) {
                matches.push(k.clone());
            }
        }
        // Colon commands
        if word.starts_with(':') {
            let colon_cmds = [
                ":nick", ":gnick", ":abbrev", ":bm", ":bookmark", ":dirs", ":history", ":rmhistory",
                ":rehash", ":theme", ":calc", ":stats", ":jobs", ":fg",
                ":env", ":config", ":validate", ":save_session", ":load_session",
                ":list_sessions", ":delete_session", ":record", ":replay",
                ":abbrev", ":version", ":info", ":help",
            ];
            for cmd in &colon_cmds {
                if cmd.starts_with(word) && !matches.iter().any(|m| m == cmd) {
                    matches.push(cmd.to_string());
                }
            }
        }
        // Also check files/dirs in current directory (for auto-cd)
        if let Ok(entries) = std::fs::read_dir(".") {
            for entry in entries.flatten() {
                if let Some(name) = entry.file_name().to_str() {
                    if name.starts_with(word) && !matches.contains(&name.to_string())
                        && (word.starts_with('.') || !name.starts_with('.'))
                    {
                        let mut completion = name.to_string();
                        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                            completion.push('/');
                        }
                        matches.push(completion);
                    }
                }
            }
        }
        // Sort by completion weight (most used first), then alphabetically
        matches.sort_by(|a, b| {
            let wa = weights.get(a).copied().unwrap_or(0);
            let wb = weights.get(b).copied().unwrap_or(0);
            wb.cmp(&wa).then(a.cmp(b))
        });
        matches.truncate(config.completion_limit);
        return (matches, prefix.rfind(' ').map(|i| i + 1).unwrap_or(0));
    } else {
        // Smart command-specific completions
        let cmd = parts[0];
        let word = if prefix.ends_with(' ') { "" } else { parts.last().copied().unwrap_or("") };
        let ws = prefix.rfind(' ').map(|i| i + 1).unwrap_or(0);

        // Try smart completions for known commands
        if parts.len() == 2 || (parts.len() == 1 && prefix.ends_with(' ')) {
            let sub_prefix = if prefix.ends_with(' ') { "" } else { word };
            let smart = smart_completions(cmd, sub_prefix);
            if !smart.is_empty() {
                return (smart, ws);
            }
        }

        // Complete switches from --help
        if word.starts_with('-') {
            let switches = get_switches(cmd);
            let mut matches: Vec<String> = switches.iter()
                .filter(|s| s.starts_with(word))
                .cloned()
                .collect();
            if !matches.is_empty() {
                matches.sort();
                matches.truncate(config.completion_limit);
                return (matches, ws);
            }
        }

        // Complete file/directory
        let expanded = if word.starts_with('~') {
            let home = dirs::home_dir().unwrap_or_default().to_string_lossy().to_string();
            word.replacen('~', &home, 1)
        } else {
            word.to_string()
        };

        let (dir, file_prefix) = if expanded.contains('/') {
            let idx = expanded.rfind('/').unwrap();
            (&expanded[..=idx], &expanded[idx + 1..])
        } else {
            ("./", expanded.as_str())
        };

        let mut matches = Vec::new();
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                if let Some(name) = entry.file_name().to_str() {
                    if name.starts_with(file_prefix) && (file_prefix.starts_with('.') || !name.starts_with('.')) {
                        let mut completion = if dir == "./" {
                            name.to_string()
                        } else {
                            format!("{}{}", dir, name)
                        };
                        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                            completion.push('/');
                        }
                        matches.push(completion);
                    }
                }
            }
        }
        matches.sort();
        matches.truncate(config.completion_limit);
        (matches, ws)
    }
}

fn common_prefix(strings: &[String]) -> String {
    if strings.is_empty() {
        return String::new();
    }
    let first = &strings[0];
    let mut len = first.len();
    for s in &strings[1..] {
        len = len.min(s.len());
        for (i, (a, b)) in first.chars().zip(s.chars()).enumerate() {
            if a != b {
                len = len.min(i);
                break;
            }
        }
    }
    first[..len].to_string()
}

fn display_width(s: &str) -> usize {
    use unicode_width::UnicodeWidthStr;
    // Strip ANSI codes for width calculation
    let stripped: String = strip_ansi(s);
    UnicodeWidthStr::width(stripped.as_str())
}

fn visible_len(s: &str) -> usize {
    display_width(s)
}

fn strip_ansi(s: &str) -> String {
    let mut result = String::new();
    let mut in_escape = false;
    let mut in_csi = false;
    let mut in_osc = false;
    for ch in s.chars() {
        if in_osc {
            // OSC sequences end with BEL (\x07) or ST (\x1b\\)
            if ch == '\x07' {
                in_osc = false;
            }
            continue;
        }
        if in_escape {
            if ch == '[' {
                in_csi = true;
                in_escape = false;
                continue;
            }
            if ch == ']' {
                in_osc = true;
                in_escape = false;
                continue;
            }
            in_escape = false;
            continue;
        }
        if in_csi {
            if ch.is_ascii_alphabetic() {
                in_csi = false;
            }
            continue;
        }
        if ch == '\x1b' {
            in_escape = true;
            continue;
        }
        result.push(ch);
    }
    result
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn prev_char_boundary(s: &str, pos: usize) -> usize {
    let mut p = pos.saturating_sub(1);
    while p > 0 && !s.is_char_boundary(p) {
        p -= 1;
    }
    p
}

fn next_char_boundary(s: &str, pos: usize) -> usize {
    let mut p = pos + 1;
    while p < s.len() && !s.is_char_boundary(p) {
        p += 1;
    }
    p
}
