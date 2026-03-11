# AGENTS.md

## Project Overview

Anvl is a terminal-based multi-workspace manager built in Rust. It provides embedded terminal sessions (agent + shell tabs) with Git integration, attention detection, and session persistence via a daemon/attach model.

## Architecture

Cargo workspace with three crates:

- **`crates/protocol`** — Serializable IPC types: workspace routing, attention levels, terminal kinds, command/event enums. Pure data types with serde, no business logic.
- **`crates/core`** (`anvl_core`) — Application state: workspaces, Git operations, PTY spawning, attention detection, SSH, and the async event loop. Uses `portable-pty` for terminal management and `tokio` for async.
- **`crates/tui`** — Terminal UI binary (`anvl`). Built with Ratatui/crossterm. Renders home and workspace screens, handles keyboard/mouse input, manages sessions. Contains `ui/` (rendering) and `keymap.rs` (input handling).

Dependency chain: `tui` → `core` → `protocol`

## Build & Run

```sh
cargo build --release
cargo run              # runs the TUI binary
```

No test suite currently. Verify changes by building and running the TUI manually.

## Code Conventions

- Rust 2021 edition, stable toolchain
- Error handling: `anyhow` for application errors, `thiserror` for library error types in core
- Async runtime: `tokio` multi-threaded
- Locking: `parking_lot` (not std mutexes) in core
- Terminal parsing: `vt100` crate in the TUI
- IDs: `uuid` v4 for workspace and session identifiers
- Keep `protocol` free of business logic — it is shared types only

## SSH-First Behavior

- Treat SSH-backed workspaces as first-class: when adding or changing features, attempt the SSH path, not just local workspaces.
- Prefer shared code paths that accept `Option<&SshTarget>` or equivalent rather than separate local-only implementations when practical.
- If a feature cannot support SSH yet, document the gap clearly in code and the user-facing behavior instead of silently falling back to local-only behavior.

## Key Files

| Path | Purpose |
|---|---|
| `crates/tui/src/main.rs` | Entry point, session management, daemon logic |
| `crates/tui/src/app.rs` | Main app state and event loop |
| `crates/tui/src/keymap.rs` | Input handling and key bindings |
| `crates/tui/src/ui/` | UI rendering (screens, widgets, footer) |
| `crates/core/src/state.rs` | Central application state |
| `crates/core/src/workspace/` | Workspace management (git, terminal, attention, SSH) |
| `crates/protocol/src/lib.rs` | Shared types (commands, events, enums) |

## Configuration

Config lives under `~/.config/anvl/` (respects `XDG_CONFIG_HOME`):
- `sessions.json` — session registry
- `workspaces.json` — default workspace persistence
- `workspaces.<session-name>.json` — per-session workspace state

## Environment Variables

| Variable | Description | Default |
|---|---|---|
| `ANVL_WEB_PORT` | Embedded web server port | `3001` |
| `ANVL_DISABLE_EMBEDDED_WEB` | Disable embedded web server | — |
| `ANVL_SESSION_NAME` | Passed to daemon subprocess | — |
| `SHELL` | Shell for terminal sessions | `zsh` |
