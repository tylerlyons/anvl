pub mod commands;
pub mod events;
pub mod state;
pub mod workspace;

use std::path::PathBuf;
use std::time::Instant;
use std::time::{SystemTime, UNIX_EPOCH};
use std::{fs, time::Duration};

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc};

use protocol::{AttentionLevel, Command, Event, WorkspaceSummary};
use state::{AppState, Workspace};
use uuid::Uuid;
use workspace::attention::{append_recent_output, detect_needs_input_text};
use workspace::git::{diff_file, refresh_git};
use workspace::terminal::{start_terminal, TerminalOutput};

#[derive(Clone)]
pub struct CoreHandle {
    pub cmd_tx: mpsc::Sender<Command>,
    pub evt_tx: broadcast::Sender<Event>,
}

pub fn spawn_core() -> CoreHandle {
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<Command>(256);
    let cmd_tx_internal = cmd_tx.clone();
    let (evt_tx, _) = broadcast::channel::<Event>(256);
    let evt_tx_task = evt_tx.clone();

    tokio::spawn(async move {
        let mut state = AppState::default();
        restore_workspaces(&mut state, &evt_tx_task).await;
        let _ = evt_tx_task.send(Event::WorkspaceList {
            items: workspace_summaries(&state),
        });
        let mut git_tick = tokio::time::interval(Duration::from_secs(2));

        loop {
            tokio::select! {
                maybe_cmd = cmd_rx.recv() => {
                    let Some(cmd) = maybe_cmd else { break; };
                    match cmd {
                Command::SetRoute(route) => state.route = route,
                Command::AddWorkspace { name, path } => {
                    let id = Uuid::new_v4();
                    let repo_path = std::path::PathBuf::from(path);
                    let initial_git = refresh_git(&repo_path).await.unwrap_or_default();
                    let ws = Workspace {
                        id,
                        name,
                        path: repo_path,
                        git: initial_git.clone(),
                        attention: AttentionLevel::None,
                        terminals: Default::default(),
                        last_activity: Instant::now(),
                    };
                    state.ordered_ids.push(id);
                    state.workspaces.insert(id, ws);
                    let _ = evt_tx_task.send(Event::WorkspaceGitUpdated {
                        id,
                        git: initial_git,
                    });
                }
                Command::RemoveWorkspace { id } => {
                    state.workspaces.remove(&id);
                    state.ordered_ids.retain(|wid| *wid != id);
                }
                Command::RenameWorkspace { id, name } => {
                    if let Some(ws) = state.workspaces.get_mut(&id) {
                        ws.name = name;
                        ws.last_activity = Instant::now();
                    }
                }
                Command::SetAttention { id, level } => {
                    if let Some(ws) = state.workspaces.get_mut(&id) {
                        ws.attention = level;
                        let _ = evt_tx_task.send(Event::WorkspaceAttentionChanged { id, level });
                    }
                }
                Command::ClearAttention { id } => {
                    if let Some(ws) = state.workspaces.get_mut(&id) {
                        ws.attention = AttentionLevel::None;
                        let _ = evt_tx_task.send(Event::WorkspaceAttentionChanged {
                            id,
                            level: AttentionLevel::None,
                        });
                    }
                }
                Command::RefreshGit { id } => {
                    if let Some(path) = state.workspaces.get(&id).map(|ws| ws.path.clone()) {
                        match refresh_git(&path).await {
                            Ok(git) => {
                                if let Some(ws) = state.workspaces.get_mut(&id) {
                                    ws.git = git.clone();
                                    ws.last_activity = Instant::now();
                                }
                                let _ = evt_tx_task.send(Event::WorkspaceGitUpdated { id, git });
                            }
                            Err(err) => {
                                let _ = evt_tx_task.send(Event::Error {
                                    message: format!(
                                        "RefreshGit failed for {}: {err}",
                                        path.display()
                                    ),
                                });
                            }
                        }
                    }
                }
                Command::LoadDiff { id, file } => {
                    if let Some(path) = state.workspaces.get(&id).map(|ws| ws.path.clone()) {
                        match diff_file(&path, &file).await {
                            Ok(diff) => {
                                let _ = evt_tx_task.send(Event::WorkspaceDiffUpdated {
                                    id,
                                    file,
                                    diff,
                                });
                            }
                            Err(err) => {
                                let _ = evt_tx_task.send(Event::Error {
                                    message: format!(
                                        "LoadDiff failed for {}: {err}",
                                        path.display()
                                    ),
                                });
                            }
                        }
                    }
                }
                Command::StartTerminal {
                    id,
                    kind,
                    tab_id,
                    cmd,
                } => {
                    if let Some(ws) = state.workspaces.get_mut(&id) {
                        let cwd = ws.path.clone();
                        let command = if cmd.is_empty() {
                            default_terminal_cmd(kind)
                        } else {
                            cmd
                        };

                        let tid = normalize_tab_id(kind, tab_id);
                        match kind {
                            protocol::TerminalKind::Agent => {
                                if let Some(existing) = ws.terminals.agent.take() {
                                    let _ = existing.stop().await;
                                }
                            }
                            protocol::TerminalKind::Shell => {
                                if let Some(existing) = ws.terminals.shells.remove(&tid) {
                                    let _ = existing.stop().await;
                                }
                            }
                        }

                        match start_terminal(cwd, command).await {
                            Ok((session, mut out_rx)) => {
                                match kind {
                                    protocol::TerminalKind::Agent => ws.terminals.agent = Some(session),
                                    protocol::TerminalKind::Shell => {
                                        ws.terminals.shells.insert(tid.clone(), session);
                                    }
                                }
                                ws.last_activity = Instant::now();
                                let _ = evt_tx_task.send(Event::TerminalStarted {
                                    id,
                                    kind,
                                    tab_id: Some(tid.clone()),
                                });

                                let evt_tx_outputs = evt_tx_task.clone();
                                let cmd_tx_outputs = cmd_tx_internal.clone();
                                let out_tab_id = tid.clone();
                                tokio::spawn(async move {
                                    let mut recent_agent_output = String::new();
                                    let mut idle_armed = false;
                                    let mut idle_triggered = false;
                                    const AGENT_IDLE_NEEDS_INPUT_SECS: u64 = 10;

                                    loop {
                                        let out = if matches!(kind, protocol::TerminalKind::Agent)
                                            && idle_armed
                                        {
                                            tokio::select! {
                                                maybe_out = out_rx.recv() => maybe_out,
                                                _ = tokio::time::sleep(Duration::from_secs(AGENT_IDLE_NEEDS_INPUT_SECS)) => {
                                                    idle_armed = false;
                                                    idle_triggered = true;
                                                    let _ = cmd_tx_outputs
                                                        .send(Command::SetAttention {
                                                            id,
                                                            level: AttentionLevel::NeedsInput,
                                                        })
                                                        .await;
                                                    continue;
                                                }
                                            }
                                        } else {
                                            out_rx.recv().await
                                        };

                                        let Some(out) = out else { break; };

                                        match out {
                                            TerminalOutput::Bytes(bytes) => {
                                                if matches!(kind, protocol::TerminalKind::Agent) {
                                                    if idle_triggered {
                                                        let _ = cmd_tx_outputs
                                                            .send(Command::ClearAttention { id })
                                                            .await;
                                                        idle_triggered = false;
                                                    }

                                                    append_recent_output(
                                                        &mut recent_agent_output,
                                                        &bytes,
                                                    );
                                                    if detect_needs_input_text(&recent_agent_output)
                                                    {
                                                        let _ = cmd_tx_outputs
                                                            .send(Command::SetAttention {
                                                                id,
                                                                level: AttentionLevel::NeedsInput,
                                                            })
                                                            .await;
                                                        // Prevent stale prompt text from re-triggering
                                                        // until fresh prompt output appears.
                                                        recent_agent_output.clear();
                                                    }
                                                    idle_armed = true;
                                                }
                                                let data_b64 =
                                                    base64::engine::general_purpose::STANDARD
                                                        .encode(bytes);
                                                let _ =
                                                    evt_tx_outputs.send(Event::TerminalOutput {
                                                        id,
                                                        kind,
                                                        tab_id: Some(out_tab_id.clone()),
                                                        data_b64,
                                                    });
                                            }
                                            TerminalOutput::Exited(code) => {
                                                let _ = evt_tx_outputs.send(Event::TerminalExited {
                                                    id,
                                                    kind,
                                                    tab_id: Some(out_tab_id.clone()),
                                                    code,
                                                });
                                                break;
                                            }
                                        }
                                    }
                                });
                            }
                            Err(err) => {
                                let _ = evt_tx_task.send(Event::Error {
                                    message: format!(
                                        "StartTerminal failed for workspace {id}: {err}"
                                    ),
                                });
                            }
                        }
                    }
                }
                Command::StopTerminal { id, kind, tab_id } => {
                    if let Some(ws) = state.workspaces.get_mut(&id) {
                        let tid = normalize_tab_id(kind, tab_id);
                        let stopped = match kind {
                            protocol::TerminalKind::Agent => ws.terminals.agent.take(),
                            protocol::TerminalKind::Shell => ws.terminals.shells.remove(&tid),
                        };
                        if let Some(session) = stopped {
                            let _ = session.stop().await;
                            let _ = evt_tx_task.send(Event::TerminalExited {
                                id,
                                kind,
                                tab_id: Some(tid),
                                code: None,
                            });
                        }
                    }
                }
                Command::SendTerminalInput {
                    id,
                    kind,
                    tab_id,
                    data_b64,
                } => {
                    if let Some(ws) = state.workspaces.get_mut(&id) {
                        let tid = normalize_tab_id(kind, tab_id);
                        let session = match kind {
                            protocol::TerminalKind::Agent => ws.terminals.agent.as_mut(),
                            protocol::TerminalKind::Shell => ws.terminals.shells.get_mut(&tid),
                        };
                        if let Some(session) = session {
                            if let Ok(bytes) =
                                base64::engine::general_purpose::STANDARD.decode(data_b64)
                            {
                                let _ = session.send_input(&bytes).await;
                                if matches!(kind, protocol::TerminalKind::Agent)
                                    && ws.attention == AttentionLevel::NeedsInput
                                {
                                    ws.attention = AttentionLevel::None;
                                    let _ = evt_tx_task.send(Event::WorkspaceAttentionChanged {
                                        id,
                                        level: AttentionLevel::None,
                                    });
                                }
                            }
                        }
                    }
                }
                Command::ResizeTerminal {
                    id,
                    kind,
                    tab_id,
                    cols,
                    rows,
                } => {
                    if let Some(ws) = state.workspaces.get_mut(&id) {
                        let tid = normalize_tab_id(kind, tab_id);
                        let session = match kind {
                            protocol::TerminalKind::Agent => ws.terminals.agent.as_mut(),
                            protocol::TerminalKind::Shell => ws.terminals.shells.get_mut(&tid),
                        };
                        if let Some(session) = session {
                            let _ = session.resize(cols, rows).await;
                        }
                    }
                }
            }

            save_workspaces(&state);
            let items = workspace_summaries(&state);
            let _ = evt_tx_task.send(Event::WorkspaceList { items });
                }
                _ = git_tick.tick() => {
                    let ids = state.ordered_ids.clone();
                    for id in ids {
                        if let Some(path) = state.workspaces.get(&id).map(|ws| ws.path.clone()) {
                            if let Ok(git) = refresh_git(&path).await {
                                if let Some(ws) = state.workspaces.get_mut(&id) {
                                    ws.git = git.clone();
                                }
                                let _ = evt_tx_task.send(Event::WorkspaceGitUpdated { id, git });
                            }
                        }
                    }
                    let _ = evt_tx_task.send(Event::WorkspaceList {
                        items: workspace_summaries(&state),
                    });
                }
            }
        }
    });

    CoreHandle { cmd_tx, evt_tx }
}

fn workspace_summaries(state: &AppState) -> Vec<WorkspaceSummary> {
    state
        .ordered_ids
        .iter()
        .filter_map(|id| state.workspaces.get(id))
        .map(|ws| WorkspaceSummary {
            id: ws.id,
            name: ws.name.clone(),
            path: ws.path.display().to_string(),
            branch: ws.git.branch.clone(),
            dirty_files: ws.git.changed.len(),
            attention: ws.attention,
            agent_running: ws.terminals.agent.is_some(),
            shell_running: !ws.terminals.shells.is_empty(),
            last_activity_unix_ms: unix_ms_now(),
        })
        .collect::<Vec<_>>()
}

fn unix_ms_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn default_terminal_cmd(kind: protocol::TerminalKind) -> Vec<String> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "zsh".to_string());
    match kind {
        protocol::TerminalKind::Agent => vec![shell.clone(), "-i".to_string()],
        protocol::TerminalKind::Shell => vec![shell, "-i".to_string()],
    }
}

fn normalize_tab_id(kind: protocol::TerminalKind, tab_id: Option<String>) -> String {
    match kind {
        protocol::TerminalKind::Agent => "agent".to_string(),
        protocol::TerminalKind::Shell => tab_id
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "shell".to_string()),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedWorkspace {
    name: String,
    path: String,
}

fn persist_file() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    let base = PathBuf::from(home).join(".config/multiws");
    let file = if let Ok(session) = std::env::var("MULTIWS_SESSION_NAME") {
        let safe = sanitize_session_name(&session);
        format!("workspaces.{safe}.json")
    } else {
        "workspaces.json".to_string()
    };
    Some(base.join(file))
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

fn save_workspaces(state: &AppState) {
    let Some(path) = persist_file() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let items = state
        .ordered_ids
        .iter()
        .filter_map(|id| state.workspaces.get(id))
        .map(|ws| PersistedWorkspace {
            name: ws.name.clone(),
            path: ws.path.display().to_string(),
        })
        .collect::<Vec<_>>();
    if let Ok(json) = serde_json::to_string_pretty(&items) {
        let _ = fs::write(path, json);
    }
}

async fn restore_workspaces(state: &mut AppState, evt_tx: &broadcast::Sender<Event>) {
    let Some(path) = persist_file() else {
        return;
    };
    let Ok(raw) = fs::read_to_string(path) else {
        return;
    };
    let Ok(items) = serde_json::from_str::<Vec<PersistedWorkspace>>(&raw) else {
        return;
    };
    for item in items {
        let id = Uuid::new_v4();
        let repo_path = PathBuf::from(item.path);
        let initial_git = refresh_git(&repo_path).await.unwrap_or_default();
        let ws = Workspace {
            id,
            name: item.name,
            path: repo_path,
            git: initial_git.clone(),
            attention: AttentionLevel::None,
            terminals: Default::default(),
            last_activity: Instant::now(),
        };
        state.ordered_ids.push(id);
        state.workspaces.insert(id, ws);
        let _ = evt_tx.send(Event::WorkspaceGitUpdated {
            id,
            git: initial_git,
        });
    }
}
