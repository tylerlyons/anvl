mod app;
mod keymap;
mod ui;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
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
use anvl_core::{spawn_core, CoreHandle};
use protocol::{AttentionLevel, Command, Event as CoreEvent, Route, TerminalKind};
use ratatui::{backend::CrosstermBackend, Terminal};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;

struct Backend {
    cmd_tx: mpsc::Sender<Command>,
    evt_rx: mpsc::Receiver<CoreEvent>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-V" || a == "--version") {
        println!("anvl {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if let Some(arg) = args.first() {
        return Err(anyhow!("unknown argument: {arg}"));
    }
    let (backend, _core) = build_local_backend();
    run_tui(backend).await
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

        let mut pending_clipboard_text: Option<String> = None;
        terminal.draw(|frame| {
            match app.route {
                Route::Home => ui::screens::home::render(frame, frame.area(), &app),
                Route::Workspace { .. } => {
                    ui::screens::workspace::render(frame, frame.area(), &app)
                }
            }
            // Extract selected text from the rendered buffer before applying highlights.
            if let Some(sel) = &app.pending_copy_selection {
                pending_clipboard_text =
                    Some(extract_selected_text_from_buf(frame.buffer_mut(), sel));
            }
            if let Some(sel) = &app.mouse_selection {
                if !sel.is_empty() {
                    apply_selection_highlight(frame, sel);
                }
            }
        })?;
        if let Some(text) = pending_clipboard_text {
            app.pending_copy_selection = None;
            if !text.is_empty() {
                if let Ok(mut clipboard) = arboard::Clipboard::new() {
                    let _ = clipboard.set_text(text);
                    app.git_action_message =
                        Some(("Copied to clipboard".to_string(), Instant::now()));
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
                                let editing = app.dir_browser.as_ref().map_or(false, |b| b.editing_path);
                                if editing {
                                    match key.code {
                                        KeyCode::Esc => app.cancel_add_workspace(),
                                        KeyCode::Enter => {
                                            if let Some(browser) = app.dir_browser_mut() {
                                                browser.confirm_path_edit();
                                            }
                                        }
                                        KeyCode::Backspace => {
                                            if let Some(browser) = app.dir_browser_mut() {
                                                browser.path_input.pop();
                                            }
                                        }
                                        KeyCode::Tab => {
                                            if let Some(browser) = app.dir_browser_mut() {
                                                apply_path_autocomplete(&mut browser.path_input);
                                                browser.confirm_path_edit();
                                            }
                                        }
                                        KeyCode::Char(c) => {
                                            if let Some(browser) = app.dir_browser_mut() {
                                                browser.path_input.push(c);
                                            }
                                        }
                                        _ => {}
                                    }
                                } else {
                                    match key.code {
                                        KeyCode::Esc => app.cancel_add_workspace(),
                                        KeyCode::Char('j') | KeyCode::Down => {
                                            if let Some(browser) = app.dir_browser_mut() {
                                                browser.move_selection(1);
                                            }
                                        }
                                        KeyCode::Char('k') | KeyCode::Up => {
                                            if let Some(browser) = app.dir_browser_mut() {
                                                browser.move_selection(-1);
                                            }
                                        }
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
                                            if let Some(browser) = app.dir_browser_mut() {
                                                browser.go_up();
                                            }
                                        }
                                        KeyCode::Char('.') => {
                                            if let Some(browser) = app.dir_browser_mut() {
                                                browser.toggle_hidden();
                                            }
                                        }
                                        KeyCode::Char('/') => {
                                            if let Some(browser) = app.dir_browser_mut() {
                                                browser.begin_path_edit();
                                            }
                                        }
                                        KeyCode::Tab => {
                                            if let Some(browser) = app.dir_browser_mut() {
                                                browser.enter_selected();
                                            }
                                        }
                                        KeyCode::Char(' ') => {
                                            let child_path = app
                                                .dir_browser
                                                .as_ref()
                                                .and_then(|b| b.selected_child_path());
                                            if let Some(path) = child_path {
                                                if let Some((name, path)) =
                                                    app.take_add_workspace_request_with_path(path)
                                                {
                                                    let _ = backend
                                                        .cmd_tx
                                                        .send(Command::AddWorkspace { name, path })
                                                        .await;
                                                }
                                            }
                                        }
                                        _ => {}
                                    }
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
                                        app.move_home_selection(0, 1)
                                    }
                                    KeyCode::Up | KeyCode::Char('k') => app.move_home_selection(0, -1),
                                    KeyCode::Left | KeyCode::Char('h') => {
                                        app.move_home_selection(-1, 0)
                                    }
                                    KeyCode::Right | KeyCode::Char('l') => {
                                        app.move_home_selection(1, 0)
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

                            // Ctrl+G toggles terminal passthrough mode.
                            if key.code == KeyCode::Char('g')
                                && key.modifiers.contains(KeyModifiers::CONTROL)
                                && matches!(app.focus, app::Focus::WsTerminal)
                            {
                                app.toggle_active_tab_passthrough();
                                continue;
                            }

                            // In passthrough mode, forward everything (including Esc/Tab)
                            // to the terminal.
                            if app.active_tab_passthrough()
                                && matches!(app.focus, app::Focus::WsTerminal)
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
                                && key.code != KeyCode::BackTab
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
        KeyCode::Esc => Some(vec![0x1b]),
        KeyCode::Enter => Some(vec![b'\r']),
        KeyCode::Backspace => Some(vec![0x7f]),
        KeyCode::Tab => Some(vec![b'\t']),
        KeyCode::BackTab => Some(b"\x1b[Z".to_vec()),
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

    // Handle drag selection (works across all routes/panes)
    match mouse.kind {
        MouseEventKind::Drag(MouseButton::Left) => {
            if let Some(sel) = &mut app.mouse_selection {
                sel.end_col = mouse.column;
                sel.end_row = mouse.row;
            }
            return;
        }
        MouseEventKind::Up(MouseButton::Left) => {
            if let Some(sel) = app.mouse_selection.take() {
                if !sel.is_empty() {
                    app.pending_copy_selection = Some(sel);
                }
            }
            return;
        }
        _ => {}
    }

    match app.route {
        Route::Home => match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                app.mouse_selection = Some(app::MouseSelection::at(mouse.column, mouse.row));
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
                app.mouse_selection = Some(app::MouseSelection::at(mouse.column, mouse.row));
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

/// xterm-256 colour 39 — a medium sky-blue used for mouse selection highlighting.
const SELECTION_BG: ratatui::style::Color = ratatui::style::Color::Indexed(39);

fn apply_selection_highlight(frame: &mut ratatui::Frame, sel: &app::MouseSelection) {
    let ((start_col, start_row), (end_col, end_row)) = sel.ordered();
    let buf = frame.buffer_mut();
    let width = buf.area.width;
    for row in start_row..=end_row {
        let row_start = if row == start_row { start_col } else { 0 };
        let row_end = if row == end_row {
            end_col
        } else {
            width.saturating_sub(1)
        };
        for col in row_start..=row_end {
            if let Some(cell) = buf.cell_mut(ratatui::layout::Position::new(col, row)) {
                cell.set_style(
                    ratatui::style::Style::default()
                        .bg(SELECTION_BG)
                        .fg(ratatui::style::Color::Black),
                );
            }
        }
    }
}

fn extract_selected_text_from_buf(
    buf: &ratatui::buffer::Buffer,
    sel: &app::MouseSelection,
) -> String {
    let ((start_col, start_row), (end_col, end_row)) = sel.ordered();
    let width = buf.area.width;
    let mut result = String::new();
    for row in start_row..=end_row {
        let row_start = if row == start_row { start_col } else { 0 };
        let row_end = if row == end_row {
            end_col
        } else {
            width.saturating_sub(1)
        };
        let mut line = String::new();
        for col in row_start..=row_end {
            if let Some(cell) = buf.cell(ratatui::layout::Position::new(col, row)) {
                line.push_str(cell.symbol());
            }
        }
        if !result.is_empty() {
            result.push('\n');
        }
        result.push_str(line.trim_end());
    }
    result
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

