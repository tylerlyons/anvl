mod app;
mod keymap;
mod ui;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

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
use multiws_core::spawn_core;
use protocol::{AttentionLevel, Command, Event as CoreEvent, Route, TerminalKind};
use ratatui::{backend::CrosstermBackend, Terminal};
use tokio::sync::broadcast::error::TryRecvError;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    stdout.execute(EnterAlternateScreen)?;
    stdout.execute(EnableMouseCapture)?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let mut app = TuiApp::default();
    let core = spawn_core();
    let web_port = std::env::var("MULTIWS_WEB_PORT")
        .ok()
        .and_then(|v| v.parse::<u16>().ok())
        .unwrap_or(3001);
    if std::env::var("MULTIWS_DISABLE_EMBEDDED_WEB").is_err() {
        let core_for_web = core.clone();
        tokio::spawn(async move {
            let _ = server::run_server_with_core(core_for_web, web_port).await;
        });
    }
    let mut evt_rx = core.evt_tx.subscribe();
    let mut last_flash_toggle = Instant::now();

    loop {
        loop {
            match evt_rx.try_recv() {
                Ok(evt) => match evt {
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
                        if let Ok(bytes) =
                            base64::engine::general_purpose::STANDARD.decode(data_b64)
                        {
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
                    _ => {}
                },
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Closed) => break,
                Err(TryRecvError::Lagged(_)) => continue,
            }
        }

        terminal.draw(|frame| match app.route {
            Route::Home => ui::screens::home::render(frame, frame.area(), &app),
            Route::Workspace { .. } => ui::screens::workspace::render(frame, frame.area(), &app),
        })?;

        if let Route::Workspace { id } = app.route {
            if let Ok(size) = terminal.size() {
                let area = ratatui::layout::Rect::new(0, 0, size.width, size.height);
                let inner = ui::screens::workspace::terminal_content_rect(area, app.focus);
                let cols = inner.width.max(1);
                let rows = inner.height.max(1);
                let tid = app.active_tab_id();
                let kind = app.active_tab_kind();
                if app.should_send_resize(id, &tid, cols, rows) {
                    app.resize_terminal_parser(id, &tid, cols, rows);
                    let _ = core
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
                        && !matches!(app.focus, app::Focus::WsTerminal)
                    {
                        break;
                    }

                    match app.route {
                        Route::Home => {
                            if app.is_confirming_delete() {
                                match key.code {
                                    KeyCode::Char('y') | KeyCode::Char('Y') => {
                                        if let Some(id) = app.take_delete_workspace() {
                                            let _ = core
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
                                            let _ = core
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
                            } else {
                                match key.code {
                                    KeyCode::Esc => {
                                        app.go_home();
                                    }
                                    KeyCode::Enter => {
                                        if let Some(id) = app.selected_workspace_id() {
                                            app.open_workspace(id);
                                            let _ =
                                                core.cmd_tx.send(Command::RefreshGit { id }).await;
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
                                            let _ = core.cmd_tx.send(cmd).await;
                                        }
                                    }
                                    KeyCode::Char('g') => {
                                        if let Some(id) = app.selected_workspace_id() {
                                            let _ =
                                                core.cmd_tx.send(Command::RefreshGit { id }).await;
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
                                            let _ = core
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
                                    let _ = core
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
                                            let _ = core
                                                .cmd_tx
                                                .send(Command::LoadDiff { id, file })
                                                .await;
                                        }
                                    } else if matches!(app.focus, app::Focus::WsTerminalTabs) {
                                        let _ = core
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
                                    let _ = core.cmd_tx.send(Command::RefreshGit { id }).await;
                                }
                                KeyCode::Down | KeyCode::Char('j') => match app.focus {
                                    app::Focus::WsFiles => {
                                        app.move_workspace_file_selection(1);
                                        if let Some(file) = app.selected_changed_file() {
                                            let _ = core
                                                .cmd_tx
                                                .send(Command::LoadDiff { id, file })
                                                .await;
                                        }
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
                                            let _ = core
                                                .cmd_tx
                                                .send(Command::LoadDiff { id, file })
                                                .await;
                                        }
                                    }
                                    app::Focus::WsDiff => {
                                        app.ws_diff_scroll = app.ws_diff_scroll.saturating_sub(1)
                                    }
                                    _ => {}
                                },
                                KeyCode::Char('e') if matches!(app.focus, app::Focus::WsHeader) => {
                                    app.begin_rename_workspace();
                                }
                                KeyCode::Char('1') => {
                                    app.set_active_tab_index(0);
                                }
                                KeyCode::Char('2') => {
                                    app.set_active_tab_index(1);
                                }
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
                                    let _ = core
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
                                        let _ = core
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
                                    let _ = core
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
                                    let _ = core
                                        .cmd_tx
                                        .send(Command::StopTerminal {
                                            id,
                                            kind: app.active_tab_kind(),
                                            tab_id: Some(app.active_tab_id()),
                                        })
                                        .await;
                                }
                                KeyCode::Char('s') => {
                                    let _ = core
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
                                    let _ = core
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
                    handle_mouse(&mut app, &core.cmd_tx, &mut terminal, mouse).await;
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

fn cycle_workspace_focus(focus: app::Focus) -> app::Focus {
    match focus {
        app::Focus::WsHeader => app::Focus::WsTerminalTabs,
        app::Focus::WsTerminalTabs => app::Focus::WsTerminal,
        app::Focus::WsTerminal => app::Focus::WsFiles,
        app::Focus::WsFiles => app::Focus::WsDiff,
        app::Focus::WsDiff => app::Focus::WsHeader,
        _ => app::Focus::WsTerminalTabs,
    }
}

fn cycle_workspace_focus_reverse(focus: app::Focus) -> app::Focus {
    match focus {
        app::Focus::WsHeader => app::Focus::WsDiff,
        app::Focus::WsTerminalTabs => app::Focus::WsHeader,
        app::Focus::WsTerminal => app::Focus::WsTerminalTabs,
        app::Focus::WsFiles => app::Focus::WsTerminal,
        app::Focus::WsDiff => app::Focus::WsFiles,
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

                if let Some(idx) = ui::widgets::tile_grid::index_at(
                    area,
                    mouse.column,
                    mouse.row,
                    app.workspaces.len(),
                ) {
                    app.set_home_selection(idx);
                    if let Some(id) = app.selected_workspace_id() {
                        app.open_workspace(id);
                        let _ = cmd_tx.send(Command::RefreshGit { id }).await;
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
                        ui::screens::workspace::WorkspaceHit::Header => {
                            app.focus = app::Focus::WsHeader;
                        }
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
                        ui::screens::workspace::WorkspaceHit::DiffPane => {
                            app.focus = app::Focus::WsDiff;
                        }
                    }
                }
            }
            MouseEventKind::ScrollUp => {
                if matches!(app.focus, app::Focus::WsDiff)
                    || matches!(
                        ui::screens::workspace::hit_test(area, app, mouse.column, mouse.row),
                        Some(ui::screens::workspace::WorkspaceHit::DiffPane)
                    )
                {
                    app.ws_diff_scroll = app.ws_diff_scroll.saturating_sub(3);
                }
            }
            MouseEventKind::ScrollDown => {
                if matches!(app.focus, app::Focus::WsDiff)
                    || matches!(
                        ui::screens::workspace::hit_test(area, app, mouse.column, mouse.row),
                        Some(ui::screens::workspace::WorkspaceHit::DiffPane)
                    )
                {
                    app.ws_diff_scroll = app.ws_diff_scroll.saturating_add(3);
                }
            }
            _ => {}
        },
    }
}

fn point_in_rect(r: ratatui::layout::Rect, x: u16, y: u16) -> bool {
    x >= r.x && y >= r.y && x < r.right() && y < r.bottom()
}
