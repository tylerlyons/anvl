use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub type WorkspaceId = Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Route {
    Home,
    Workspace { id: WorkspaceId },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttentionLevel {
    None,
    Notice,
    NeedsInput,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TerminalKind {
    Agent,
    Shell,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceSummary {
    pub id: WorkspaceId,
    pub name: String,
    pub path: String,
    pub branch: Option<String>,
    pub dirty_files: usize,
    pub attention: AttentionLevel,
    pub agent_running: bool,
    pub shell_running: bool,
    pub last_activity_unix_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangedFile {
    pub path: String,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GitState {
    pub branch: Option<String>,
    pub changed: Vec<ChangedFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Command {
    SetRoute(Route),
    AddWorkspace {
        name: String,
        path: String,
    },
    RemoveWorkspace {
        id: WorkspaceId,
    },
    RenameWorkspace {
        id: WorkspaceId,
        name: String,
    },
    SetAttention {
        id: WorkspaceId,
        level: AttentionLevel,
    },
    ClearAttention {
        id: WorkspaceId,
    },
    RefreshGit {
        id: WorkspaceId,
    },
    LoadDiff {
        id: WorkspaceId,
        file: String,
    },
    StartTerminal {
        id: WorkspaceId,
        kind: TerminalKind,
        #[serde(default)]
        tab_id: Option<String>,
        cmd: Vec<String>,
    },
    StopTerminal {
        id: WorkspaceId,
        kind: TerminalKind,
        #[serde(default)]
        tab_id: Option<String>,
    },
    SendTerminalInput {
        id: WorkspaceId,
        kind: TerminalKind,
        #[serde(default)]
        tab_id: Option<String>,
        data_b64: String,
    },
    ResizeTerminal {
        id: WorkspaceId,
        kind: TerminalKind,
        #[serde(default)]
        tab_id: Option<String>,
        cols: u16,
        rows: u16,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {
    WorkspaceList {
        items: Vec<WorkspaceSummary>,
    },
    WorkspaceGitUpdated {
        id: WorkspaceId,
        git: GitState,
    },
    WorkspaceDiffUpdated {
        id: WorkspaceId,
        file: String,
        diff: String,
    },
    WorkspaceAttentionChanged {
        id: WorkspaceId,
        level: AttentionLevel,
    },
    TerminalStarted {
        id: WorkspaceId,
        kind: TerminalKind,
        #[serde(default)]
        tab_id: Option<String>,
    },
    TerminalExited {
        id: WorkspaceId,
        kind: TerminalKind,
        #[serde(default)]
        tab_id: Option<String>,
        code: Option<i32>,
    },
    TerminalOutput {
        id: WorkspaceId,
        kind: TerminalKind,
        #[serde(default)]
        tab_id: Option<String>,
        data_b64: String,
    },
    Error {
        message: String,
    },
}
