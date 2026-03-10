use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
    Frame,
};

use crate::app::TuiApp;
use crate::ui::footer;
use crate::ui::widgets::tile_grid::ORANGE;
use protocol::{AttentionLevel, Route};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceHit {
    Header,
    TerminalTab(usize),
    TerminalPane,
    FilesList(usize),
    LogList(usize),
    BranchesPane(usize),
    DiffPane,
}

#[derive(Debug, Clone, Copy)]
struct WorkspaceLayout {
    header: Rect,
    terminal_tabs: Rect,
    terminal_pane: Rect,
    git_files: Rect,
    git_log: Rect,
    git_branches: Rect,
    git_diff: Rect,
    footer: Rect,
}

fn layout(area: Rect, focus: crate::app::Focus) -> WorkspaceLayout {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(2),
        ])
        .split(area);
    let body = Layout::default()
        .direction(Direction::Vertical)
        .constraints(match focus {
            crate::app::Focus::WsTerminal | crate::app::Focus::WsTerminalTabs => {
                [Constraint::Percentage(72), Constraint::Percentage(28)]
            }
            crate::app::Focus::WsFiles
            | crate::app::Focus::WsLog
            | crate::app::Focus::WsBranches
            | crate::app::Focus::WsDiff => {
                [Constraint::Percentage(35), Constraint::Percentage(65)]
            }
            _ => [Constraint::Percentage(55), Constraint::Percentage(45)],
        })
        .split(chunks[1]);
    let terminal_area = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(5), Constraint::Min(3)])
        .split(body[0]);
    let git_area = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(35), Constraint::Percentage(65)])
        .split(body[1]);

    // Split left pane into files (top) + commit log (middle) + branches (bottom)
    let left_split = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(33),
            Constraint::Percentage(34),
            Constraint::Percentage(33),
        ])
        .split(git_area[0]);

    WorkspaceLayout {
        header: chunks[0],
        terminal_tabs: terminal_area[0],
        terminal_pane: terminal_area[1],
        git_files: left_split[0],
        git_log: left_split[1],
        git_branches: left_split[2],
        git_diff: git_area[1],
        footer: chunks[2],
    }
}

/// Returns the standard focused/unfocused border style used by all non-attention panes.
fn standard_border_style(focused: bool) -> (Style, BorderType) {
    if focused {
        (
            Style::default()
                .fg(Color::LightBlue)
                .add_modifier(Modifier::BOLD),
            BorderType::Thick,
        )
    } else {
        (
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::DIM),
            BorderType::Plain,
        )
    }
}

/// Computes the border style for the terminal pane, accounting for attention level.
///
/// When the workspace has an active attention state (`NeedsInput` or `Error`) and
/// `flash_on` is true, the border flashes in the corresponding colour.  Otherwise
/// the standard focused / unfocused styling is used.
pub fn pane_border_style(
    focused: bool,
    attention: AttentionLevel,
    flash_on: bool,
) -> (Style, BorderType) {
    match attention {
        AttentionLevel::NeedsInput if flash_on => (
            Style::default()
                .fg(ORANGE)
                .add_modifier(Modifier::BOLD),
            BorderType::Thick,
        ),
        AttentionLevel::Error if flash_on => (
            Style::default()
                .fg(Color::Red)
                .add_modifier(Modifier::BOLD),
            BorderType::Thick,
        ),
        _ => standard_border_style(focused),
    }
}

/// Builds the title `Line` for the terminal pane, with an optional attention badge.
pub fn build_terminal_title_line(
    attention: AttentionLevel,
    flash_on: bool,
    passthrough: bool,
) -> Line<'static> {
    let raw_badge = if passthrough {
        Some(Span::styled(
            " [passthrough]",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ))
    } else {
        None
    };

    let mut spans = match attention {
        AttentionLevel::NeedsInput => {
            let badge_style = if flash_on {
                Style::default()
                    .fg(ORANGE)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(ORANGE)
            };
            vec![
                Span::raw("Terminal "),
                Span::styled("⚠ input", badge_style),
            ]
        }
        AttentionLevel::Error => {
            let badge_style = if flash_on {
                Style::default()
                    .fg(Color::Red)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Red)
            };
            vec![
                Span::raw("Terminal "),
                Span::styled("✖ error", badge_style),
            ]
        }
        _ => vec![Span::raw("Terminal")],
    };
    if let Some(badge) = raw_badge {
        spans.push(badge);
    }
    Line::from(spans)
}

const BRAILLE_SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn spinner_frame(tick: u8) -> &'static str {
    BRAILLE_SPINNER[(tick as usize) % BRAILLE_SPINNER.len()]
}

pub fn render(frame: &mut Frame, area: Rect, app: &TuiApp) {
    let l = layout(area, app.focus);

    let ws_id = match app.route {
        Route::Workspace { id } => Some(id),
        _ => None,
    };
    let attention = app.effective_attention(
        ws_id
            .and_then(|id| app.workspaces.iter().find(|w| w.id == id))
            .map(|w| w.attention)
            .unwrap_or(AttentionLevel::None),
    );

    let title = ws_id
        .and_then(|id| app.workspaces.iter().find(|w| w.id == id))
        .map(|w| {
            let git = ws_id.and_then(|id| app.workspace_git.get(&id));
            let branch = git.and_then(|g| g.branch.as_deref()).unwrap_or("-");
            let ab = match (git.and_then(|g| g.ahead), git.and_then(|g| g.behind)) {
                (Some(a), Some(b)) if a == 0 && b == 0 => " =".to_string(),
                (Some(a), Some(b)) => {
                    let mut s = String::new();
                    if a > 0 {
                        s.push_str(&format!(" ↑{a}"));
                    }
                    if b > 0 {
                        s.push_str(&format!(" ↓{b}"));
                    }
                    s
                }
                _ => String::new(),
            };
            format!(
                "Workspace: {} ({})  {}  ◈{}{}",
                w.name, w.path, branch, w.dirty_files, ab
            )
        })
        .unwrap_or_else(|| "Workspace".to_string());

    let (header_style, header_border_type) =
        standard_border_style(false);
    frame.render_widget(
        Paragraph::new(if let Some(name) = &app.rename_workspace_input {
            format!("{title}\nRename: {name}")
        } else {
            title
        })
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(header_style)
                .border_type(header_border_type),
        ),
        l.header,
    );

    // --- Changed Files ---
    let files = ws_id
        .and_then(|id| app.workspace_git.get(&id))
        .map(|g| g.changed.clone())
        .unwrap_or_default();
    let mut list_state = ListState::default();
    if !files.is_empty() {
        list_state.select(Some(app.ws_selected_file.min(files.len() - 1)));
    }
    let file_items = files
        .iter()
        .map(|f| {
            let idx = f.index_status;
            let wt = f.worktree_status;
            let idx_style = match idx {
                '?' => Style::default().fg(Color::Red),
                ' ' => Style::default().fg(Color::DarkGray),
                _ => Style::default().fg(Color::Green),
            };
            let wt_style = match wt {
                '?' => Style::default().fg(Color::Red),
                ' ' => Style::default().fg(Color::DarkGray),
                _ => Style::default().fg(Color::Yellow),
            };
            ListItem::new(Line::from(vec![
                Span::styled(idx.to_string(), idx_style),
                Span::styled(wt.to_string(), wt_style),
                Span::raw(format!(" {}", f.path)),
            ]))
        })
        .collect::<Vec<_>>();
    let (files_style, files_border_type) =
        standard_border_style(app.focus == crate::app::Focus::WsFiles);
    let file_list = List::new(file_items)
        .block(
            Block::default()
                .title("Changed Files")
                .borders(Borders::ALL)
                .border_style(files_style)
                .border_type(files_border_type),
        )
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Blue)
                .add_modifier(Modifier::BOLD),
        );
    frame.render_stateful_widget(file_list, l.git_files, &mut list_state);

    // --- Recent Commits ---
    let commits = ws_id
        .and_then(|id| app.workspace_git.get(&id))
        .map(|g| g.recent_commits.clone())
        .unwrap_or_default();
    let tags = ws_id
        .and_then(|id| app.workspace_git.get(&id))
        .map(|g| g.tags.clone())
        .unwrap_or_default();
    let total_log_items = commits.len() + if tags.is_empty() { 0 } else { 1 + tags.len() };
    let mut commit_list_state = ListState::default();
    if total_log_items > 0 {
        commit_list_state.select(Some(app.ws_selected_commit.min(total_log_items - 1)));
    }
    let mut commit_items: Vec<ListItem> = commits
        .iter()
        .map(|c| {
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{} ", c.hash),
                    Style::default().fg(Color::Yellow),
                ),
                Span::raw(&c.message),
                Span::styled(
                    format!(" ({}, {})", c.author, c.date),
                    Style::default().fg(Color::DarkGray),
                ),
            ]))
        })
        .collect();
    if !tags.is_empty() {
        commit_items.push(ListItem::new(Line::from(Span::styled(
            "─── Tags ───",
            Style::default().fg(Color::DarkGray).add_modifier(Modifier::DIM),
        ))));
        for t in &tags {
            commit_items.push(ListItem::new(Line::from(vec![
                Span::styled(format!("{} ", t.hash), Style::default().fg(Color::Yellow)),
                Span::raw(&t.name),
                Span::styled(format!(" ({})", t.date), Style::default().fg(Color::DarkGray)),
            ])));
        }
    }
    let (log_style, log_border_type) = standard_border_style(app.focus == crate::app::Focus::WsLog);
    let commit_list = List::new(commit_items)
        .block(
            Block::default()
                .title("Recent Commits")
                .borders(Borders::ALL)
                .border_style(log_style)
                .border_type(log_border_type),
        )
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
    frame.render_stateful_widget(commit_list, l.git_log, &mut commit_list_state);

    // --- Branches Pane ---
    {
        let branch_split = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(l.git_branches);

        let is_branches_focused = app.focus == crate::app::Focus::WsBranches;
        let local_active = matches!(app.ws_branch_sub_pane, crate::app::BranchSubPane::Local);
        let remote_active = matches!(app.ws_branch_sub_pane, crate::app::BranchSubPane::Remote);

        // Local branches
        let local_branches = ws_id
            .and_then(|id| app.workspace_git.get(&id))
            .map(|g| g.local_branches.clone())
            .unwrap_or_default();
        let mut local_list_state = ListState::default();
        if !local_branches.is_empty() {
            local_list_state.select(Some(
                app.ws_selected_local_branch.min(local_branches.len() - 1),
            ));
        }
        let local_items = local_branches
            .iter()
            .map(|b| {
                let mut spans = Vec::new();
                if b.is_head {
                    spans.push(Span::styled(
                        "* ",
                        Style::default()
                            .fg(Color::Green)
                            .add_modifier(Modifier::BOLD),
                    ));
                } else {
                    spans.push(Span::raw("  "));
                }
                let name_style = if b.is_head {
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                spans.push(Span::styled(b.name.clone(), name_style));
                let git_op_active = b.is_head && ws_id.map(|id| app.is_git_op_in_progress(id)).unwrap_or(false);
                if git_op_active {
                    // Re-style all existing spans yellow during git ops
                    let yellow_bold = Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD);
                    for s in &mut spans {
                        *s = Span::styled(s.content.clone(), yellow_bold);
                    }
                    spans.push(Span::styled(
                        format!(" {}", spinner_frame(app.spinner_tick)),
                        yellow_bold,
                    ));
                }
                // Ahead/behind indicators
                match (b.ahead, b.behind) {
                    (Some(a), Some(b_count)) if a == 0 && b_count == 0 => {
                        spans.push(Span::styled(
                            " =",
                            Style::default().add_modifier(Modifier::DIM),
                        ));
                    }
                    (ahead, behind) => {
                        if let Some(a) = ahead {
                            if a > 0 {
                                spans.push(Span::styled(
                                    format!(" \u{2191}{a}"),
                                    Style::default().fg(Color::Green),
                                ));
                            }
                        }
                        if let Some(b_count) = behind {
                            if b_count > 0 {
                                spans.push(Span::styled(
                                    format!(" \u{2193}{b_count}"),
                                    Style::default().fg(Color::Red),
                                ));
                            }
                        }
                    }
                }
                ListItem::new(Line::from(spans))
            })
            .collect::<Vec<_>>();

        let local_title = if is_branches_focused && local_active {
            "Local [*]"
        } else {
            "Local"
        };
        let (local_style, local_border_type) = if is_branches_focused && local_active {
            (
                Style::default()
                    .fg(Color::LightBlue)
                    .add_modifier(Modifier::BOLD),
                BorderType::Thick,
            )
        } else {
            (
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::DIM),
                BorderType::Plain,
            )
        };
        let local_list = List::new(local_items)
            .block(
                Block::default()
                    .title(local_title)
                    .borders(Borders::ALL)
                    .border_style(local_style)
                    .border_type(local_border_type),
            )
            .highlight_style(
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            );
        frame.render_stateful_widget(local_list, branch_split[0], &mut local_list_state);

        // Remote branches
        let remote_branches = ws_id
            .and_then(|id| app.workspace_git.get(&id))
            .map(|g| g.remote_branches.clone())
            .unwrap_or_default();
        let mut remote_list_state = ListState::default();
        if !remote_branches.is_empty() {
            remote_list_state.select(Some(
                app.ws_selected_remote_branch
                    .min(remote_branches.len() - 1),
            ));
        }
        let remote_items = remote_branches
            .iter()
            .map(|b| ListItem::new(Line::from(Span::raw(format!("  {}", b.full_name)))))
            .collect::<Vec<_>>();

        let remote_title = if is_branches_focused && remote_active {
            "Remote [*]"
        } else {
            "Remote"
        };
        let (remote_style, remote_border_type) = if is_branches_focused && remote_active {
            (
                Style::default()
                    .fg(Color::LightBlue)
                    .add_modifier(Modifier::BOLD),
                BorderType::Thick,
            )
        } else {
            (
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::DIM),
                BorderType::Plain,
            )
        };
        let remote_list = List::new(remote_items)
            .block(
                Block::default()
                    .title(remote_title)
                    .borders(Borders::ALL)
                    .border_style(remote_style)
                    .border_type(remote_border_type),
            )
            .highlight_style(
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            );
        frame.render_stateful_widget(remote_list, branch_split[1], &mut remote_list_state);
    }

    // --- Diff Pane ---
    let diff_text = ws_id
        .and_then(|id| app.workspace_diff.get(&id))
        .map(|(_, d)| d.clone())
        .unwrap_or_else(|| "Select a file and press Enter to load diff.".to_string());
    let diff_lines = diff_text
        .lines()
        .map(|line| {
            if line.starts_with('+') {
                Line::from(Span::styled(
                    line.to_string(),
                    Style::default().fg(Color::Green),
                ))
            } else if line.starts_with('-') {
                Line::from(Span::styled(
                    line.to_string(),
                    Style::default().fg(Color::Red),
                ))
            } else {
                Line::from(Span::raw(line.to_string()))
            }
        })
        .collect::<Vec<_>>();
    let (diff_style, diff_border_type) =
        standard_border_style(app.focus == crate::app::Focus::WsDiff);
    frame.render_widget(
        Paragraph::new(diff_lines)
            .block(
                Block::default()
                    .title("Diff")
                    .borders(Borders::ALL)
                    .border_style(diff_style)
                    .border_type(diff_border_type),
            )
            .scroll((app.ws_diff_scroll, 0))
            .wrap(Wrap { trim: false }),
        l.git_diff,
    );

    // --- Terminal Tabs ---
    let ws_summary = ws_id.and_then(|id| app.workspaces.iter().find(|w| w.id == id));
    let (agent_running, shell_running) = ws_summary
        .map(|w| (w.agent_running, w.shell_running))
        .unwrap_or((false, false));
    let (tabs_border_style, tabs_border_type) =
        standard_border_style(app.focus == crate::app::Focus::WsTerminalTabs);
    let tabs_block = Block::default()
        .title("Tabs")
        .borders(Borders::ALL)
        .border_style(tabs_border_style)
        .border_type(tabs_border_type);
    let tabs_inner = tabs_block.inner(l.terminal_tabs);
    frame.render_widget(tabs_block, l.terminal_tabs);

    let tab_rects = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(
            app.ws_tabs
                .iter()
                .map(|_| Constraint::Ratio(1, app.ws_tabs.len().max(1) as u32))
                .collect::<Vec<_>>(),
        )
        .split(tabs_inner);
    for (i, tab) in app.ws_tabs.iter().enumerate() {
        let running = match tab.kind {
            protocol::TerminalKind::Agent => agent_running,
            protocol::TerminalKind::Shell => shell_running,
        };
        let label = if i == app.ws_active_tab {
            app.rename_tab_input
                .as_ref()
                .cloned()
                .unwrap_or_else(|| tab.label.clone())
        } else {
            tab.label.clone()
        };
        let is_active = i == app.ws_active_tab;
        let is_agent = matches!(tab.kind, protocol::TerminalKind::Agent);
        let (border_style, border_type) = if is_active
            && is_agent
            && matches!(attention, AttentionLevel::NeedsInput | AttentionLevel::Error)
            && app.spinner_tick % 2 == 0
        {
            let color = match attention {
                AttentionLevel::Error => Color::Red,
                _ => ORANGE,
            };
            (
                Style::default().fg(color).add_modifier(Modifier::BOLD),
                BorderType::Thick,
            )
        } else if is_active {
            (
                Style::default()
                    .fg(Color::Blue)
                    .add_modifier(Modifier::BOLD),
                BorderType::Thick,
            )
        } else {
            (
                Style::default().add_modifier(Modifier::DIM),
                BorderType::Plain,
            )
        };
        let status = if running { "run" } else { "stop" };
        let content = Line::from(format!("{label} {status}"));
        let tab_block = Block::default()
            .title(format!("{}", i + 1))
            .borders(Borders::ALL)
            .border_style(border_style)
            .border_type(border_type);
        frame.render_widget(Paragraph::new(content).block(tab_block), tab_rects[i]);
    }

    // --- Terminal Pane ---
    let terminal_focused = app.focus == crate::app::Focus::WsTerminal;
    let terminal_lines = ws_id
        .map(|id| app.terminal_lines(id, &app.active_tab_id()))
        .unwrap_or_else(|| vec![Line::from("No terminal output yet.")]);
    let (term_style, term_border_type) =
        pane_border_style(terminal_focused, attention, app.spinner_tick % 2 == 0);
    let term_title = build_terminal_title_line(attention, app.spinner_tick % 2 == 0, app.active_tab_passthrough());
    frame.render_widget(Clear, l.terminal_pane);
    frame.render_widget(
        Paragraph::new(terminal_lines).block(
            Block::default()
                .title(term_title)
                .borders(Borders::ALL)
                .border_style(term_style)
                .border_type(term_border_type),
        ),
        l.terminal_pane,
    );

    // --- Footer ---
    footer::render(frame, l.footer, app);

    // --- Toast overlay for git action messages ---
    if let Some((msg, ts)) = &app.git_action_message {
        if ts.elapsed() < std::time::Duration::from_secs(3) {
            let toast_width = (msg.len() as u16 + 4).min(area.width);
            let toast_rect = Rect::new(
                area.x + area.width.saturating_sub(toast_width) - 1,
                area.y + area.height.saturating_sub(4),
                toast_width,
                3,
            );
            frame.render_widget(Clear, toast_rect);
            frame.render_widget(
                Paragraph::new(msg.as_str()).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::Green)),
                ),
                toast_rect,
            );
        }
    }

    // --- Create Branch modal ---
    if let Some(input) = &app.create_branch_input {
        let modal_w = 60u16.min(area.width.saturating_sub(4));
        let modal_h = 5u16;
        let modal_rect = Rect::new(
            area.x + (area.width.saturating_sub(modal_w)) / 2,
            area.y + (area.height.saturating_sub(modal_h)) / 2,
            modal_w,
            modal_h,
        );
        frame.render_widget(Clear, modal_rect);
        frame.render_widget(
            Paragraph::new(format!("{input}_"))
                .block(
                    Block::default()
                        .title("New Branch (Enter to create, Esc to cancel)")
                        .borders(Borders::ALL)
                        .border_style(
                            Style::default()
                                .fg(Color::LightBlue)
                                .add_modifier(Modifier::BOLD),
                        )
                        .border_type(BorderType::Thick),
                )
                .wrap(Wrap { trim: false }),
            modal_rect,
        );
    }

    // --- Commit modal ---
    if let Some(input) = &app.commit_input {
        let modal_w = 60u16.min(area.width.saturating_sub(4));
        let modal_h = 5u16;
        let modal_rect = Rect::new(
            area.x + (area.width.saturating_sub(modal_w)) / 2,
            area.y + (area.height.saturating_sub(modal_h)) / 2,
            modal_w,
            modal_h,
        );
        frame.render_widget(Clear, modal_rect);
        frame.render_widget(
            Paragraph::new(format!("{input}_"))
                .block(
                    Block::default()
                        .title("Commit Message (Enter to commit, Esc to cancel)")
                        .borders(Borders::ALL)
                        .border_style(
                            Style::default()
                                .fg(Color::LightBlue)
                                .add_modifier(Modifier::BOLD),
                        )
                        .border_type(BorderType::Thick),
                )
                .wrap(Wrap { trim: false }),
            modal_rect,
        );
    }

    // --- Discard confirmation modal ---
    if let Some(file) = &app.confirm_discard_file {
        let modal_w = 60u16.min(area.width.saturating_sub(4));
        let modal_h = 5u16;
        let modal_rect = Rect::new(
            area.x + (area.width.saturating_sub(modal_w)) / 2,
            area.y + (area.height.saturating_sub(modal_h)) / 2,
            modal_w,
            modal_h,
        );
        frame.render_widget(Clear, modal_rect);
        frame.render_widget(
            Paragraph::new(format!("Discard changes to {file}?"))
                .block(
                    Block::default()
                        .title("Confirm (y/Enter = yes, n/Esc = cancel)")
                        .borders(Borders::ALL)
                        .border_style(
                            Style::default()
                                .fg(Color::Red)
                                .add_modifier(Modifier::BOLD),
                        )
                        .border_type(BorderType::Thick),
                )
                .wrap(Wrap { trim: false }),
            modal_rect,
        );
    }

    // --- Stash message modal ---
    if let Some(input) = &app.stash_input {
        let modal_w = 60u16.min(area.width.saturating_sub(4));
        let modal_h = 5u16;
        let modal_rect = Rect::new(
            area.x + (area.width.saturating_sub(modal_w)) / 2,
            area.y + (area.height.saturating_sub(modal_h)) / 2,
            modal_w,
            modal_h,
        );
        frame.render_widget(Clear, modal_rect);
        frame.render_widget(
            Paragraph::new(format!("{input}_"))
                .block(
                    Block::default()
                        .title("Stash Message (Enter to stash, Esc to cancel)")
                        .borders(Borders::ALL)
                        .border_style(
                            Style::default()
                                .fg(Color::LightBlue)
                                .add_modifier(Modifier::BOLD),
                        )
                        .border_type(BorderType::Thick),
                )
                .wrap(Wrap { trim: false }),
            modal_rect,
        );
    }
}

pub fn hit_test(area: Rect, app: &TuiApp, x: u16, y: u16) -> Option<WorkspaceHit> {
    let l = layout(area, app.focus);

    let point_inside = |r: Rect| x >= r.x && y >= r.y && x < r.right() && y < r.bottom();
    if point_inside(l.header) {
        return Some(WorkspaceHit::Header);
    }
    if point_inside(l.terminal_tabs) {
        if app.ws_tabs.is_empty() {
            return Some(WorkspaceHit::TerminalTab(0));
        }
        let tab_w = (l.terminal_tabs.width / app.ws_tabs.len() as u16).max(1);
        let idx = ((x.saturating_sub(l.terminal_tabs.x)) / tab_w) as usize;
        return Some(WorkspaceHit::TerminalTab(idx.min(app.ws_tabs.len() - 1)));
    }
    if point_inside(l.terminal_pane) {
        return Some(WorkspaceHit::TerminalPane);
    }
    if point_inside(l.git_diff) {
        return Some(WorkspaceHit::DiffPane);
    }
    if point_inside(l.git_log) {
        let ws_id = match app.route {
            Route::Workspace { id } => id,
            _ => return None,
        };
        let commit_count = app
            .workspace_git
            .get(&ws_id)
            .map(|g| g.recent_commits.len())
            .unwrap_or(0);
        if commit_count == 0 {
            return Some(WorkspaceHit::LogList(0));
        }
        let content_top = l.git_log.y.saturating_add(1);
        if y < content_top {
            return Some(WorkspaceHit::LogList(0));
        }
        let idx = (y - content_top) as usize;
        return Some(WorkspaceHit::LogList(idx.min(commit_count - 1)));
    }
    if point_inside(l.git_branches) {
        let content_top = l.git_branches.y.saturating_add(1);
        if y < content_top {
            return Some(WorkspaceHit::BranchesPane(0));
        }
        let idx = (y - content_top) as usize;
        return Some(WorkspaceHit::BranchesPane(idx));
    }
    if point_inside(l.git_files) {
        let ws_id = match app.route {
            Route::Workspace { id } => id,
            _ => return None,
        };
        let file_count = app
            .workspace_git
            .get(&ws_id)
            .map(|g| g.changed.len())
            .unwrap_or(0);
        if file_count == 0 {
            return Some(WorkspaceHit::FilesList(0));
        }

        let content_top = l.git_files.y.saturating_add(1);
        if y < content_top {
            return Some(WorkspaceHit::FilesList(0));
        }
        let idx = (y - content_top) as usize;
        return Some(WorkspaceHit::FilesList(idx.min(file_count - 1)));
    }
    None
}

pub fn terminal_content_rect(area: Rect, focus: crate::app::Focus) -> Rect {
    let pane = layout(area, focus).terminal_pane;
    Rect::new(
        pane.x.saturating_add(1),
        pane.y.saturating_add(1),
        pane.width.saturating_sub(2),
        pane.height.saturating_sub(2),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- pane_border_style tests ---

    #[test]
    fn pane_border_no_attention_unfocused() {
        let (style, border_type) = pane_border_style(false, AttentionLevel::None, false);
        assert_eq!(border_type, BorderType::Plain);
        assert_eq!(style.fg, Some(Color::White));
        assert!(style.add_modifier.contains(Modifier::DIM));
    }

    #[test]
    fn pane_border_no_attention_focused() {
        let (style, border_type) = pane_border_style(true, AttentionLevel::None, false);
        assert_eq!(border_type, BorderType::Thick);
        assert_eq!(style.fg, Some(Color::LightBlue));
        assert!(style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn pane_border_needs_input_flash_on() {
        let (style, border_type) = pane_border_style(true, AttentionLevel::NeedsInput, true);
        assert_eq!(border_type, BorderType::Thick);
        assert_eq!(style.fg, Some(ORANGE));
        assert!(style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn pane_border_needs_input_flash_off() {
        // flash_off reverts to focused style
        let (style, border_type) = pane_border_style(true, AttentionLevel::NeedsInput, false);
        assert_eq!(border_type, BorderType::Thick);
        assert_eq!(style.fg, Some(Color::LightBlue));
        assert!(style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn pane_border_error_flash_on() {
        let (style, border_type) = pane_border_style(false, AttentionLevel::Error, true);
        assert_eq!(border_type, BorderType::Thick);
        assert_eq!(style.fg, Some(Color::Red));
        assert!(style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn pane_border_error_flash_off_unfocused() {
        let (style, border_type) = pane_border_style(false, AttentionLevel::Error, false);
        assert_eq!(border_type, BorderType::Plain);
        assert_eq!(style.fg, Some(Color::White));
        assert!(style.add_modifier.contains(Modifier::DIM));
    }

    #[test]
    fn pane_border_notice_no_flash() {
        // Notice level does not trigger attention flash
        let (style, border_type) = pane_border_style(true, AttentionLevel::Notice, true);
        assert_eq!(border_type, BorderType::Thick);
        assert_eq!(style.fg, Some(Color::LightBlue));
        assert!(style.add_modifier.contains(Modifier::BOLD));
    }

    // --- build_terminal_title_line tests ---

    #[test]
    fn terminal_title_no_attention() {
        let line = build_terminal_title_line(AttentionLevel::None, false, false);
        assert_eq!(line.spans.len(), 1);
        assert_eq!(line.spans[0].content, "Terminal");
    }

    #[test]
    fn terminal_title_needs_input() {
        let line = build_terminal_title_line(AttentionLevel::NeedsInput, true, false);
        assert_eq!(line.spans.len(), 2);
        assert_eq!(line.spans[0].content, "Terminal ");
        assert_eq!(line.spans[1].content, "⚠ input");
        assert_eq!(line.spans[1].style.fg, Some(ORANGE));
        assert!(line.spans[1].style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn terminal_title_error() {
        let line = build_terminal_title_line(AttentionLevel::Error, true, false);
        assert_eq!(line.spans.len(), 2);
        assert_eq!(line.spans[0].content, "Terminal ");
        assert_eq!(line.spans[1].content, "✖ error");
        assert_eq!(line.spans[1].style.fg, Some(Color::Red));
        assert!(line.spans[1].style.add_modifier.contains(Modifier::BOLD));
    }
}
