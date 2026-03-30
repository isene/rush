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

/// Read a line of input with editing, history, tab completion, syntax highlighting
pub fn getline(
    config: &Config,
    state: &mut State,
    exe_cache: &[String],
) -> Option<String> {
    let prompt_str = prompt::build_prompt(config.c_prompt);
    print!("\r{}", prompt_str);
    io::stdout().flush().ok();

    let prompt_width = visible_len(&prompt_str);
    let mut buf = String::new();
    let mut cursor = 0usize; // byte position
    let mut hist_pos: Option<usize> = None;
    let mut saved_buf = String::new();

    // History search state
    let mut history_search_active = false;
    let mut history_search_buf = String::new();
    let mut history_search_matches: Vec<String> = Vec::new();
    let mut history_search_index: usize = 0;

    terminal::enable_raw_mode().ok();

    let result = loop {
        let ev = match event::read() {
            Ok(ev) => ev,
            Err(_) => {
                std::thread::sleep(std::time::Duration::from_millis(10));
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

        match ev {
            Event::Key(KeyEvent { code, modifiers, .. }) => {
                match (code, modifiers) {
                    // Ctrl-C: clear line
                    (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                        println!("^C");
                        break Some(String::new());
                    }
                    // Ctrl-D: exit
                    (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
                        if buf.is_empty() {
                            println!();
                            break None; // Signal exit
                        }
                    }
                    // Ctrl-L: clear screen
                    (KeyCode::Char('l'), KeyModifiers::CONTROL) => {
                        print!("\x1b[2J\x1b[H{}", prompt_str);
                        redraw_line(&prompt_str, &buf, cursor, config, exe_cache, &state.history);
                    }
                    // Enter: execute
                    (KeyCode::Enter, _) => {
                        println!();
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
                            // Find previous char boundary
                            let prev = prev_char_boundary(&buf, cursor);
                            buf.drain(prev..cursor);
                            cursor = prev;
                            redraw_line(&prompt_str, &buf, cursor, config, exe_cache, &state.history);
                        }
                    }
                    // Delete
                    (KeyCode::Delete, _) => {
                        if cursor < buf.len() {
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
                        buf.truncate(cursor);
                        redraw_line(&prompt_str, &buf, cursor, config, exe_cache, &state.history);
                    }
                    // Ctrl-U: kill to beginning
                    (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
                        buf.drain(..cursor);
                        cursor = 0;
                        redraw_line(&prompt_str, &buf, cursor, config, exe_cache, &state.history);
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
                    // Regular char
                    (KeyCode::Char(c), _) => {
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

/// Syntax highlight the command line
fn syntax_highlight(line: &str, config: &Config, exe_cache: &[String]) -> String {
    if line.is_empty() {
        return String::new();
    }

    let parts = shell_split(line);
    if parts.is_empty() {
        return line.to_string();
    }

    let cmd = &parts[0];

    // Determine command color
    let cmd_color = if cmd.starts_with(':') {
        config.c_colon
    } else if config.nick.contains_key(cmd.as_str()) {
        config.c_nick
    } else if config.gnick.contains_key(cmd.as_str()) {
        config.c_gnick
    } else if config.bookmarks.contains_key(cmd.as_str()) {
        config.c_bookmark
    } else if exe_cache.binary_search(cmd).is_ok() || std::path::Path::new(cmd).exists() {
        config.c_cmd
    } else {
        196 // Red for unknown
    };

    // Color the command part
    let cmd_end = line.find(' ').unwrap_or(line.len());
    let mut result = format!("\x1b[38;5;{}m{}\x1b[0m", cmd_color, &line[..cmd_end]);

    // Color remaining arguments
    if cmd_end < line.len() {
        let rest = &line[cmd_end..];
        let mut colored_rest = String::new();
        for word in rest.split(' ') {
            if word.starts_with('-') {
                colored_rest.push_str(&format!("\x1b[38;5;{}m{}\x1b[0m", config.c_switch, word));
            } else if std::path::Path::new(word).exists() {
                let c = if std::path::Path::new(word).is_dir() {
                    config.c_dir
                } else {
                    config.c_path
                };
                colored_rest.push_str(&format!("\x1b[38;5;{}m{}\x1b[0m", c, word));
            } else {
                colored_rest.push_str(word);
            }
            colored_rest.push(' ');
        }
        // Remove trailing space, preserve original spacing
        if !colored_rest.is_empty() {
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

    if parts.is_empty() || (parts.len() == 1 && !prefix.ends_with(' ')) {
        // Complete command
        let word = parts.first().copied().unwrap_or("");
        let mut matches: Vec<String> = exe_cache
            .iter()
            .filter(|e| e.starts_with(word))
            .map(|s| s.to_string())
            .collect();
        // Also check nicks and bookmarks
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
