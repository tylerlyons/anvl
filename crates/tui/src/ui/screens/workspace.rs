use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
    Frame,
};

use crate::app::TuiApp;
use protocol::Route;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceHit {
    Header,
    TerminalTab(usize),
    TerminalPane,
    FilesList(usize),
    DiffPane,
}

#[derive(Debug, Clone, Copy)]
struct WorkspaceLayout {
    header: Rect,
    terminal_tabs: Rect,
    terminal_pane: Rect,
    git_files: Rect,
    git_diff: Rect,
    footer: Rect,
}

fn layout(area: Rect, focus: crate::app::Focus) -> WorkspaceLayout {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(5), Constraint::Length(2)])
        .split(area);
    let body = Layout::default()
        .direction(Direction::Vertical)
        .constraints(match focus {
            crate::app::Focus::WsTerminal | crate::app::Focus::WsTerminalTabs => {
                [Constraint::Percentage(72), Constraint::Percentage(28)]
            }
            crate::app::Focus::WsFiles | crate::app::Focus::WsDiff => {
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

    WorkspaceLayout {
        header: chunks[0],
        terminal_tabs: terminal_area[0],
        terminal_pane: terminal_area[1],
        git_files: git_area[0],
        git_diff: git_area[1],
        footer: chunks[2],
    }
}

pub fn render(frame: &mut Frame, area: Rect, app: &TuiApp) {
    let l = layout(area, app.focus);

    let focused_border = |focused: bool| {
        if focused {
            Style::default()
                .fg(Color::LightCyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        }
    };

    let ws_id = match app.route {
        Route::Workspace { id } => Some(id),
        _ => None,
    };
    let title = ws_id
        .and_then(|id| app.workspaces.iter().find(|w| w.id == id))
        .map(|w| format!("Workspace: {} ({})", w.name, w.path))
        .unwrap_or_else(|| "Workspace".to_string());

    frame.render_widget(
        Paragraph::new(if let Some(name) = &app.rename_workspace_input {
            format!("{title}\nRename: {name}")
        } else {
            title
        })
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(focused_border(app.focus == crate::app::Focus::WsHeader)),
        ),
        l.header,
    );

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
        .map(|f| ListItem::new(format!("{:>2} {}", f.status, f.path)))
        .collect::<Vec<_>>();
    let file_list = List::new(file_items)
        .block(
            Block::default()
                .title("Changed Files")
                .borders(Borders::ALL)
                .border_style(focused_border(app.focus == crate::app::Focus::WsFiles)),
        )
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
    frame.render_stateful_widget(file_list, l.git_files, &mut list_state);

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
    frame.render_widget(
        Paragraph::new(diff_lines)
            .block(
                Block::default()
                    .title("Diff")
                    .borders(Borders::ALL)
                    .border_style(focused_border(app.focus == crate::app::Focus::WsDiff)),
            )
            .scroll((app.ws_diff_scroll, 0))
            .wrap(Wrap { trim: false }),
        l.git_diff,
    );

    let ws_summary = ws_id.and_then(|id| app.workspaces.iter().find(|w| w.id == id));
    let (agent_running, shell_running) = ws_summary
        .map(|w| (w.agent_running, w.shell_running))
        .unwrap_or((false, false));
    let tab_rects = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(
            app.ws_tabs
                .iter()
                .map(|_| Constraint::Ratio(1, app.ws_tabs.len().max(1) as u32))
                .collect::<Vec<_>>(),
        )
        .split(l.terminal_tabs);
    let tabs_focused = app.focus == crate::app::Focus::WsTerminalTabs;
    let selected_style = Style::default()
        .fg(Color::LightGreen)
        .add_modifier(Modifier::BOLD);
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
        frame.render_widget(
            Paragraph::new(format!(
                "{}\n{}\n[h/l] [n new] [x close] [r rename]",
                label,
                if running { "running" } else { "stopped" }
            ))
            .block(
                Block::default()
                    .title(format!("{}", i + 1))
                    .borders(Borders::ALL)
                    .border_style(if i == app.ws_active_tab {
                        selected_style
                    } else {
                        focused_border(tabs_focused)
                    }),
            ),
            tab_rects[i],
        );
    }

    let terminal_lines = ws_id
        .map(|id| app.terminal_lines(id, &app.active_tab_id()))
        .unwrap_or_else(|| vec![Line::from("No terminal output yet.")]);
    frame.render_widget(
        Paragraph::new(terminal_lines).block(
            Block::default()
                .title("Terminal")
                .borders(Borders::ALL)
                .border_style(focused_border(app.focus == crate::app::Focus::WsTerminal)),
        ),
        l.terminal_pane,
    );

    let footer = "Tab/S-Tab focus | h/l or <-/-> tab | n new tab | x close | r rename tab | e rename workspace | a start | A stop | Esc Home, then n = new workspace";
    frame.render_widget(
        Paragraph::new(footer)
            .block(Block::default().borders(Borders::TOP))
            .style(Style::default().fg(Color::Gray)),
        l.footer,
    );
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
