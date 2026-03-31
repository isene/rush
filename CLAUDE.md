# Rush - Claude Code Instructions

## Build
```bash
PATH="/usr/bin:$PATH" cargo build --release
```
The PATH prefix is required because ~/bin/cc (Claude Code sessions script) shadows the C compiler.

## Release Process
1. Update version in `Cargo.toml` and `src/main.rs` (if version constant exists)
2. Commit and push
3. Tag: `git tag vX.Y.Z && git push --tags`
4. GitHub Actions automatically builds binaries for 4 platforms
5. Release appears at https://github.com/isene/rush/releases

## Key Facts
- User's login shell (registered in /etc/shells, set via chsh)
- Wezterm config: `config.default_prog = { 'rush' }` in ~/.config/wezterm/wezterm.lua
- Config: ~/.rushrc.json (JSON), State: ~/.rushstate.json
- Binary symlinked: ~/bin/rush -> target/release/rush
- LS_COLORS sourced from ~/.local/share/lscolors.sh on startup
- Plugin API: ~/.rush/plugins/ (language-agnostic, JSON stdin/stdout)

## Part of Fe2O3
Rush is part of the Fe2O3 Rust Terminal Suite. Libraries (crust, glow, plot) are separate crates. Apps compile to single static binaries.

## Testing
- After changes: `PATH="/usr/bin:$PATH" cargo check` (fast)
- Full build: `PATH="/usr/bin:$PATH" cargo build --release` (~30s)
- The user uses rush as their daily shell, so don't break existing features
