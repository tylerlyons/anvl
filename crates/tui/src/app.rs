use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use protocol::{AttentionLevel, BranchInfo, GitState, RemoteBranchInfo, Route, TerminalKind, WorkspaceId, WorkspaceSummary};
use ratatui::{
    style::{Color as TuiColor, Modifier, Style},
    text::{Line, Span},
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    HomeGrid,
    WsFiles,
    WsLog,
    WsBranches,
    WsDiff,
    WsTerminal,
    WsTerminalTabs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BranchSubPane {
    Local,
    Remote,
}

pub struct TuiApp {
    pub route: Route,
    pub focus: Focus,
    pub workspaces: Vec<WorkspaceSummary>,
    pub workspace_git: HashMap<WorkspaceId, GitState>,
    pub workspace_diff: HashMap<WorkspaceId, (String, String)>,
    pub terminal_state: HashMap<WorkspaceId, WorkspaceTerminalState>,
    pub last_resize_sent: HashMap<(WorkspaceId, String), (u16, u16)>,
    pub workspace_tabs: HashMap<WorkspaceId, WorkspaceTabsState>,
    pub saved_tabs_by_path: HashMap<String, PersistedWorkspaceTabs>,
    pub ws_tabs: Vec<TerminalTab>,
    pub ws_active_tab: usize,
    pub ws_next_shell_tab: u32,
    pub home_selected: usize,
    pub ws_selected_file: usize,
    pub ws_selected_commit: usize,
    pub ws_selected_local_branch: usize,
    pub ws_selected_remote_branch: usize,
    pub ws_branch_sub_pane: BranchSubPane,
    pub ws_pending_select_head_branch: bool,
    pub ws_diff_scroll: u16,
    pub flash_on: bool,
    pub add_workspace_path_input: Option<String>,
    pub pending_delete_workspace: Option<WorkspaceId>,
    pub rename_workspace_input: Option<String>,
    pub rename_tab_input: Option<String>,
    pub git_action_message: Option<(String, Instant)>,
    pub commit_input: Option<String>,
    pub create_branch_input: Option<String>,
    pub settings: Settings,
    /// Workspace IDs with an in-flight git network operation (pull/push/fetch).
    /// Set on command dispatch, cleared when `GitActionResult` arrives.
    pub git_op_in_progress: HashSet<WorkspaceId>,
    pub settings_open: bool,
    pub settings_selected: usize,
}

impl Default for TuiApp {
    fn default() -> Self {
        Self {
            route: Route::Home,
            focus: Focus::HomeGrid,
            workspaces: Vec::new(),
            workspace_git: HashMap::new(),
            workspace_diff: HashMap::new(),
            terminal_state: HashMap::new(),
            last_resize_sent: HashMap::new(),
            workspace_tabs: HashMap::new(),
            saved_tabs_by_path: load_saved_tabs_by_path(),
            ws_tabs: vec![
                TerminalTab::agent(),
                TerminalTab::shell("shell".to_string(), "shell".to_string()),
            ],
            ws_active_tab: 1,
            ws_next_shell_tab: 2,
            home_selected: 0,
            ws_selected_file: 0,
            ws_selected_commit: 0,
            ws_selected_local_branch: 0,
            ws_selected_remote_branch: 0,
            ws_branch_sub_pane: BranchSubPane::Local,
            ws_pending_select_head_branch: false,
            ws_diff_scroll: 0,
            flash_on: false,
            add_workspace_path_input: None,
            pending_delete_workspace: None,
            rename_workspace_input: None,
            rename_tab_input: None,
            git_action_message: None,
            commit_input: None,
            create_branch_input: None,
            git_op_in_progress: HashSet::new(),
            settings: load_settings(),
            settings_open: false,
            settings_selected: 0,
        }
    }
}

impl TuiApp {
    pub fn set_workspaces(&mut self, workspaces: Vec<WorkspaceSummary>) {
        self.persist_tabs_for_active_workspace();
        self.workspaces = workspaces;
        if self.workspaces.is_empty() {
            self.home_selected = 0;
        } else if self.home_selected >= self.workspaces.len() {
            self.home_selected = self.workspaces.len() - 1;
        }
        self.reconcile_workspace_tab_state();
    }

    pub fn selected_workspace_id(&self) -> Option<WorkspaceId> {
        self.workspaces.get(self.home_selected).map(|w| w.id)
    }

    pub fn active_workspace_id(&self) -> Option<WorkspaceId> {
        match self.route {
            Route::Workspace { id } => Some(id),
            Route::Home => None,
        }
    }

    pub fn open_workspace(&mut self, id: WorkspaceId) {
        self.persist_tabs_for_active_workspace();
        self.route = Route::Workspace { id };
        self.focus = Focus::WsTerminal;
        self.load_tabs_for_workspace(id);
    }

    pub fn go_home(&mut self) {
        self.persist_tabs_for_active_workspace();
        self.route = Route::Home;
        self.focus = Focus::HomeGrid;
    }

    pub fn move_home_selection(&mut self, delta: isize) {
        if self.workspaces.is_empty() {
            self.home_selected = 0;
            return;
        }

        let len = self.workspaces.len() as isize;
        let next = (self.home_selected as isize + delta).clamp(0, len - 1);
        self.home_selected = next as usize;
    }

    pub fn set_home_selection(&mut self, index: usize) {
        if self.workspaces.is_empty() {
            self.home_selected = 0;
        } else {
            self.home_selected = index.min(self.workspaces.len() - 1);
        }
    }

    pub fn active_tab(&self) -> &TerminalTab {
        &self.ws_tabs[self.ws_active_tab.min(self.ws_tabs.len().saturating_sub(1))]
    }

    pub fn active_tab_id(&self) -> String {
        self.active_tab().id.clone()
    }

    pub fn active_tab_kind(&self) -> TerminalKind {
        self.active_tab().kind
    }

    pub fn move_terminal_tab(&mut self, delta: isize) {
        if self.ws_tabs.is_empty() {
            return;
        }
        let len = self.ws_tabs.len() as isize;
        let next = (self.ws_active_tab as isize + delta).clamp(0, len - 1);
        self.ws_active_tab = next as usize;
        self.persist_tabs_for_active_workspace();
    }

    pub fn set_active_tab_index(&mut self, index: usize) {
        if self.ws_tabs.is_empty() {
            self.ws_active_tab = 0;
        } else {
            self.ws_active_tab = index.min(self.ws_tabs.len() - 1);
        }
        self.persist_tabs_for_active_workspace();
    }

    pub fn add_shell_tab(&mut self) {
        let n = self.ws_next_shell_tab;
        self.ws_next_shell_tab = self.ws_next_shell_tab.saturating_add(1);
        let id = format!("shell-{n}");
        let label = format!("shell-{n}");
        self.ws_tabs.push(TerminalTab::shell(id, label));
        self.ws_active_tab = self.ws_tabs.len() - 1;
        self.persist_tabs_for_active_workspace();
    }

    pub fn can_close_active_tab(&self) -> bool {
        self.ws_tabs
            .get(self.ws_active_tab)
            .map(|t| t.kind == TerminalKind::Shell)
            .unwrap_or(false)
            && self.ws_tabs.len() > 1
    }

    pub fn close_active_tab(&mut self) -> Option<TerminalTab> {
        if !self.can_close_active_tab() {
            return None;
        }
        let idx = self.ws_active_tab.min(self.ws_tabs.len() - 1);
        let removed = self.ws_tabs.remove(idx);
        if self.ws_active_tab >= self.ws_tabs.len() {
            self.ws_active_tab = self.ws_tabs.len().saturating_sub(1);
        }
        self.persist_tabs_for_active_workspace();
        Some(removed)
    }

    pub fn begin_rename_tab(&mut self) {
        let Some(tab) = self.ws_tabs.get(self.ws_active_tab) else {
            return;
        };
        if tab.kind != TerminalKind::Shell {
            return;
        }
        self.rename_tab_input = Some(tab.label.clone());
    }

    pub fn is_renaming_tab(&self) -> bool {
        self.rename_tab_input.is_some()
    }

    pub fn rename_tab_input_mut(&mut self) -> Option<&mut String> {
        self.rename_tab_input.as_mut()
    }

    pub fn cancel_rename_tab(&mut self) {
        self.rename_tab_input = None;
    }

    pub fn apply_rename_tab(&mut self) {
        let Some(name) = self.rename_tab_input.take() else {
            return;
        };
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return;
        }
        if let Some(tab) = self.ws_tabs.get_mut(self.ws_active_tab) {
            if tab.kind == TerminalKind::Shell {
                tab.label = trimmed.to_string();
            }
        }
        self.persist_tabs_for_active_workspace();
    }

    pub fn begin_add_workspace(&mut self, initial_path: String) {
        self.add_workspace_path_input = Some(initial_path);
    }

    pub fn cancel_add_workspace(&mut self) {
        self.add_workspace_path_input = None;
    }

    pub fn add_workspace_input_mut(&mut self) -> Option<&mut String> {
        self.add_workspace_path_input.as_mut()
    }

    pub fn is_adding_workspace(&self) -> bool {
        self.add_workspace_path_input.is_some()
    }

    pub fn begin_delete_workspace(&mut self) {
        self.pending_delete_workspace = self.selected_workspace_id();
    }

    pub fn cancel_delete_workspace(&mut self) {
        self.pending_delete_workspace = None;
    }

    pub fn is_confirming_delete(&self) -> bool {
        self.pending_delete_workspace.is_some()
    }

    pub fn take_delete_workspace(&mut self) -> Option<WorkspaceId> {
        self.pending_delete_workspace.take()
    }

    pub fn begin_rename_workspace_home(&mut self) {
        let Some(id) = self.selected_workspace_id() else {
            return;
        };
        self.rename_workspace_input = self
            .workspaces
            .iter()
            .find(|w| w.id == id)
            .map(|w| w.name.clone());
    }

    pub fn cancel_rename_workspace(&mut self) {
        self.rename_workspace_input = None;
    }

    pub fn is_renaming_workspace(&self) -> bool {
        self.rename_workspace_input.is_some()
    }

    pub fn rename_input_mut(&mut self) -> Option<&mut String> {
        self.rename_workspace_input.as_mut()
    }

    pub fn take_rename_request(&mut self) -> Option<(WorkspaceId, String)> {
        let id = self.active_workspace_id()?;
        let name = self.rename_workspace_input.take()?.trim().to_string();
        if name.is_empty() {
            return None;
        }
        Some((id, name))
    }

    pub fn take_rename_request_home(&mut self) -> Option<(WorkspaceId, String)> {
        let id = self.selected_workspace_id()?;
        let name = self.rename_workspace_input.take()?.trim().to_string();
        if name.is_empty() {
            return None;
        }
        Some((id, name))
    }

    pub fn take_add_workspace_request(&mut self) -> Option<(String, String)> {
        let path = self.add_workspace_path_input.take()?;
        let trimmed = path.trim().to_string();
        if trimmed.is_empty() {
            return None;
        }
        let name = Path::new(&trimmed)
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "workspace".to_string());
        Some((name, trimmed))
    }

    pub fn set_workspace_git(&mut self, id: WorkspaceId, git: GitState) {
        self.workspace_git.insert(id, git);
        self.clamp_selected_file();
        self.clamp_selected_branches();
    }

    pub fn set_workspace_diff(&mut self, id: WorkspaceId, file: String, diff: String) {
        self.workspace_diff.insert(id, (file, diff));
    }

    pub fn append_terminal_bytes(&mut self, id: WorkspaceId, tab_id: &str, bytes: &[u8]) {
        let is_new_ws = !self.terminal_state.contains_key(&id);
        let state = self
            .terminal_state
            .entry(id)
            .or_insert_with(WorkspaceTerminalState::new);
        let is_new_tab = !state.tabs.contains_key(tab_id);
        state.tab_mut(tab_id).append_bytes(bytes);
        if is_new_ws || is_new_tab {
            self.last_resize_sent.remove(&(id, tab_id.to_string()));
        }
    }

    pub fn reset_terminal(&mut self, id: WorkspaceId, tab_id: &str) {
        let state = self
            .terminal_state
            .entry(id)
            .or_insert_with(WorkspaceTerminalState::new);
        state.tab_mut(tab_id).reset();
        self.last_resize_sent.remove(&(id, tab_id.to_string()));
    }

    pub fn resize_terminal_parser(&mut self, id: WorkspaceId, tab_id: &str, cols: u16, rows: u16) {
        if let Some(state) = self.terminal_state.get_mut(&id) {
            if let Some(tab) = state.tabs.get_mut(tab_id) {
                tab.rebuild_for_size(cols, rows);
            }
        }
    }

    pub fn has_terminal_tab(&self, id: WorkspaceId, tab_id: &str) -> bool {
        self.terminal_state
            .get(&id)
            .and_then(|s| s.tabs.get(tab_id))
            .is_some()
    }

    pub fn scroll_terminal_scrollback(&mut self, id: WorkspaceId, tab_id: &str, delta: isize) {
        let state = self
            .terminal_state
            .entry(id)
            .or_insert_with(WorkspaceTerminalState::new);
        let parser = &mut state.tab_mut(tab_id).parser;
        let current = parser.screen().scrollback() as isize;
        let next = (current + delta).max(0) as usize;
        parser.set_scrollback(next);
    }

    pub fn should_send_resize(
        &mut self,
        id: WorkspaceId,
        tab_id: &str,
        cols: u16,
        rows: u16,
    ) -> bool {
        let key = (id, tab_id.to_string());
        let next = (cols.max(1), rows.max(1));
        if self.last_resize_sent.get(&key).copied() == Some(next) {
            return false;
        }
        self.last_resize_sent.insert(key, next);
        true
    }

    pub fn terminal_lines(&self, id: WorkspaceId, tab_id: &str) -> Vec<Line<'static>> {
        let Some(state) = self.terminal_state.get(&id) else {
            return vec![Line::from("No terminal output yet.")];
        };
        let Some(tab) = state.tabs.get(tab_id) else {
            return vec![Line::from("No terminal output yet.")];
        };
        let parser = &tab.parser;
        let screen = parser.screen();
        let (cursor_row, cursor_col) = screen.cursor_position();
        let show_cursor = !screen.hide_cursor();
        let (rows, cols) = screen.size();
        let mut lines = Vec::with_capacity(rows as usize);
        for r in 0..rows {
            let mut spans = Vec::with_capacity(cols as usize);
            for c in 0..cols {
                let Some(cell) = screen.cell(r, c) else {
                    continue;
                };
                if cell.is_wide_continuation() {
                    continue;
                }
                let mut style = Style::default();
                let fg = map_color(cell.fgcolor());
                let bg = map_color(cell.bgcolor());
                style = style.fg(fg).bg(bg);
                if cell.bold() {
                    style = style.add_modifier(Modifier::BOLD);
                }
                if cell.italic() {
                    style = style.add_modifier(Modifier::ITALIC);
                }
                if cell.underline() {
                    style = style.add_modifier(Modifier::UNDERLINED);
                }
                if cell.inverse() {
                    style = style.fg(bg).bg(fg);
                }
                if show_cursor && r == cursor_row && c == cursor_col {
                    style = style.add_modifier(Modifier::REVERSED);
                }
                let text = if cell.has_contents() {
                    cell.contents()
                } else {
                    " ".to_string()
                };
                spans.push(Span::styled(text, style));
            }
            lines.push(Line::from(spans));
        }
        lines
    }

    pub fn move_workspace_file_selection(&mut self, delta: isize) {
        let Some(id) = self.active_workspace_id() else {
            return;
        };
        let Some(git) = self.workspace_git.get(&id) else {
            self.ws_selected_file = 0;
            return;
        };
        if git.changed.is_empty() {
            self.ws_selected_file = 0;
            return;
        }
        let len = git.changed.len() as isize;
        let next = (self.ws_selected_file as isize + delta).clamp(0, len - 1);
        self.ws_selected_file = next as usize;
    }

    pub fn selected_changed_file(&self) -> Option<String> {
        let id = self.active_workspace_id()?;
        let git = self.workspace_git.get(&id)?;
        git.changed
            .get(self.ws_selected_file)
            .map(|c| c.path.clone())
    }

    pub fn move_workspace_commit_selection(&mut self, delta: isize) {
        let Some(id) = self.active_workspace_id() else {
            return;
        };
        let Some(git) = self.workspace_git.get(&id) else {
            self.ws_selected_commit = 0;
            return;
        };
        if git.recent_commits.is_empty() {
            self.ws_selected_commit = 0;
            return;
        }
        let len = git.recent_commits.len() as isize;
        let next = (self.ws_selected_commit as isize + delta).clamp(0, len - 1);
        self.ws_selected_commit = next as usize;
    }

    pub fn selected_commit_hash(&self) -> Option<String> {
        let id = self.active_workspace_id()?;
        let git = self.workspace_git.get(&id)?;
        git.recent_commits
            .get(self.ws_selected_commit)
            .map(|c| c.hash.clone())
    }

    pub fn is_committing(&self) -> bool {
        self.commit_input.is_some()
    }

    pub fn is_creating_branch(&self) -> bool {
        self.create_branch_input.is_some()
    }

    pub fn is_settings_open(&self) -> bool {
        self.settings_open
    }

    pub fn open_settings(&mut self) {
        self.settings_open = true;
        self.settings_selected = 0;
    }

    pub fn close_settings(&mut self) {
        self.settings_open = false;
    }

    pub fn toggle_selected_setting(&mut self) {
        match self.settings_selected {
            0 => self.settings.attention_notifications = !self.settings.attention_notifications,
            _ => {}
        }
        let _ = save_settings(&self.settings);
    }

    pub fn settings_count(&self) -> usize {
        1
    }

    pub fn effective_attention(&self, raw: AttentionLevel) -> AttentionLevel {
        if self.settings.attention_notifications {
            raw
        } else {
            AttentionLevel::None
        }
    }

    pub fn begin_git_op(&mut self, id: WorkspaceId) {
        self.git_op_in_progress.insert(id);
    }

    pub fn finish_git_op(&mut self, id: WorkspaceId) {
        self.git_op_in_progress.remove(&id);
    }

    pub fn is_git_op_in_progress(&self, id: WorkspaceId) -> bool {
        self.git_op_in_progress.contains(&id)
    }

    pub fn begin_create_branch(&mut self) {
        self.create_branch_input = Some(String::new());
    }

    pub fn cancel_create_branch(&mut self) {
        self.create_branch_input = None;
    }

    pub fn move_branch_selection(&mut self, delta: isize) {
        let Some(id) = self.active_workspace_id() else { return };
        let Some(git) = self.workspace_git.get(&id) else { return };
        match self.ws_branch_sub_pane {
            BranchSubPane::Local => {
                if git.local_branches.is_empty() {
                    self.ws_selected_local_branch = 0;
                    return;
                }
                let len = git.local_branches.len() as isize;
                let next = (self.ws_selected_local_branch as isize + delta).clamp(0, len - 1);
                self.ws_selected_local_branch = next as usize;
            }
            BranchSubPane::Remote => {
                if git.remote_branches.is_empty() {
                    self.ws_selected_remote_branch = 0;
                    return;
                }
                let len = git.remote_branches.len() as isize;
                let next = (self.ws_selected_remote_branch as isize + delta).clamp(0, len - 1);
                self.ws_selected_remote_branch = next as usize;
            }
        }
    }

    pub fn selected_local_branch(&self) -> Option<&BranchInfo> {
        let id = self.active_workspace_id()?;
        let git = self.workspace_git.get(&id)?;
        git.local_branches.get(self.ws_selected_local_branch)
    }

    pub fn selected_remote_branch(&self) -> Option<&RemoteBranchInfo> {
        let id = self.active_workspace_id()?;
        let git = self.workspace_git.get(&id)?;
        git.remote_branches.get(self.ws_selected_remote_branch)
    }

    pub fn toggle_branch_sub_pane(&mut self, direction: BranchSubPane) {
        self.ws_branch_sub_pane = direction;
    }

    fn clamp_selected_branches(&mut self) {
        let Some(id) = self.active_workspace_id() else { return };
        if let Some(git) = self.workspace_git.get(&id) {
            if git.local_branches.is_empty() {
                self.ws_selected_local_branch = 0;
            } else if self.ws_selected_local_branch >= git.local_branches.len() {
                self.ws_selected_local_branch = git.local_branches.len() - 1;
            }
            if git.remote_branches.is_empty() {
                self.ws_selected_remote_branch = 0;
            } else if self.ws_selected_remote_branch >= git.remote_branches.len() {
                self.ws_selected_remote_branch = git.remote_branches.len() - 1;
            }
            if self.ws_pending_select_head_branch {
                if let Some(idx) = git.local_branches.iter().position(|b| b.is_head) {
                    self.ws_selected_local_branch = idx;
                    self.ws_branch_sub_pane = BranchSubPane::Local;
                }
                self.ws_pending_select_head_branch = false;
            }
        }
    }

    fn clamp_selected_file(&mut self) {
        let Some(id) = self.active_workspace_id() else {
            return;
        };
        if let Some(git) = self.workspace_git.get(&id) {
            if git.changed.is_empty() {
                self.ws_selected_file = 0;
            } else if self.ws_selected_file >= git.changed.len() {
                self.ws_selected_file = git.changed.len() - 1;
            }
        }
    }

    fn reconcile_workspace_tab_state(&mut self) {
        let valid_ids = self
            .workspaces
            .iter()
            .map(|w| w.id)
            .collect::<std::collections::HashSet<_>>();
        self.workspace_tabs.retain(|id, _| valid_ids.contains(id));
        for ws in &self.workspaces {
            self.workspace_tabs.entry(ws.id).or_insert_with(|| {
                if let Some(saved) = self.saved_tabs_by_path.get(&ws.path) {
                    sanitize_workspace_tabs(WorkspaceTabsState::from_saved(saved))
                } else {
                    WorkspaceTabsState::default_state()
                }
            });
        }
        if let Some(id) = self.active_workspace_id() {
            self.load_tabs_for_workspace(id);
        }
    }

    fn load_tabs_for_workspace(&mut self, id: WorkspaceId) {
        let from_saved = self
            .workspace_path(id)
            .and_then(|p| self.saved_tabs_by_path.get(&p).cloned())
            .map(|saved| WorkspaceTabsState::from_saved(&saved));
        let fallback =
            sanitize_workspace_tabs(from_saved.unwrap_or_else(WorkspaceTabsState::default_state));
        let state = self.workspace_tabs.entry(id).or_insert(fallback).clone();
        self.ws_tabs = state.tabs;
        self.ws_active_tab = state.active.min(self.ws_tabs.len().saturating_sub(1));
        self.ws_next_shell_tab = state.next_shell_tab.max(2);
    }

    fn persist_tabs_for_active_workspace(&mut self) {
        let Some(id) = self.active_workspace_id() else {
            return;
        };
        let state = sanitize_workspace_tabs(WorkspaceTabsState {
            tabs: self.ws_tabs.clone(),
            active: self.ws_active_tab,
            next_shell_tab: self.ws_next_shell_tab,
        });
        self.workspace_tabs.insert(id, state.clone());
        if let Some(path) = self.workspace_path(id) {
            self.saved_tabs_by_path
                .insert(path, PersistedWorkspaceTabs::from_state(&state));
            let _ = save_saved_tabs_by_path(&self.saved_tabs_by_path);
        }
    }

    fn workspace_path(&self, id: WorkspaceId) -> Option<String> {
        self.workspaces
            .iter()
            .find(|w| w.id == id)
            .map(|w| w.path.clone())
    }
}

#[derive(Clone)]
pub struct WorkspaceTabsState {
    pub tabs: Vec<TerminalTab>,
    pub active: usize,
    pub next_shell_tab: u32,
}

impl WorkspaceTabsState {
    fn default_state() -> Self {
        Self {
            tabs: vec![
                TerminalTab::agent(),
                TerminalTab::shell("shell".to_string(), "shell".to_string()),
            ],
            active: 1,
            next_shell_tab: 2,
        }
    }

    fn from_saved(saved: &PersistedWorkspaceTabs) -> Self {
        Self {
            tabs: saved
                .tabs
                .iter()
                .map(|t| TerminalTab {
                    id: t.id.clone(),
                    label: t.label.clone(),
                    kind: t.kind,
                })
                .collect(),
            active: saved.active,
            next_shell_tab: saved.next_shell_tab,
        }
    }
}

fn sanitize_workspace_tabs(mut state: WorkspaceTabsState) -> WorkspaceTabsState {
    if state.tabs.is_empty() {
        return WorkspaceTabsState::default_state();
    }
    let has_agent = state.tabs.iter().any(|t| t.kind == TerminalKind::Agent);
    if !has_agent {
        state.tabs.insert(0, TerminalTab::agent());
    }
    let has_shell = state.tabs.iter().any(|t| t.kind == TerminalKind::Shell);
    if !has_shell {
        state
            .tabs
            .push(TerminalTab::shell("shell".to_string(), "shell".to_string()));
    }
    state.active = state.active.min(state.tabs.len().saturating_sub(1));
    state.next_shell_tab = state.next_shell_tab.max(2);
    state
}

pub struct WorkspaceTerminalState {
    pub tabs: HashMap<String, TerminalBufferState>,
}

impl WorkspaceTerminalState {
    fn new() -> Self {
        let mut tabs = HashMap::new();
        tabs.insert("agent".to_string(), TerminalBufferState::new());
        tabs.insert("shell".to_string(), TerminalBufferState::new());
        Self { tabs }
    }

    fn tab_mut(&mut self, tab_id: &str) -> &mut TerminalBufferState {
        self.tabs
            .entry(tab_id.to_string())
            .or_insert_with(TerminalBufferState::new)
    }
}

const MAX_TERMINAL_HISTORY_BYTES: usize = 2 * 1024 * 1024;

pub struct TerminalBufferState {
    pub parser: vt100::Parser,
    pub history: Vec<u8>,
}

impl TerminalBufferState {
    fn new() -> Self {
        Self {
            parser: make_parser(),
            history: Vec::new(),
        }
    }

    fn append_bytes(&mut self, bytes: &[u8]) {
        self.history.extend_from_slice(bytes);
        if self.history.len() > MAX_TERMINAL_HISTORY_BYTES {
            let trim = self.history.len() - MAX_TERMINAL_HISTORY_BYTES;
            self.history.drain(..trim);
        }
        self.parser.process(bytes);
    }

    fn reset(&mut self) {
        self.parser = make_parser();
        self.history.clear();
    }

    fn rebuild_for_size(&mut self, cols: u16, rows: u16) {
        let mut parser = vt100::Parser::new(rows.max(1), cols.max(1), 8000);
        parser.process(&self.history);
        self.parser = parser;
    }
}

#[derive(Clone)]
pub struct TerminalTab {
    pub id: String,
    pub label: String,
    pub kind: TerminalKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedWorkspaceTabs {
    pub tabs: Vec<PersistedTab>,
    pub active: usize,
    pub next_shell_tab: u32,
}

impl PersistedWorkspaceTabs {
    fn from_state(state: &WorkspaceTabsState) -> Self {
        Self {
            tabs: state
                .tabs
                .iter()
                .map(|t| PersistedTab {
                    id: t.id.clone(),
                    label: t.label.clone(),
                    kind: t.kind,
                })
                .collect(),
            active: state.active,
            next_shell_tab: state.next_shell_tab,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedTab {
    pub id: String,
    pub label: String,
    pub kind: TerminalKind,
}

impl TerminalTab {
    fn agent() -> Self {
        Self {
            id: "agent".to_string(),
            label: "agent".to_string(),
            kind: TerminalKind::Agent,
        }
    }

    fn shell(id: String, label: String) -> Self {
        Self {
            id,
            label,
            kind: TerminalKind::Shell,
        }
    }
}

fn make_parser() -> vt100::Parser {
    vt100::Parser::new(24, 120, 8000)
}

fn map_color(color: vt100::Color) -> TuiColor {
    match color {
        vt100::Color::Default => TuiColor::Reset,
        vt100::Color::Idx(i) => TuiColor::Indexed(i),
        vt100::Color::Rgb(r, g, b) => TuiColor::Rgb(r, g, b),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct TabsPersistFile {
    workspaces: HashMap<String, PersistedWorkspaceTabs>,
}

fn load_saved_tabs_by_path() -> HashMap<String, PersistedWorkspaceTabs> {
    let Some(path) = tabs_persist_path() else {
        return HashMap::new();
    };
    let Ok(raw) = fs::read_to_string(path) else {
        return HashMap::new();
    };
    serde_json::from_str::<TabsPersistFile>(&raw)
        .map(|f| f.workspaces)
        .unwrap_or_default()
}

fn save_saved_tabs_by_path(
    workspaces: &HashMap<String, PersistedWorkspaceTabs>,
) -> anyhow::Result<()> {
    let Some(path) = tabs_persist_path() else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = TabsPersistFile {
        workspaces: workspaces.clone(),
    };
    let raw = serde_json::to_string_pretty(&file)?;
    fs::write(path, raw)?;
    Ok(())
}

fn tabs_persist_path() -> Option<PathBuf> {
    let base = if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        PathBuf::from(xdg)
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".config")
    } else {
        return None;
    };
    Some(base.join("anvl").join("tui_tabs.json"))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default = "default_true")]
    pub attention_notifications: bool,
}

fn default_true() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            attention_notifications: true,
        }
    }
}

fn settings_persist_path() -> Option<PathBuf> {
    let base = if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        PathBuf::from(xdg)
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".config")
    } else {
        return None;
    };
    Some(base.join("anvl").join("settings.json"))
}

fn load_settings() -> Settings {
    let Some(path) = settings_persist_path() else {
        return Settings::default();
    };
    let Ok(raw) = fs::read_to_string(path) else {
        return Settings::default();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

fn save_settings(settings: &Settings) -> anyhow::Result<()> {
    let Some(path) = settings_persist_path() else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let raw = serde_json::to_string_pretty(settings)?;
    fs::write(path, raw)?;
    Ok(())
}
