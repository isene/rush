use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal;
use std::io::{self, Write};

use crate::config::{Config, State};
use crate::execute::{build_exe_cache, shell_split};
use crate::prompt;

/// Read a line of input with editing, history, tab completion, syntax highlighting
pub fn getline(
    config: &Config,
    state: &mut State,
    exe_cache: &[String],
) -> Option<String> {
    let prompt_str = prompt::build_prompt(config.c_prompt);
    print!("{}", prompt_str);
    io::stdout().flush().ok();

    let prompt_width = visible_len(&prompt_str);
    let mut buf = String::new();
    let mut cursor = 0usize; // byte position
    let mut hist_pos: Option<usize> = None;
    let mut saved_buf = String::new();

    terminal::enable_raw_mode().ok();

    let result = loop {
        let ev = match event::read() {
            Ok(ev) => ev,
            Err(_) => {
                std::thread::sleep(std::time::Duration::from_millis(10));
                continue;
            }
        };

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
                        redraw_line(&prompt_str, &buf, cursor, config, exe_cache);
                    }
                    // Enter: execute
                    (KeyCode::Enter, _) => {
                        println!();
                        break Some(buf);
                    }
                    // Tab: completion
                    (KeyCode::Tab, _) => {
                        if let Some(completed) = complete(&buf, cursor, exe_cache, config) {
                            buf = completed;
                            cursor = buf.len();
                            redraw_line(&prompt_str, &buf, cursor, config, exe_cache);
                        }
                    }
                    // Backspace
                    (KeyCode::Backspace, _) => {
                        if cursor > 0 {
                            // Find previous char boundary
                            let prev = prev_char_boundary(&buf, cursor);
                            buf.drain(prev..cursor);
                            cursor = prev;
                            redraw_line(&prompt_str, &buf, cursor, config, exe_cache);
                        }
                    }
                    // Delete
                    (KeyCode::Delete, _) => {
                        if cursor < buf.len() {
                            let next = next_char_boundary(&buf, cursor);
                            buf.drain(cursor..next);
                            redraw_line(&prompt_str, &buf, cursor, config, exe_cache);
                        }
                    }
                    // Left
                    (KeyCode::Left, _) => {
                        if cursor > 0 {
                            cursor = prev_char_boundary(&buf, cursor);
                            set_cursor_col(prompt_width + display_width(&buf[..cursor]));
                        }
                    }
                    // Right
                    (KeyCode::Right, _) => {
                        if cursor < buf.len() {
                            cursor = next_char_boundary(&buf, cursor);
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
                            redraw_line(&prompt_str, &buf, cursor, config, exe_cache);
                        }
                    }
                    // Down: history
                    (KeyCode::Down, _) => {
                        match hist_pos {
                            Some(0) => {
                                hist_pos = None;
                                buf = saved_buf.clone();
                                cursor = buf.len();
                                redraw_line(&prompt_str, &buf, cursor, config, exe_cache);
                            }
                            Some(p) => {
                                hist_pos = Some(p - 1);
                                buf = state.history[state.history.len() - p].clone();
                                cursor = buf.len();
                                redraw_line(&prompt_str, &buf, cursor, config, exe_cache);
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
                            redraw_line(&prompt_str, &buf, cursor, config, exe_cache);
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
                        redraw_line(&prompt_str, &buf, cursor, config, exe_cache);
                    }
                    // Ctrl-U: kill to beginning
                    (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
                        buf.drain(..cursor);
                        cursor = 0;
                        redraw_line(&prompt_str, &buf, cursor, config, exe_cache);
                    }
                    // Regular char
                    (KeyCode::Char(c), _) => {
                        buf.insert(cursor, c);
                        cursor += c.len_utf8();
                        redraw_line(&prompt_str, &buf, cursor, config, exe_cache);
                    }
                    _ => {}
                }
            }
            Event::Resize(_, _) => {
                redraw_line(&prompt_str, &buf, cursor, config, exe_cache);
            }
            _ => {}
        }
    };

    terminal::disable_raw_mode().ok();
    result
}

fn redraw_line(prompt: &str, buf: &str, cursor: usize, config: &Config, exe_cache: &[String]) {
    let prompt_width = visible_len(prompt);
    let highlighted = syntax_highlight(buf, config, exe_cache);
    print!("\r\x1b[K{}{}", prompt, highlighted);
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

/// Tab completion
fn complete(buf: &str, cursor: usize, exe_cache: &[String], config: &Config) -> Option<String> {
    let prefix = &buf[..cursor];
    let parts: Vec<&str> = prefix.split_whitespace().collect();

    let (completions, word_start) = if parts.is_empty() || (parts.len() == 1 && !prefix.ends_with(' ')) {
        // Complete command
        let word = parts.first().copied().unwrap_or("");
        let mut matches: Vec<&str> = exe_cache
            .iter()
            .filter(|e| e.starts_with(word))
            .map(|s| s.as_str())
            .collect();
        // Also check nicks and bookmarks
        for k in config.nick.keys() {
            if k.starts_with(word) && !matches.contains(&k.as_str()) {
                matches.push(k.as_str());
            }
        }
        for k in config.bookmarks.keys() {
            if k.starts_with(word) && !matches.contains(&k.as_str()) {
                matches.push(k.as_str());
            }
        }
        matches.sort();
        matches.truncate(config.completion_limit);
        (matches.iter().map(|s| s.to_string()).collect::<Vec<_>>(), prefix.rfind(' ').map(|i| i + 1).unwrap_or(0))
    } else {
        // Complete file/directory
        let word = if prefix.ends_with(' ') { "" } else { parts.last().copied().unwrap_or("") };
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
        (matches, prefix.rfind(' ').map(|i| i + 1).unwrap_or(0))
    };

    if completions.is_empty() {
        return None;
    }

    if completions.len() == 1 {
        let mut new_buf = buf[..word_start].to_string();
        new_buf.push_str(&completions[0]);
        if !completions[0].ends_with('/') {
            new_buf.push(' ');
        }
        // Preserve anything after cursor
        if cursor < buf.len() {
            new_buf.push_str(&buf[cursor..]);
        }
        return Some(new_buf);
    }

    // Multiple matches: show them
    println!();
    for (i, m) in completions.iter().enumerate() {
        if std::path::Path::new(m).is_dir() || m.ends_with('/') {
            print!("\x1b[38;5;12m{}\x1b[0m  ", m);
        } else {
            print!("{}  ", m);
        }
        if (i + 1) % 5 == 0 {
            println!();
        }
    }
    println!();

    // Find common prefix
    let common = common_prefix(&completions);
    if common.len() > buf[word_start..cursor].len() {
        let mut new_buf = buf[..word_start].to_string();
        new_buf.push_str(&common);
        if cursor < buf.len() {
            new_buf.push_str(&buf[cursor..]);
        }
        return Some(new_buf);
    }

    None
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
    for ch in s.chars() {
        if in_escape {
            if ch == '[' {
                in_csi = true;
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
