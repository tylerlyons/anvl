mod app;
mod keymap;
mod ui;

use std::io::Write as _;
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command as OsCommand, Stdio};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use app::TuiApp;
use base64::Engine as _;
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    },
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use futures::{SinkExt, StreamExt};
use anvl_core::{spawn_core, CoreHandle};
use protocol::{AttentionLevel, Command, Event as CoreEvent, Route, TerminalKind};
use ratatui::{backend::CrosstermBackend, Terminal};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};

#[derive(Debug)]
enum LaunchMode {
    Local,
    CreateSession { name: String },
    AttachSession { name: String },
    RemoveSession { name: String },
    ListSessions,
    RunDaemon { name: Option<String>, port: u16 },
}

#[derive(Debug)]
struct Cli {
    mode: LaunchMode,
    detach: bool,
}

struct Backend {
    cmd_tx: mpsc::Sender<Command>,
    evt_rx: mpsc::Receiver<CoreEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionEntry {
    name: String,
    port: u16,
    pid: u32,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct SessionRegistry {
    sessions: Vec<SessionEntry>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = parse_cli(std::env::args().skip(1).collect::<Vec<_>>())?;
    match cli.mode {
        LaunchMode::RunDaemon { name, port } => run_daemon(name, port).await,
        LaunchMode::RemoveSession { name } => delete_session(&name),
        LaunchMode::ListSessions => list_sessions(),
        LaunchMode::CreateSession { name } => {
            let entry = ensure_session_running(&name).await?;
            if cli.detach {
                println!(
                    "session '{}' running in background on port {} (detached)",
                    entry.name, entry.port
                );
                return Ok(());
            }
            let backend = build_remote_backend(entry.port).await?;
            run_tui(backend).await
        }
        LaunchMode::AttachSession { name } => {
            let entry = get_session(&name)?.ok_or_else(|| {
                anyhow!(
                    "session '{}' not found. create it with: anvl -s {}",
                    name,
                    name
                )
            })?;
            if !port_open(entry.port) {
                return Err(anyhow!(
                    "session '{}' exists but is not reachable on port {}",
                    name,
                    entry.port
                ));
            }
            if cli.detach {
                println!(
                    "session '{}' is running on port {} (detached)",
                    entry.name, entry.port
                );
                return Ok(());
            }
            let backend = build_remote_backend(entry.port).await?;
            run_tui(backend).await
        }
        LaunchMode::Local => {
            if cli.detach {
                return Err(anyhow!(
                    "--detach requires a named session: use `anvl -s <name> -d` or `anvl -a <name> -d`"
                ));
            }
            let (backend, core) = build_local_backend();
            let web_port = std::env::var("ANVL_WEB_PORT")
                .ok()
                .and_then(|v| v.parse::<u16>().ok())
                .unwrap_or(3001);
            if std::env::var("ANVL_DISABLE_EMBEDDED_WEB").is_err() {
                let core_for_web = core.clone();
                tokio::spawn(async move {
                    let _ = server::run_server_with_core(core_for_web, web_port).await;
                });
            }
            run_tui(backend).await
        }
    }
}

fn parse_cli(args: Vec<String>) -> Result<Cli> {
    let mut i = 0usize;
    let mut mode = LaunchMode::Local;
    let mut detach = false;
    let mut daemon_port: Option<u16> = None;
    let mut daemon_name: Option<String> = None;

    while i < args.len() {
        match args[i].as_str() {
            "-s" | "--session" => {
                let Some(name) = args.get(i + 1).cloned() else {
                    return Err(anyhow!("missing session name for {}", args[i]));
                };
                mode = LaunchMode::CreateSession { name };
                i += 2;
            }
            "-a" => {
                let Some(name) = args.get(i + 1).cloned() else {
                    return Err(anyhow!("missing session name for -a"));
                };
                mode = LaunchMode::AttachSession { name };
                i += 2;
            }
            "-r" | "--remove" => {
                let Some(name) = args.get(i + 1).cloned() else {
                    return Err(anyhow!("missing session name for {}", args[i]));
                };
                mode = LaunchMode::RemoveSession { name };
                i += 2;
            }
            "-d" | "--detach" => {
                detach = true;
                i += 1;
            }
            "-l" | "--list" => {
                mode = LaunchMode::ListSessions;
                i += 1;
            }
            "--run-daemon" => {
                mode = LaunchMode::RunDaemon {
                    name: None,
                    port: 3001,
                };
                i += 1;
            }
            "--session-name" => {
                let Some(name) = args.get(i + 1).cloned() else {
                    return Err(anyhow!("missing name for --session-name"));
                };
                daemon_name = Some(name);
                i += 2;
            }
            "--port" => {
                let Some(v) = args.get(i + 1) else {
                    return Err(anyhow!("missing port for --port"));
                };
                daemon_port = Some(
                    v.parse::<u16>()
                        .with_context(|| format!("invalid port '{}': expected number", v))?,
                );
                i += 2;
            }
            other => {
                return Err(anyhow!("unknown argument: {other}"));
            }
        }
    }

    if matches!(mode, LaunchMode::RunDaemon { .. }) {
        return Ok(Cli {
            mode: LaunchMode::RunDaemon {
                name: daemon_name,
                port: daemon_port.unwrap_or(3001),
            },
            detach,
        });
    }

    if detach
        && matches!(
            mode,
            LaunchMode::RemoveSession { .. } | LaunchMode::ListSessions
        )
    {
        return Err(anyhow!(
            "--detach is only valid with session create/attach (-s or -a)"
        ));
    }

    Ok(Cli { mode, detach })
}

async fn run_daemon(name: Option<String>, port: u16) -> Result<()> {
    if let Some(session_name) = name {
        std::env::set_var("ANVL_SESSION_NAME", session_name);
    } else {
        std::env::remove_var("ANVL_SESSION_NAME");
    }
    let core = spawn_core();
    server::run_server_with_core(core, port).await
}

fn build_local_backend() -> (Backend, CoreHandle) {
    let core = spawn_core();
    let cmd_tx = core.cmd_tx.clone();

    let (evt_tx, evt_rx) = mpsc::channel::<CoreEvent>(1024);
    let mut broadcast_rx = core.evt_tx.subscribe();
    tokio::spawn(async move {
        loop {
            match broadcast_rx.recv().await {
                Ok(evt) => {
                    if evt_tx.send(evt).await.is_err() {
                        break;
                    }
                }
                Err(RecvError::Closed) => break,
                Err(RecvError::Lagged(_)) => continue,
            }
        }
    });

    (Backend { cmd_tx, evt_rx }, core)
}

async fn build_remote_backend(port: u16) -> Result<Backend> {
    let ws_url = format!("ws://127.0.0.1:{port}/ws");
    let (ws_stream, _) = connect_async(&ws_url)
        .await
        .with_context(|| format!("failed to connect websocket at {ws_url}"))?;
    let (mut ws_write, mut ws_read) = ws_stream.split();

    let (cmd_tx, mut cmd_rx) = mpsc::channel::<Command>(1024);
    let (evt_tx, evt_rx) = mpsc::channel::<CoreEvent>(1024);

    tokio::spawn(async move {
        loop {
            tokio::select! {
                maybe_cmd = cmd_rx.recv() => {
                    let Some(cmd) = maybe_cmd else { break; };
                    let Ok(payload) = serde_json::to_string(&cmd) else { continue; };
                    if ws_write.send(Message::Text(payload.into())).await.is_err() {
                        break;
                    }
                }
                maybe_msg = ws_read.next() => {
                    let Some(msg_res) = maybe_msg else { break; };
                    let Ok(msg) = msg_res else { break; };
                    match msg {
                        Message::Text(txt) => {
                            if let Ok(evt) = serde_json::from_str::<CoreEvent>(&txt) {
                                if evt_tx.send(evt).await.is_err() {
                                    break;
                                }
                            }
                        }
                        Message::Binary(_) => {}
                        Message::Ping(_) => {}
                        Message::Pong(_) => {}
                        Message::Close(_) => break,
                        Message::Frame(_) => {}
                    }
                }
            }
        }
    });

    Ok(Backend { cmd_tx, evt_rx })
}

async fn run_tui(mut backend: Backend) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    stdout.execute(EnterAlternateScreen)?;
    stdout.execute(EnableMouseCapture)?;

    let backend_term = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend_term)?;
    let mut app = TuiApp::default();
    let mut last_flash_toggle = Instant::now();

    loop {
        loop {
            match backend.evt_rx.try_recv() {
                Ok(evt) => apply_event(&mut app, evt),
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => break,
            }
        }

        if let Route::Workspace { id } = app.route {
            if let Ok(size) = terminal.size() {
                let area = ratatui::layout::Rect::new(0, 0, size.width, size.height);
                let inner = ui::screens::workspace::terminal_content_rect(area, app.focus);
                let cols = inner.width.max(1);
                let rows = inner.height.max(1);
                let tid = app.active_tab_id();
                let kind = app.active_tab_kind();
                if app.has_terminal_tab(id, &tid) && app.should_send_resize(id, &tid, cols, rows) {
                    app.resize_terminal_parser(id, &tid, cols, rows);
                    let _ = backend
                        .cmd_tx
                        .send(Command::ResizeTerminal {
                            id,
                            kind,
                            tab_id: Some(tid),
                            cols,
                            rows,
                        })
                        .await;
                }
            }
        }

        terminal.draw(|frame| match app.route {
            Route::Home => ui::screens::home::render(frame, frame.area(), &app),
            Route::Workspace { .. } => ui::screens::workspace::render(frame, frame.area(), &app),
        })?;

        if event::poll(Duration::from_millis(16))? {
            match event::read()? {
                Event::Key(key) => {
                    if matches!(key.kind, KeyEventKind::Release) {
                        continue;
                    }

                    if keymap::is_quit(key)
                        && !app.is_adding_workspace()
                        && !app.is_confirming_delete()
                        && !app.is_renaming_workspace()
                        && !app.is_renaming_tab()
                        && !app.is_committing()
                        && !app.is_creating_branch()
                        && !app.is_settings_open()
                        && !matches!(app.focus, app::Focus::WsTerminal)
                    {
                        break;
                    }

                    match app.route {
                        Route::Home => {
                            if app.is_settings_open() {
                                match key.code {
                                    KeyCode::Esc | KeyCode::Char('S') => app.close_settings(),
                                    KeyCode::Down | KeyCode::Char('j') => {
                                        app.settings_selected = (app.settings_selected + 1)
                                            .min(app.settings_count() - 1);
                                    }
                                    KeyCode::Up | KeyCode::Char('k') => {
                                        app.settings_selected =
                                            app.settings_selected.saturating_sub(1);
                                    }
                                    KeyCode::Enter | KeyCode::Char(' ') => {
                                        app.toggle_selected_setting()
                                    }
                                    _ => {}
                                }
                            } else if app.is_confirming_delete() {
                                match key.code {
                                    KeyCode::Char('y') | KeyCode::Char('Y') => {
                                        if let Some(id) = app.take_delete_workspace() {
                                            let _ = backend
                                                .cmd_tx
                                                .send(Command::RemoveWorkspace { id })
                                                .await;
                                        }
                                    }
                                    KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                                        app.cancel_delete_workspace()
                                    }
                                    _ => {}
                                }
                            } else if app.is_adding_workspace() {
                                match key.code {
                                    KeyCode::Esc => app.cancel_add_workspace(),
                                    KeyCode::Enter => {
                                        if let Some((name, path)) = app.take_add_workspace_request()
                                        {
                                            let _ = backend
                                                .cmd_tx
                                                .send(Command::AddWorkspace { name, path })
                                                .await;
                                        }
                                    }
                                    KeyCode::Backspace => {
                                        if let Some(input) = app.add_workspace_input_mut() {
                                            input.pop();
                                        }
                                    }
                                    KeyCode::Tab => {
                                        if let Some(input) = app.add_workspace_input_mut() {
                                            apply_path_autocomplete(input);
                                        }
                                    }
                                    KeyCode::Char(c) => {
                                        if let Some(input) = app.add_workspace_input_mut() {
                                            input.push(c);
                                        }
                                    }
                                    _ => {}
                                }
                            } else if app.is_renaming_workspace() {
                                match key.code {
                                    KeyCode::Esc => app.cancel_rename_workspace(),
                                    KeyCode::Enter => {
                                        if let Some((id, name)) =
                                            app.take_rename_request_home()
                                        {
                                            let _ = backend
                                                .cmd_tx
                                                .send(Command::RenameWorkspace { id, name })
                                                .await;
                                        }
                                    }
                                    KeyCode::Backspace => {
                                        if let Some(input) = app.rename_input_mut() {
                                            input.pop();
                                        }
                                    }
                                    KeyCode::Char(c) => {
                                        if let Some(input) = app.rename_input_mut() {
                                            input.push(c);
                                        }
                                    }
                                    _ => {}
                                }
                            } else {
                                match key.code {
                                    KeyCode::Esc => {
                                        app.go_home();
                                    }
                                    KeyCode::Enter => {
                                        if let Some(id) = app.selected_workspace_id() {
                                            app.open_workspace(id);
                                            start_workspace_tab_terminals(
                                                &backend.cmd_tx,
                                                id,
                                                &app.ws_tabs,
                                            )
                                            .await;
                                            let _ = backend
                                                .cmd_tx
                                                .send(Command::RefreshGit { id })
                                                .await;
                                            let _ = backend
                                                .cmd_tx
                                                .send(Command::ClearAttention { id })
                                                .await;
                                        }
                                    }
                                    KeyCode::Down | KeyCode::Char('j') => {
                                        app.move_home_selection(1)
                                    }
                                    KeyCode::Up | KeyCode::Char('k') => app.move_home_selection(-1),
                                    KeyCode::Left | KeyCode::Char('h') => {
                                        app.move_home_selection(-1)
                                    }
                                    KeyCode::Right | KeyCode::Char('l') => {
                                        app.move_home_selection(1)
                                    }
                                    KeyCode::Char('n') => {
                                        let cwd = std::env::current_dir()
                                            .unwrap_or_else(|_| PathBuf::from("."))
                                            .display()
                                            .to_string();
                                        app.begin_add_workspace(cwd);
                                    }
                                    KeyCode::Char('D') => app.begin_delete_workspace(),
                                    KeyCode::Char('e') => app.begin_rename_workspace_home(),
                                    KeyCode::Char('S') => app.open_settings(),
                                    KeyCode::Char('!') => {
                                        if let Some(id) = app.selected_workspace_id() {
                                            let level = app
                                                .workspaces
                                                .get(app.home_selected)
                                                .map(|w| w.attention)
                                                .unwrap_or(AttentionLevel::None);
                                            let cmd = if matches!(
                                                level,
                                                AttentionLevel::NeedsInput | AttentionLevel::Error
                                            ) {
                                                Command::ClearAttention { id }
                                            } else {
                                                Command::SetAttention {
                                                    id,
                                                    level: AttentionLevel::NeedsInput,
                                                }
                                            };
                                            let _ = backend.cmd_tx.send(cmd).await;
                                        }
                                    }
                                    KeyCode::Char('g') => {
                                        if let Some(id) = app.selected_workspace_id() {
                                            let _ = backend
                                                .cmd_tx
                                                .send(Command::RefreshGit { id })
                                                .await;
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                        Route::Workspace { id } => {
                            if app.is_renaming_tab() {
                                match key.code {
                                    KeyCode::Esc => app.cancel_rename_tab(),
                                    KeyCode::Enter => app.apply_rename_tab(),
                                    KeyCode::Backspace => {
                                        if let Some(input) = app.rename_tab_input_mut() {
                                            input.pop();
                                        }
                                    }
                                    KeyCode::Char(c) => {
                                        if let Some(input) = app.rename_tab_input_mut() {
                                            input.push(c);
                                        }
                                    }
                                    _ => {}
                                }
                                continue;
                            }

                            if app.is_renaming_workspace() {
                                match key.code {
                                    KeyCode::Esc => app.cancel_rename_workspace(),
                                    KeyCode::Enter => {
                                        if let Some((id, name)) = app.take_rename_request() {
                                            let _ = backend
                                                .cmd_tx
                                                .send(Command::RenameWorkspace { id, name })
                                                .await;
                                        }
                                    }
                                    KeyCode::Backspace => {
                                        if let Some(input) = app.rename_input_mut() {
                                            input.pop();
                                        }
                                    }
                                    KeyCode::Char(c) => {
                                        if let Some(input) = app.rename_input_mut() {
                                            input.push(c);
                                        }
                                    }
                                    _ => {}
                                }
                                continue;
                            }

                            if app.is_creating_branch() {
                                match key.code {
                                    KeyCode::Esc => { app.cancel_create_branch(); }
                                    KeyCode::Enter => {
                                        if let Some(name) = app.create_branch_input.take() {
                                            let trimmed = name.trim().to_string();
                                            if !trimmed.is_empty() {
                                                app.ws_pending_select_head_branch = true;
                                                let _ = backend
                                                    .cmd_tx
                                                    .send(Command::GitCreateBranch {
                                                        id,
                                                        branch: trimmed,
                                                    })
                                                    .await;
                                            }
                                        }
                                    }
                                    KeyCode::Backspace => {
                                        if let Some(input) = app.create_branch_input.as_mut() {
                                            input.pop();
                                        }
                                    }
                                    KeyCode::Char(c) => {
                                        if let Some(input) = app.create_branch_input.as_mut() {
                                            input.push(c);
                                        }
                                    }
                                    _ => {}
                                }
                                continue;
                            }

                            if app.is_committing() {
                                match key.code {
                                    KeyCode::Esc => { app.commit_input = None; }
                                    KeyCode::Enter => {
                                        if let Some(msg) = app.commit_input.take() {
                                            let trimmed = msg.trim().to_string();
                                            if !trimmed.is_empty() {
                                                let _ = backend
                                                    .cmd_tx
                                                    .send(Command::GitCommit {
                                                        id,
                                                        message: trimmed,
                                                    })
                                                    .await;
                                            }
                                        }
                                    }
                                    KeyCode::Backspace => {
                                        if let Some(input) = app.commit_input.as_mut() {
                                            input.pop();
                                        }
                                    }
                                    KeyCode::Char(c) => {
                                        if let Some(input) = app.commit_input.as_mut() {
                                            input.push(c);
                                        }
                                    }
                                    _ => {}
                                }
                                continue;
                            }

                            if key.code == KeyCode::Esc {
                                if matches!(app.focus, app::Focus::WsTerminal) {
                                    app.focus = app::Focus::WsTerminalTabs;
                                } else {
                                    app.go_home();
                                }
                                continue;
                            }

                            if matches!(app.focus, app::Focus::WsTerminal)
                                && key.code != KeyCode::Tab
                            {
                                if let Some(bytes) = key_to_terminal_bytes(key) {
                                    let _ = backend
                                        .cmd_tx
                                        .send(Command::SendTerminalInput {
                                            id,
                                            kind: app.active_tab_kind(),
                                            tab_id: Some(app.active_tab_id()),
                                            data_b64: base64::engine::general_purpose::STANDARD
                                                .encode(bytes),
                                        })
                                        .await;
                                    continue;
                                }
                            }

                            match key.code {
                                KeyCode::Enter => {
                                    if matches!(app.focus, app::Focus::WsFiles) {
                                        if let Some(file) = app.selected_changed_file() {
                                            let _ = backend
                                                .cmd_tx
                                                .send(Command::LoadDiff { id, file })
                                                .await;
                                        }
                                    } else if matches!(app.focus, app::Focus::WsLog) {
                                        if let Some(hash) = app.selected_commit_hash() {
                                            let _ = backend
                                                .cmd_tx
                                                .send(Command::LoadCommitDiff { id, hash })
                                                .await;
                                        }
                                    }
                                }
                                KeyCode::Tab => {
                                    if key.modifiers.contains(KeyModifiers::SHIFT) {
                                        app.focus = cycle_workspace_focus_reverse(app.focus);
                                    } else {
                                        app.focus = cycle_workspace_focus(app.focus);
                                    }
                                }
                                KeyCode::BackTab => {
                                    app.focus = cycle_workspace_focus_reverse(app.focus)
                                }
                                KeyCode::Char('g') => {
                                    let _ = backend.cmd_tx.send(Command::RefreshGit { id }).await;
                                }
                                KeyCode::Down | KeyCode::Char('j') => match app.focus {
                                    app::Focus::WsFiles => {
                                        app.move_workspace_file_selection(1);
                                        if let Some(file) = app.selected_changed_file() {
                                            let _ = backend
                                                .cmd_tx
                                                .send(Command::LoadDiff { id, file })
                                                .await;
                                        }
                                    }
                                    app::Focus::WsLog => {
                                        app.move_workspace_commit_selection(1);
                                    }
                                    app::Focus::WsBranches => {
                                        app.move_branch_selection(1);
                                    }
                                    app::Focus::WsDiff => {
                                        app.ws_diff_scroll = app.ws_diff_scroll.saturating_add(1)
                                    }
                                    _ => {}
                                },
                                KeyCode::Up | KeyCode::Char('k') => match app.focus {
                                    app::Focus::WsFiles => {
                                        app.move_workspace_file_selection(-1);
                                        if let Some(file) = app.selected_changed_file() {
                                            let _ = backend
                                                .cmd_tx
                                                .send(Command::LoadDiff { id, file })
                                                .await;
                                        }
                                    }
                                    app::Focus::WsLog => {
                                        app.move_workspace_commit_selection(-1);
                                    }
                                    app::Focus::WsBranches => {
                                        app.move_branch_selection(-1);
                                    }
                                    app::Focus::WsDiff => {
                                        app.ws_diff_scroll = app.ws_diff_scroll.saturating_sub(1)
                                    }
                                    _ => {}
                                },
                                KeyCode::Char(' ')
                                    if matches!(app.focus, app::Focus::WsFiles) =>
                                {
                                    // Toggle stage/unstage selected file
                                    if let Some(git) = app.workspace_git.get(&id) {
                                        if let Some(f) = git.changed.get(app.ws_selected_file) {
                                            let file = f.path.clone();
                                            let is_staged = f.index_status != ' ' && f.index_status != '?';
                                            let cmd = if is_staged {
                                                Command::GitUnstageFile { id, file }
                                            } else {
                                                Command::GitStageFile { id, file }
                                            };
                                            let _ = backend.cmd_tx.send(cmd).await;
                                        }
                                    }
                                }
                                KeyCode::Char('+')
                                    if matches!(app.focus, app::Focus::WsFiles) =>
                                {
                                    let _ = backend
                                        .cmd_tx
                                        .send(Command::GitStageAll { id })
                                        .await;
                                }
                                KeyCode::Char('-')
                                    if matches!(app.focus, app::Focus::WsFiles) =>
                                {
                                    let _ = backend
                                        .cmd_tx
                                        .send(Command::GitUnstageAll { id })
                                        .await;
                                }
                                KeyCode::Char('c')
                                    if matches!(app.focus, app::Focus::WsFiles) =>
                                {
                                    app.commit_input = Some(String::new());
                                }
                                KeyCode::Char('c')
                                    if matches!(app.focus, app::Focus::WsBranches) =>
                                {
                                    if matches!(app.ws_branch_sub_pane, app::BranchSubPane::Local) {
                                        app.begin_create_branch();
                                    }
                                }
                                KeyCode::Char('[')
                                    if matches!(app.focus, app::Focus::WsBranches) =>
                                {
                                    app.toggle_branch_sub_pane(app::BranchSubPane::Local);
                                }
                                KeyCode::Char(']')
                                    if matches!(app.focus, app::Focus::WsBranches) =>
                                {
                                    app.toggle_branch_sub_pane(app::BranchSubPane::Remote);
                                }
                                KeyCode::Char(' ')
                                    if matches!(app.focus, app::Focus::WsBranches) =>
                                {
                                    match app.ws_branch_sub_pane {
                                        app::BranchSubPane::Local => {
                                            if let Some(branch) = app.selected_local_branch() {
                                                if !branch.is_head {
                                                    let branch_name = branch.name.clone();
                                                    let _ = backend
                                                        .cmd_tx
                                                        .send(Command::GitCheckoutBranch {
                                                            id,
                                                            branch: branch_name,
                                                        })
                                                        .await;
                                                }
                                            }
                                        }
                                        app::BranchSubPane::Remote => {
                                            if let Some(rb) = app.selected_remote_branch() {
                                                let full = rb.full_name.clone();
                                                if let Some(local_name) = full.splitn(2, '/').nth(1) {
                                                    let local_name = local_name.to_string();
                                                    app.ws_pending_select_head_branch = true;
                                                    app.ws_branch_sub_pane = app::BranchSubPane::Local;
                                                    let _ = backend
                                                        .cmd_tx
                                                        .send(Command::GitCheckoutRemoteBranch {
                                                            id,
                                                            remote_branch: full,
                                                            local_name,
                                                        })
                                                        .await;
                                                }
                                            }
                                        }
                                    }
                                }
                                KeyCode::Char('p')
                                    if matches!(app.focus, app::Focus::WsBranches) =>
                                {
                                    let _ = backend.cmd_tx.send(Command::GitPull { id }).await;
                                    app.begin_git_op(id);
                                }
                                KeyCode::Char('f')
                                    if matches!(app.focus, app::Focus::WsBranches) =>
                                {
                                    let _ = backend.cmd_tx.send(Command::GitFetch { id }).await;
                                    app.begin_git_op(id);
                                }
                                KeyCode::Char('P')
                                    if matches!(app.focus, app::Focus::WsBranches) =>
                                {
                                    let _ = backend.cmd_tx.send(Command::GitPush { id }).await;
                                    app.begin_git_op(id);
                                }
                                KeyCode::Char('1') => app.set_active_tab_index(0),
                                KeyCode::Char('2') => app.set_active_tab_index(1),
                                KeyCode::Right | KeyCode::Char('l')
                                    if matches!(app.focus, app::Focus::WsTerminalTabs) =>
                                {
                                    app.move_terminal_tab(1);
                                }
                                KeyCode::Left | KeyCode::Char('h')
                                    if matches!(app.focus, app::Focus::WsTerminalTabs) =>
                                {
                                    app.move_terminal_tab(-1);
                                }
                                KeyCode::Char('n')
                                    if matches!(app.focus, app::Focus::WsTerminalTabs) =>
                                {
                                    app.add_shell_tab();
                                    let _ = backend
                                        .cmd_tx
                                        .send(Command::StartTerminal {
                                            id,
                                            kind: TerminalKind::Shell,
                                            tab_id: Some(app.active_tab_id()),
                                            cmd: Vec::new(),
                                        })
                                        .await;
                                }
                                KeyCode::Char('x')
                                    if matches!(app.focus, app::Focus::WsTerminalTabs) =>
                                {
                                    if let Some(closed) = app.close_active_tab() {
                                        let _ = backend
                                            .cmd_tx
                                            .send(Command::StopTerminal {
                                                id,
                                                kind: closed.kind,
                                                tab_id: Some(closed.id),
                                            })
                                            .await;
                                    }
                                }
                                KeyCode::Char('r')
                                    if matches!(app.focus, app::Focus::WsTerminalTabs) =>
                                {
                                    app.begin_rename_tab();
                                }
                                KeyCode::Char('a') => {
                                    let _ = backend
                                        .cmd_tx
                                        .send(Command::StartTerminal {
                                            id,
                                            kind: app.active_tab_kind(),
                                            tab_id: Some(app.active_tab_id()),
                                            cmd: Vec::new(),
                                        })
                                        .await;
                                    app.focus = app::Focus::WsTerminal;
                                }
                                KeyCode::Char('A') => {
                                    let _ = backend
                                        .cmd_tx
                                        .send(Command::StopTerminal {
                                            id,
                                            kind: app.active_tab_kind(),
                                            tab_id: Some(app.active_tab_id()),
                                        })
                                        .await;
                                }
                                KeyCode::Char('s') => {
                                    let _ = backend
                                        .cmd_tx
                                        .send(Command::StartTerminal {
                                            id,
                                            kind: app.active_tab_kind(),
                                            tab_id: Some(app.active_tab_id()),
                                            cmd: Vec::new(),
                                        })
                                        .await;
                                }
                                KeyCode::Char('S') => {
                                    let _ = backend
                                        .cmd_tx
                                        .send(Command::StopTerminal {
                                            id,
                                            kind: app.active_tab_kind(),
                                            tab_id: Some(app.active_tab_id()),
                                        })
                                        .await;
                                }
                                _ => {}
                            }
                        }
                    };
                }
                Event::Mouse(mouse) => {
                    handle_mouse(&mut app, &backend.cmd_tx, &mut terminal, mouse).await;
                }
                _ => {}
            }
        }

        if last_flash_toggle.elapsed() >= Duration::from_millis(250) {
            app.flash_on = !app.flash_on;
            last_flash_toggle = Instant::now();
        }
    }

    disable_raw_mode()?;
    std::io::stdout().execute(DisableMouseCapture)?;
    std::io::stdout().execute(LeaveAlternateScreen)?;
    Ok(())
}

fn list_sessions() -> Result<()> {
    let registry = load_registry()?;
    if registry.sessions.is_empty() {
        println!("no sessions");
        return Ok(());
    }

    println!("sessions:");
    for s in registry.sessions {
        let state = if port_open(s.port) {
            "running"
        } else {
            "stale"
        };
        println!("- {}  (port {} pid {} {})", s.name, s.port, s.pid, state);
    }
    Ok(())
}

fn apply_event(app: &mut TuiApp, evt: CoreEvent) {
    match evt {
        CoreEvent::WorkspaceList { items } => app.set_workspaces(items),
        CoreEvent::WorkspaceGitUpdated { id, git } => app.set_workspace_git(id, git),
        CoreEvent::WorkspaceDiffUpdated { id, file, diff } => {
            app.set_workspace_diff(id, file, diff)
        }
        CoreEvent::TerminalOutput {
            id,
            kind: _,
            data_b64,
            tab_id,
            ..
        } => {
            if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data_b64) {
                let tid = tab_id.unwrap_or_else(|| "shell".to_string());
                app.append_terminal_bytes(id, &tid, &bytes);
            }
        }
        CoreEvent::TerminalExited {
            id,
            kind: _,
            code,
            tab_id,
            ..
        } => {
            let msg = format!("\r\n[terminal exited: {:?}]\r\n", code);
            let tid = tab_id.unwrap_or_else(|| "shell".to_string());
            app.append_terminal_bytes(id, &tid, msg.as_bytes());
        }
        CoreEvent::TerminalStarted {
            id,
            kind: _,
            tab_id,
            ..
        } => {
            let tid = tab_id.unwrap_or_else(|| "shell".to_string());
            app.reset_terminal(id, &tid);
            app.append_terminal_bytes(id, &tid, b"[terminal started]\r\n");
        }
        CoreEvent::GitActionResult {
            id,
            action: _,
            success: _,
            message,
        } => {
            app.finish_git_op(id);
            app.git_action_message = Some((message, std::time::Instant::now()));
        }
        CoreEvent::WorkspaceAttentionChanged { id, level } => {
            if let Some(ws) = app.workspaces.iter_mut().find(|w| w.id == id) {
                ws.attention = level;
            }
        }
        _ => {}
    }
}

fn cycle_workspace_focus(focus: app::Focus) -> app::Focus {
    match focus {
        app::Focus::WsTerminalTabs => app::Focus::WsTerminal,
        app::Focus::WsTerminal => app::Focus::WsFiles,
        app::Focus::WsFiles => app::Focus::WsLog,
        app::Focus::WsLog => app::Focus::WsBranches,
        app::Focus::WsBranches => app::Focus::WsDiff,
        app::Focus::WsDiff => app::Focus::WsTerminalTabs,
        _ => app::Focus::WsTerminalTabs,
    }
}

fn cycle_workspace_focus_reverse(focus: app::Focus) -> app::Focus {
    match focus {
        app::Focus::WsTerminalTabs => app::Focus::WsDiff,
        app::Focus::WsTerminal => app::Focus::WsTerminalTabs,
        app::Focus::WsFiles => app::Focus::WsTerminal,
        app::Focus::WsLog => app::Focus::WsFiles,
        app::Focus::WsBranches => app::Focus::WsLog,
        app::Focus::WsDiff => app::Focus::WsBranches,
        _ => app::Focus::WsTerminalTabs,
    }
}

fn key_to_terminal_bytes(key: KeyEvent) -> Option<Vec<u8>> {
    match key.code {
        KeyCode::Char(c) => {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                let b = (c as u8) & 0x1f;
                Some(vec![b])
            } else {
                Some(c.to_string().into_bytes())
            }
        }
        KeyCode::Enter => Some(vec![b'\r']),
        KeyCode::Backspace => Some(vec![0x7f]),
        KeyCode::Tab => Some(vec![b'\t']),
        KeyCode::Left => Some(b"\x1b[D".to_vec()),
        KeyCode::Right => Some(b"\x1b[C".to_vec()),
        KeyCode::Up => Some(b"\x1b[A".to_vec()),
        KeyCode::Down => Some(b"\x1b[B".to_vec()),
        _ => None,
    }
}

fn apply_path_autocomplete(input: &mut String) {
    let current = input.trim();
    let (dir, prefix) = split_dir_and_prefix(current);
    let entries = match std::fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(_) => return,
    };

    let mut matches = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with(&prefix) {
            matches.push((name, entry.path().is_dir()));
        }
    }
    if matches.is_empty() {
        return;
    }
    matches.sort_by(|a, b| a.0.cmp(&b.0));

    let common = longest_common_prefix(
        &matches
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
    );
    let replacement = if common.len() > prefix.len() {
        common
    } else {
        matches[0].0.clone()
    };

    let mut completed = if dir.as_os_str().is_empty() || dir == Path::new(".") {
        replacement
    } else {
        format!("{}/{}", dir.display(), replacement)
    };

    if matches.len() == 1 && matches[0].1 {
        completed.push('/');
    }
    *input = completed;
}

fn split_dir_and_prefix(input: &str) -> (PathBuf, String) {
    if input.is_empty() {
        return (PathBuf::from("."), String::new());
    }
    if input.ends_with('/') {
        return (PathBuf::from(input), String::new());
    }
    let path = Path::new(input);
    let dir = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let prefix = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    (dir, prefix)
}

fn longest_common_prefix(parts: &[&str]) -> String {
    let Some(first) = parts.first() else {
        return String::new();
    };
    let mut end = first.len();
    for part in parts.iter().skip(1) {
        while end > 0 && !part.starts_with(&first[..end]) {
            end -= 1;
        }
        if end == 0 {
            break;
        }
    }
    first[..end].to_string()
}

async fn handle_mouse(
    app: &mut TuiApp,
    cmd_tx: &tokio::sync::mpsc::Sender<Command>,
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    mouse: MouseEvent,
) {
    let area = match terminal.size() {
        Ok(s) => ratatui::layout::Rect::new(0, 0, s.width, s.height),
        Err(_) => return,
    };

    match app.route {
        Route::Home => match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if app.is_confirming_delete() {
                    let rect = ui::screens::home::delete_modal_rect(area);
                    if point_in_rect(rect, mouse.column, mouse.row) {
                        let mid = rect.x + rect.width / 2;
                        if mouse.column < mid {
                            if let Some(id) = app.take_delete_workspace() {
                                let _ = cmd_tx.send(Command::RemoveWorkspace { id }).await;
                            }
                        } else {
                            app.cancel_delete_workspace();
                        }
                    } else {
                        app.cancel_delete_workspace();
                    }
                    return;
                }
                if app.is_adding_workspace() {
                    let rect = ui::screens::home::add_modal_rect(area);
                    if !point_in_rect(rect, mouse.column, mouse.row) {
                        app.cancel_add_workspace();
                    }
                    return;
                }

                let grid = ui::screens::home::grid_rect(area);
                if let Some(idx) = ui::widgets::tile_grid::index_at(
                    grid,
                    mouse.column,
                    mouse.row,
                    app.workspaces.len(),
                ) {
                    app.set_home_selection(idx);
                    if let Some(id) = app.selected_workspace_id() {
                        app.open_workspace(id);
                        start_workspace_tab_terminals(cmd_tx, id, &app.ws_tabs).await;
                        let _ = cmd_tx.send(Command::RefreshGit { id }).await;
                        let _ = cmd_tx.send(Command::ClearAttention { id }).await;
                    }
                }
            }
            _ => {}
        },
        Route::Workspace { id } => match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(hit) =
                    ui::screens::workspace::hit_test(area, app, mouse.column, mouse.row)
                {
                    match hit {
                        ui::screens::workspace::WorkspaceHit::Header => {}
                        ui::screens::workspace::WorkspaceHit::TerminalTab(idx) => {
                            app.focus = app::Focus::WsTerminalTabs;
                            app.set_active_tab_index(idx);
                        }
                        ui::screens::workspace::WorkspaceHit::TerminalPane => {
                            app.focus = app::Focus::WsTerminal;
                        }
                        ui::screens::workspace::WorkspaceHit::FilesList(idx) => {
                            app.focus = app::Focus::WsFiles;
                            app.ws_selected_file = idx;
                            if let Some(file) = app.selected_changed_file() {
                                let _ = cmd_tx.send(Command::LoadDiff { id, file }).await;
                            }
                        }
                        ui::screens::workspace::WorkspaceHit::LogList(idx) => {
                            app.focus = app::Focus::WsLog;
                            app.ws_selected_commit = idx;
                        }
                        ui::screens::workspace::WorkspaceHit::BranchesPane(idx) => {
                            app.focus = app::Focus::WsBranches;
                            match app.ws_branch_sub_pane {
                                app::BranchSubPane::Local => {
                                    app.ws_selected_local_branch = idx;
                                }
                                app::BranchSubPane::Remote => {
                                    app.ws_selected_remote_branch = idx;
                                }
                            }
                        }
                        ui::screens::workspace::WorkspaceHit::DiffPane => {
                            app.focus = app::Focus::WsDiff;
                        }
                    }
                }
            }
            MouseEventKind::ScrollUp => {
                let hit = ui::screens::workspace::hit_test(area, app, mouse.column, mouse.row);
                if matches!(app.focus, app::Focus::WsDiff)
                    || matches!(hit, Some(ui::screens::workspace::WorkspaceHit::DiffPane))
                {
                    app.ws_diff_scroll = app.ws_diff_scroll.saturating_sub(3);
                } else if matches!(app.focus, app::Focus::WsTerminal)
                    || matches!(
                        hit,
                        Some(ui::screens::workspace::WorkspaceHit::TerminalPane)
                    )
                {
                    let tab_id = app.active_tab_id();
                    app.scroll_terminal_scrollback(id, &tab_id, 3);
                }
            }
            MouseEventKind::ScrollDown => {
                let hit = ui::screens::workspace::hit_test(area, app, mouse.column, mouse.row);
                if matches!(app.focus, app::Focus::WsDiff)
                    || matches!(hit, Some(ui::screens::workspace::WorkspaceHit::DiffPane))
                {
                    app.ws_diff_scroll = app.ws_diff_scroll.saturating_add(3);
                } else if matches!(app.focus, app::Focus::WsTerminal)
                    || matches!(
                        hit,
                        Some(ui::screens::workspace::WorkspaceHit::TerminalPane)
                    )
                {
                    let tab_id = app.active_tab_id();
                    app.scroll_terminal_scrollback(id, &tab_id, -3);
                }
            }
            _ => {}
        },
    }
}

fn point_in_rect(r: ratatui::layout::Rect, x: u16, y: u16) -> bool {
    x >= r.x && y >= r.y && x < r.right() && y < r.bottom()
}

async fn start_workspace_tab_terminals(
    cmd_tx: &tokio::sync::mpsc::Sender<Command>,
    id: protocol::WorkspaceId,
    tabs: &[app::TerminalTab],
) {
    for tab in tabs {
        let _ = cmd_tx
            .send(Command::StartTerminal {
                id,
                kind: tab.kind,
                tab_id: Some(tab.id.clone()),
                cmd: Vec::new(),
            })
            .await;
    }
}

async fn ensure_session_running(name: &str) -> Result<SessionEntry> {
    let mut registry = load_registry()?;
    if let Some(existing) = registry.sessions.iter().find(|s| s.name == name).cloned() {
        if port_open(existing.port) {
            return Ok(existing);
        }
        registry.sessions.retain(|s| s.name != name);
    }

    let port = find_free_port(4101, 4299)
        .ok_or_else(|| anyhow!("no free ports available for session daemon"))?;
    let pid = spawn_daemon_process(name, port)?;

    wait_for_port(port, Duration::from_secs(8)).await?;

    let entry = SessionEntry {
        name: name.to_string(),
        port,
        pid,
    };
    registry.sessions.retain(|s| s.name != name);
    registry.sessions.push(entry.clone());
    save_registry(&registry)?;
    Ok(entry)
}

fn get_session(name: &str) -> Result<Option<SessionEntry>> {
    let registry = load_registry()?;
    Ok(registry.sessions.into_iter().find(|s| s.name == name))
}

fn delete_session(name: &str) -> Result<()> {
    let mut registry = load_registry()?;
    let Some(entry) = registry.sessions.iter().find(|s| s.name == name).cloned() else {
        println!("session '{}' not found", name);
        return Ok(());
    };

    print!(
        "Delete session '{}' on port {}? This will stop running terminals. [y/N]: ",
        entry.name, entry.port
    );
    std::io::stdout().flush()?;
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    let confirm = matches!(input.trim().to_lowercase().as_str(), "y" | "yes");
    if !confirm {
        println!("aborted");
        return Ok(());
    }

    if is_expected_daemon_process(&entry) {
        let _ = OsCommand::new("kill").arg(entry.pid.to_string()).status();
    } else {
        println!(
            "warning: pid {} does not look like session daemon '{}'; skipping kill and removing registry entry only",
            entry.pid, entry.name
        );
    }

    registry.sessions.retain(|s| s.name != name);
    save_registry(&registry)?;
    if let Some(path) = session_workspaces_persist_path(name) {
        let _ = std::fs::remove_file(path);
    }
    println!("deleted session '{}'", name);
    Ok(())
}

fn spawn_daemon_process(name: &str, port: u16) -> Result<u32> {
    let exe = std::env::current_exe()?;
    let child = OsCommand::new(exe)
        .arg("--run-daemon")
        .arg("--session-name")
        .arg(name)
        .arg("--port")
        .arg(port.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| {
            format!(
                "failed to spawn daemon for session '{}', port {}",
                name, port
            )
        })?;
    Ok(child.id())
}

async fn wait_for_port(port: u16, timeout: Duration) -> Result<()> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if port_open(port) {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(120)).await;
    }
    Err(anyhow!("daemon did not become ready on port {}", port))
}

fn port_open(port: u16) -> bool {
    let addr: SocketAddr = match format!("127.0.0.1:{port}").parse() {
        Ok(a) => a,
        Err(_) => return false,
    };
    TcpStream::connect_timeout(&addr, Duration::from_millis(150)).is_ok()
}

fn is_expected_daemon_process(entry: &SessionEntry) -> bool {
    let output = match OsCommand::new("ps")
        .arg("-p")
        .arg(entry.pid.to_string())
        .arg("-o")
        .arg("command=")
        .output()
    {
        Ok(out) => out,
        Err(_) => return false,
    };
    if !output.status.success() {
        return false;
    }
    let cmdline = String::from_utf8_lossy(&output.stdout);
    cmdline.contains("--run-daemon")
        && cmdline.contains(&format!("--port {}", entry.port))
        && cmdline.contains(&format!("--session-name {}", entry.name))
}

fn find_free_port(start: u16, end: u16) -> Option<u16> {
    (start..=end).find(|p| !port_open(*p))
}

fn session_registry_path() -> Option<PathBuf> {
    let base = if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        PathBuf::from(xdg)
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".config")
    } else {
        return None;
    };
    Some(base.join("anvl").join("sessions.json"))
}

fn session_workspaces_persist_path(name: &str) -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    let safe = sanitize_session_name(name);
    Some(
        PathBuf::from(home)
            .join(".config")
            .join("anvl")
            .join(format!("workspaces.{safe}.json")),
    )
}

fn sanitize_session_name(input: &str) -> String {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return "default".to_string();
    }
    let mut out = String::with_capacity(trimmed.len());
    for c in trimmed.chars() {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    out
}

fn load_registry() -> Result<SessionRegistry> {
    let Some(path) = session_registry_path() else {
        return Ok(SessionRegistry::default());
    };
    if !path.exists() {
        return Ok(SessionRegistry::default());
    }
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read session registry: {}", path.display()))?;
    let registry = serde_json::from_str::<SessionRegistry>(&raw).unwrap_or_default();
    Ok(registry)
}

fn save_registry(registry: &SessionRegistry) -> Result<()> {
    let Some(path) = session_registry_path() else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let raw = serde_json::to_string_pretty(registry)?;
    std::fs::write(&path, raw)
        .with_context(|| format!("failed to write session registry: {}", path.display()))?;
    Ok(())
}
