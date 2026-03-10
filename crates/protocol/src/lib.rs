use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub type WorkspaceId = Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SshTarget {
    pub host: String,
    pub user: Option<String>,
    pub port: Option<u16>,
}

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
    pub ahead: Option<u32>,
    pub behind: Option<u32>,
    pub dirty_files: usize,
    pub attention: AttentionLevel,
    pub agent_running: bool,
    pub shell_running: bool,
    pub last_activity_unix_ms: u64,
    #[serde(default)]
    pub ssh_host: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangedFile {
    pub path: String,
    pub index_status: char,
    pub worktree_status: char,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitInfo {
    pub hash: String,
    pub message: String,
    pub author: String,
    pub date: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchInfo {
    pub name: String,
    pub is_head: bool,
    pub ahead: Option<u32>,
    pub behind: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteBranchInfo {
    pub full_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TagInfo {
    pub name: String,
    pub hash: String,
    pub date: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GitState {
    pub branch: Option<String>,
    pub upstream: Option<String>,
    pub ahead: Option<u32>,
    pub behind: Option<u32>,
    pub changed: Vec<ChangedFile>,
    pub recent_commits: Vec<CommitInfo>,
    pub local_branches: Vec<BranchInfo>,
    pub remote_branches: Vec<RemoteBranchInfo>,
    #[serde(default)]
    pub tags: Vec<TagInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Command {
    SetRoute(Route),
    AddWorkspace {
        name: String,
        path: String,
        #[serde(default)]
        ssh: Option<SshTarget>,
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
    LoadCommitDiff {
        id: WorkspaceId,
        hash: String,
    },
    GitStageFile {
        id: WorkspaceId,
        file: String,
    },
    GitUnstageFile {
        id: WorkspaceId,
        file: String,
    },
    GitStageAll {
        id: WorkspaceId,
    },
    GitUnstageAll {
        id: WorkspaceId,
    },
    GitCommit {
        id: WorkspaceId,
        message: String,
    },
    GitCheckoutBranch {
        id: WorkspaceId,
        branch: String,
    },
    GitCheckoutRemoteBranch {
        id: WorkspaceId,
        remote_branch: String,
        local_name: String,
    },
    GitCreateBranch {
        id: WorkspaceId,
        branch: String,
    },
    GitPush {
        id: WorkspaceId,
    },
    GitPull {
        id: WorkspaceId,
    },
    GitFetch {
        id: WorkspaceId,
    },
    GitDiscardFile {
        id: WorkspaceId,
        file: String,
    },
    GitStash {
        id: WorkspaceId,
        message: Option<String>,
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
    GitActionResult {
        id: WorkspaceId,
        action: String,
        success: bool,
        message: String,
    },
    Error {
        message: String,
    },
}
