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

use protocol::{AttentionLevel, Command, Event, GitState, SshTarget, WorkspaceSummary};
use state::{AppState, Workspace};
use uuid::Uuid;

/// Result of a background git refresh for one workspace.
struct GitRefreshResult {
    id: Uuid,
    result: Result<GitState, anyhow::Error>,
}

use workspace::attention::AttentionDetector;
use workspace::git::{
    checkout_branch, checkout_remote_branch, commit, create_branch, diff_commit, diff_file,
    discard_file, git_fetch, git_pull, git_push, git_stash, refresh_git, stage_all, stage_file,
    unstage_all, unstage_file,
};
use workspace::ssh;
use workspace::terminal::{start_terminal, TerminalOutput};

#[derive(Clone)]
pub struct CoreHandle {
    pub cmd_tx: mpsc::Sender<Command>,
    pub evt_tx: broadcast::Sender<Event>,
}

pub fn spawn_core() -> CoreHandle {
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<Command>(1024);
    let cmd_tx_internal = cmd_tx.clone();
    let (evt_tx, _) = broadcast::channel::<Event>(4096);
    let evt_tx_task = evt_tx.clone();

    tokio::spawn(async move {
        let mut state = AppState::default();
        restore_workspaces(&mut state, &evt_tx_task).await;
        let _ = evt_tx_task.send(Event::WorkspaceList {
            items: workspace_summaries(&state),
        });
        let mut git_tick = tokio::time::interval(Duration::from_secs(2));
        let mut git_refresh_in_flight = false;
        let (git_result_tx, mut git_result_rx) = mpsc::channel::<GitRefreshResult>(64);

        loop {
            tokio::select! {
                maybe_cmd = cmd_rx.recv() => {
                    let Some(cmd) = maybe_cmd else { break; };
                    match cmd {
                Command::SetRoute(route) => state.route = route,
                Command::AddWorkspace { name, path, ssh } => {
                    let id = Uuid::new_v4();
                    let repo_path = std::path::PathBuf::from(&path);

                    // Validate SSH connection if applicable
                    if let Some(ref target) = ssh {
                        if let Err(e) = ssh::validate_ssh_connection(target, &repo_path).await {
                            let _ = evt_tx_task.send(Event::Error {
                                message: format!("SSH workspace creation failed: {e}"),
                            });
                            continue;
                        }
                    }

                    let initial_git = refresh_git(&repo_path, ssh.as_ref()).await.unwrap_or_default();
                    let ws = Workspace {
                        id,
                        name,
                        path: repo_path,
                        ssh,
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
                    if let Some(ws) = state.workspaces.get(&id) {
                        let path = ws.path.clone();
                        let ssh = ws.ssh.clone();
                        match refresh_git(&path, ssh.as_ref()).await {
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
                    if let Some(ws) = state.workspaces.get(&id) {
                        let path = ws.path.clone();
                        let ssh = ws.ssh.clone();
                        match diff_file(&path, &file, ssh.as_ref()).await {
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
                Command::LoadCommitDiff { id, hash } => {
                    if let Some(ws) = state.workspaces.get(&id) {
                        let path = ws.path.clone();
                        let ssh = ws.ssh.clone();
                        match diff_commit(&path, &hash, ssh.as_ref()).await {
                            Ok(diff) => {
                                let _ = evt_tx_task.send(Event::WorkspaceDiffUpdated {
                                    id,
                                    file: hash,
                                    diff,
                                });
                            }
                            Err(err) => {
                                let _ = evt_tx_task.send(Event::Error {
                                    message: format!(
                                        "LoadCommitDiff failed for {}: {err}",
                                        path.display()
                                    ),
                                });
                            }
                        }
                    }
                }
                Command::GitStageFile { id, file } => {
                    if let Some(ws) = state.workspaces.get(&id) {
                        let path = ws.path.clone();
                        let ssh = ws.ssh.clone();
                        let (success, message) = match stage_file(&path, &file, ssh.as_ref()).await {
                            Ok(()) => (true, format!("Staged {file}")),
                            Err(e) => (false, e.to_string()),
                        };
                        let _ = evt_tx_task.send(Event::GitActionResult {
                            id, action: "stage".to_string(), success, message,
                        });
                        if let Ok(git) = refresh_git(&path, ssh.as_ref()).await {
                            if let Some(ws) = state.workspaces.get_mut(&id) { ws.git = git.clone(); }
                            let _ = evt_tx_task.send(Event::WorkspaceGitUpdated { id, git });
                        }
                    }
                }
                Command::GitUnstageFile { id, file } => {
                    if let Some(ws) = state.workspaces.get(&id) {
                        let path = ws.path.clone();
                        let ssh = ws.ssh.clone();
                        let (success, message) = match unstage_file(&path, &file, ssh.as_ref()).await {
                            Ok(()) => (true, format!("Unstaged {file}")),
                            Err(e) => (false, e.to_string()),
                        };
                        let _ = evt_tx_task.send(Event::GitActionResult {
                            id, action: "unstage".to_string(), success, message,
                        });
                        if let Ok(git) = refresh_git(&path, ssh.as_ref()).await {
                            if let Some(ws) = state.workspaces.get_mut(&id) { ws.git = git.clone(); }
                            let _ = evt_tx_task.send(Event::WorkspaceGitUpdated { id, git });
                        }
                    }
                }
                Command::GitStageAll { id } => {
                    if let Some(ws) = state.workspaces.get(&id) {
                        let path = ws.path.clone();
                        let ssh = ws.ssh.clone();
                        let (success, message) = match stage_all(&path, ssh.as_ref()).await {
                            Ok(()) => (true, "Staged all".to_string()),
                            Err(e) => (false, e.to_string()),
                        };
                        let _ = evt_tx_task.send(Event::GitActionResult {
                            id, action: "stage_all".to_string(), success, message,
                        });
                        if let Ok(git) = refresh_git(&path, ssh.as_ref()).await {
                            if let Some(ws) = state.workspaces.get_mut(&id) { ws.git = git.clone(); }
                            let _ = evt_tx_task.send(Event::WorkspaceGitUpdated { id, git });
                        }
                    }
                }
                Command::GitUnstageAll { id } => {
                    if let Some(ws) = state.workspaces.get(&id) {
                        let path = ws.path.clone();
                        let ssh = ws.ssh.clone();
                        let (success, message) = match unstage_all(&path, ssh.as_ref()).await {
                            Ok(()) => (true, "Unstaged all".to_string()),
                            Err(e) => (false, e.to_string()),
                        };
                        let _ = evt_tx_task.send(Event::GitActionResult {
                            id, action: "unstage_all".to_string(), success, message,
                        });
                        if let Ok(git) = refresh_git(&path, ssh.as_ref()).await {
                            if let Some(ws) = state.workspaces.get_mut(&id) { ws.git = git.clone(); }
                            let _ = evt_tx_task.send(Event::WorkspaceGitUpdated { id, git });
                        }
                    }
                }
                Command::GitCommit { id, message } => {
                    if let Some(ws) = state.workspaces.get(&id) {
                        let path = ws.path.clone();
                        let ssh = ws.ssh.clone();
                        let (success, msg) = match commit(&path, &message, ssh.as_ref()).await {
                            Ok(()) => (true, "Committed".to_string()),
                            Err(e) => (false, e.to_string()),
                        };
                        let _ = evt_tx_task.send(Event::GitActionResult {
                            id, action: "commit".to_string(), success, message: msg,
                        });
                        if let Ok(git) = refresh_git(&path, ssh.as_ref()).await {
                            if let Some(ws) = state.workspaces.get_mut(&id) { ws.git = git.clone(); }
                            let _ = evt_tx_task.send(Event::WorkspaceGitUpdated { id, git });
                        }
                    }
                }
                Command::GitCheckoutBranch { id, branch } => {
                    if let Some(ws) = state.workspaces.get(&id) {
                        let path = ws.path.clone();
                        let ssh = ws.ssh.clone();
                        let (success, msg) = match checkout_branch(&path, &branch, ssh.as_ref()).await {
                            Ok(()) => (true, format!("Checked out {branch}")),
                            Err(e) => (false, e.to_string()),
                        };
                        let _ = evt_tx_task.send(Event::GitActionResult {
                            id, action: "checkout".to_string(), success, message: msg,
                        });
                        if let Ok(git) = refresh_git(&path, ssh.as_ref()).await {
                            if let Some(ws) = state.workspaces.get_mut(&id) { ws.git = git.clone(); }
                            let _ = evt_tx_task.send(Event::WorkspaceGitUpdated { id, git });
                        }
                    }
                }
                Command::GitCheckoutRemoteBranch { id, remote_branch, local_name } => {
                    if let Some(ws) = state.workspaces.get(&id) {
                        let path = ws.path.clone();
                        let ssh = ws.ssh.clone();
                        let (success, msg) = match checkout_remote_branch(&path, &remote_branch, &local_name, ssh.as_ref()).await {
                            Ok(()) => (true, format!("Created and checked out {local_name} from {remote_branch}")),
                            Err(e) => (false, e.to_string()),
                        };
                        let _ = evt_tx_task.send(Event::GitActionResult {
                            id, action: "checkout".to_string(), success, message: msg,
                        });
                        if let Ok(git) = refresh_git(&path, ssh.as_ref()).await {
                            if let Some(ws) = state.workspaces.get_mut(&id) { ws.git = git.clone(); }
                            let _ = evt_tx_task.send(Event::WorkspaceGitUpdated { id, git });
                        }
                    }
                }
                Command::GitCreateBranch { id, branch } => {
                    if let Some(ws) = state.workspaces.get(&id) {
                        let path = ws.path.clone();
                        let ssh = ws.ssh.clone();
                        let (success, msg) = match create_branch(&path, &branch, ssh.as_ref()).await {
                            Ok(()) => (true, format!("Created and checked out {branch}")),
                            Err(e) => (false, e.to_string()),
                        };
                        let _ = evt_tx_task.send(Event::GitActionResult {
                            id, action: "create_branch".to_string(), success, message: msg,
                        });
                        if let Ok(git) = refresh_git(&path, ssh.as_ref()).await {
                            if let Some(ws) = state.workspaces.get_mut(&id) { ws.git = git.clone(); }
                            let _ = evt_tx_task.send(Event::WorkspaceGitUpdated { id, git });
                        }
                    }
                }
                Command::GitPush { id } => {
                    if let Some(ws) = state.workspaces.get(&id) {
                        let path = ws.path.clone();
                        let ssh = ws.ssh.clone();
                        let (success, msg) = match git_push(&path, ssh.as_ref()).await {
                            Ok(()) => (true, "Pushed".to_string()),
                            Err(e) => (false, e.to_string()),
                        };
                        let _ = evt_tx_task.send(Event::GitActionResult {
                            id, action: "push".to_string(), success, message: msg,
                        });
                        if let Ok(git) = refresh_git(&path, ssh.as_ref()).await {
                            if let Some(ws) = state.workspaces.get_mut(&id) { ws.git = git.clone(); }
                            let _ = evt_tx_task.send(Event::WorkspaceGitUpdated { id, git });
                        }
                    }
                }
                Command::GitPull { id } => {
                    if let Some(ws) = state.workspaces.get(&id) {
                        let path = ws.path.clone();
                        let ssh = ws.ssh.clone();
                        let (success, msg) = match git_pull(&path, ssh.as_ref()).await {
                            Ok(()) => (true, "Pulled".to_string()),
                            Err(e) => (false, e.to_string()),
                        };
                        let _ = evt_tx_task.send(Event::GitActionResult {
                            id, action: "pull".to_string(), success, message: msg,
                        });
                        if let Ok(git) = refresh_git(&path, ssh.as_ref()).await {
                            if let Some(ws) = state.workspaces.get_mut(&id) { ws.git = git.clone(); }
                            let _ = evt_tx_task.send(Event::WorkspaceGitUpdated { id, git });
                        }
                    }
                }
                Command::GitFetch { id } => {
                    if let Some(ws) = state.workspaces.get(&id) {
                        let path = ws.path.clone();
                        let ssh = ws.ssh.clone();
                        let (success, msg) = match git_fetch(&path, ssh.as_ref()).await {
                            Ok(()) => (true, "Fetched".to_string()),
                            Err(e) => (false, e.to_string()),
                        };
                        let _ = evt_tx_task.send(Event::GitActionResult {
                            id, action: "fetch".to_string(), success, message: msg,
                        });
                        if let Ok(git) = refresh_git(&path, ssh.as_ref()).await {
                            if let Some(ws) = state.workspaces.get_mut(&id) { ws.git = git.clone(); }
                            let _ = evt_tx_task.send(Event::WorkspaceGitUpdated { id, git });
                        }
                    }
                }
                Command::GitDiscardFile { id, file } => {
                    if let Some(ws) = state.workspaces.get(&id) {
                        let path = ws.path.clone();
                        let ssh = ws.ssh.clone();
                        let (idx_status, wt_status) = ws.git.changed.iter()
                            .find(|c| c.path == file)
                            .map(|c| (c.index_status, c.worktree_status))
                            .unwrap_or((' ', ' '));
                        let (success, msg) = match discard_file(&path, &file, idx_status, wt_status, ssh.as_ref()).await {
                            Ok(()) => (true, format!("Discarded {file}")),
                            Err(e) => (false, e.to_string()),
                        };
                        let _ = evt_tx_task.send(Event::GitActionResult {
                            id, action: "discard".to_string(), success, message: msg,
                        });
                        if let Ok(git) = refresh_git(&path, ssh.as_ref()).await {
                            if let Some(ws) = state.workspaces.get_mut(&id) { ws.git = git.clone(); }
                            let _ = evt_tx_task.send(Event::WorkspaceGitUpdated { id, git });
                        }
                    }
                }
                Command::GitStash { id, message } => {
                    if let Some(ws) = state.workspaces.get(&id) {
                        let path = ws.path.clone();
                        let ssh = ws.ssh.clone();
                        let (success, msg) = match git_stash(&path, message.as_deref(), ssh.as_ref()).await {
                            Ok(()) => (true, "Stashed".to_string()),
                            Err(e) => (false, e.to_string()),
                        };
                        let _ = evt_tx_task.send(Event::GitActionResult {
                            id, action: "stash".to_string(), success, message: msg,
                        });
                        if let Ok(git) = refresh_git(&path, ssh.as_ref()).await {
                            if let Some(ws) = state.workspaces.get_mut(&id) { ws.git = git.clone(); }
                            let _ = evt_tx_task.send(Event::WorkspaceGitUpdated { id, git });
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
                        let ssh_target = ws.ssh.clone();
                        let command = if cmd.is_empty() {
                            default_terminal_cmd(kind)
                        } else {
                            cmd
                        };

                        let tid = normalize_tab_id(kind, tab_id);
                        let already_running = match kind {
                            protocol::TerminalKind::Agent => ws
                                .terminals
                                .agent
                                .as_ref()
                                .map(|s| s.is_alive())
                                .unwrap_or(false),
                            protocol::TerminalKind::Shell => ws
                                .terminals
                                .shells
                                .get(&tid)
                                .map(|s| s.is_alive())
                                .unwrap_or(false),
                        };
                        if already_running {
                            ws.last_activity = Instant::now();
                        } else {
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

                            match start_terminal(cwd, command, ssh_target.as_ref()).await {
                                Ok((session, mut out_rx)) => {
                                    match kind {
                                        protocol::TerminalKind::Agent => {
                                            ws.terminals.agent = Some(session)
                                        }
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
                                    let mut detector = AttentionDetector::new();
                                    let mut attention_active = false;
                                    const SETTLE_MS: u64 = 500;

                                    let is_agent = matches!(kind, protocol::TerminalKind::Agent);
                                    let mut settle_deadline: Option<tokio::time::Instant> = None;

                                    loop {
                                        let out = if is_agent {
                                            if let Some(deadline) = settle_deadline {
                                                tokio::select! {
                                                    maybe_out = out_rx.recv() => { maybe_out }
                                                    _ = tokio::time::sleep_until(deadline) => {
                                                        settle_deadline = None;
                                                        if detector.check_for_prompt() {
                                                            if !attention_active {
                                                                attention_active = true;
                                                                let _ = cmd_tx_outputs
                                                                    .send(Command::SetAttention {
                                                                        id,
                                                                        level: AttentionLevel::NeedsInput,
                                                                    })
                                                                    .await;
                                                            }
                                                        } else if attention_active {
                                                            attention_active = false;
                                                            let _ = cmd_tx_outputs
                                                                .send(Command::ClearAttention { id })
                                                                .await;
                                                        }
                                                        continue;
                                                    }
                                                }
                                            } else {
                                                out_rx.recv().await
                                            }
                                        } else {
                                            out_rx.recv().await
                                        };

                                        let Some(out) = out else { break; };

                                        match out {
                                            TerminalOutput::Bytes(bytes) => {
                                                if is_agent {
                                                    let has_content = detector.append(&bytes);
                                                    if has_content {
                                                        settle_deadline = Some(
                                                            tokio::time::Instant::now()
                                                                + Duration::from_millis(SETTLE_MS),
                                                        );
                                                        if attention_active {
                                                            attention_active = false;
                                                            let _ = cmd_tx_outputs
                                                                .send(Command::ClearAttention { id })
                                                                .await;
                                                        }
                                                    }
                                                    // ANSI-only: has_content=false → settle_deadline unchanged
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
                _ = git_tick.tick(), if !git_refresh_in_flight => {
                    let pairs: Vec<_> = state.ordered_ids.iter()
                        .filter_map(|id| state.workspaces.get(id).map(|ws| (*id, ws.path.clone(), ws.ssh.clone())))
                        .collect();
                    if !pairs.is_empty() {
                        git_refresh_in_flight = true;
                        let tx = git_result_tx.clone();
                        tokio::spawn(async move {
                            let results = futures::future::join_all(
                                pairs.into_iter().map(|(id, path, ssh)| {
                                    async move {
                                        GitRefreshResult { id, result: refresh_git(&path, ssh.as_ref()).await }
                                    }
                                })
                            ).await;
                            for r in results {
                                let _ = tx.send(r).await;
                            }
                        });
                    }
                }
                Some(gr) = git_result_rx.recv() => {
                    if let Ok(git) = gr.result {
                        if let Some(ws) = state.workspaces.get_mut(&gr.id) {
                            ws.git = git.clone();
                        }
                        let _ = evt_tx_task.send(Event::WorkspaceGitUpdated { id: gr.id, git });
                    }
                    // When channel is drained (no more pending), clear in-flight flag
                    if git_result_rx.is_empty() {
                        git_refresh_in_flight = false;
                        let _ = evt_tx_task.send(Event::WorkspaceList {
                            items: workspace_summaries(&state),
                        });
                    }
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
        .map(|ws| {
            let ssh_host = ws.ssh.as_ref().map(|t| ssh::ssh_destination(t));
            WorkspaceSummary {
                id: ws.id,
                name: ws.name.clone(),
                path: ws.path.display().to_string(),
                branch: ws.git.branch.clone(),
                ahead: ws.git.ahead,
                behind: ws.git.behind,
                dirty_files: ws.git.changed.len(),
                attention: ws.attention,
                agent_running: ws.terminals.agent.is_some(),
                shell_running: !ws.terminals.shells.is_empty(),
                last_activity_unix_ms: unix_ms_now(),
                ssh_host,
            }
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
    #[serde(default)]
    ssh: Option<SshTarget>,
}

fn persist_file() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    let base = PathBuf::from(home).join(".config/anvl");
    let file = if let Ok(session) = std::env::var("ANVL_SESSION_NAME") {
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
            ssh: ws.ssh.clone(),
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
        let initial_git = refresh_git(&repo_path, item.ssh.as_ref()).await.unwrap_or_default();
        let ws = Workspace {
            id,
            name: item.name,
            path: repo_path,
            ssh: item.ssh,
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
