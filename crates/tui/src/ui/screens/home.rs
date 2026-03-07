use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};

use crate::app::TuiApp;
use crate::ui::footer;
use crate::ui::widgets::tile_grid;
use tile_grid::ORANGE;

/// Renders the home screen: dashboard header, tile grid, footer, and any open modals.
pub fn render(frame: &mut Frame, area: Rect, app: &TuiApp) {
    let chunks = home_chunks(area);
    render_dashboard(frame, chunks[0], app);
    tile_grid::render(frame, chunks[1], &app.workspaces, app.home_selected, app.flash_on);
    footer::render(frame, chunks[2], app);
    render_modals(frame, area, app);
}

/// Renders the rounded dashboard box with colored status badges.
fn render_dashboard(frame: &mut Frame, area: Rect, app: &TuiApp) {
    let needs_input = app
        .workspaces
        .iter()
        .filter(|w| matches!(w.attention, protocol::AttentionLevel::NeedsInput))
        .count();
    let errors = app
        .workspaces
        .iter()
        .filter(|w| matches!(w.attention, protocol::AttentionLevel::Error))
        .count();
    let dirty = app.workspaces.iter().map(|w| w.dirty_files).sum::<usize>();
    let running_agents = app.workspaces.iter().filter(|w| w.agent_running).count();

    let mut spans = Vec::new();
    spans.extend(dashboard_badge(needs_input, "\u{26A0}", "input", ORANGE));
    spans.extend(dashboard_badge(errors, "\u{2716}", "error", Color::Red));
    spans.extend(dashboard_badge(dirty, "\u{25C8}", "changes", Color::Yellow));
    spans.extend(dashboard_badge(running_agents, "\u{25CF}", "agents", Color::Green));

    let dashboard = Paragraph::new(Line::from(spans)).block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title_top(Line::from(Span::styled(
                " \u{25C8} flow ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ))),
    );
    frame.render_widget(dashboard, area);
}

/// Builds a styled icon+count badge span pair for the dashboard header.
/// Returns dimmed spans when `count` is zero so the layout stays stable.
fn dashboard_badge(count: usize, icon: &str, label: &str, color: Color) -> Vec<Span<'static>> {
    let dim = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::DIM);
    if count > 0 {
        vec![
            Span::styled(
                format!("{} {} ", icon, count),
                Style::default()
                    .fg(color)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{}     ", label),
                Style::default().fg(Color::DarkGray),
            ),
        ]
    } else {
        vec![
            Span::styled(format!("{} {} ", icon, count), dim),
            Span::styled(format!("{}     ", label), dim),
        ]
    }
}

/// Renders the add-workspace and delete-confirmation modals when active.
fn render_modals(frame: &mut Frame, area: Rect, app: &TuiApp) {
    if let Some(path_input) = &app.add_workspace_path_input {
        let modal = centered_rect(area, 70, 7);
        frame.render_widget(Clear, modal);
        frame.render_widget(
            Paragraph::new(format!("New Workspace Path\n\n{}", path_input))
                .alignment(Alignment::Left)
                .block(
                    Block::default()
                        .title("Add Workspace")
                        .borders(Borders::ALL),
                ),
            modal,
        );
    }

    if let Some(id) = app.pending_delete_workspace {
        let name = app
            .workspaces
            .iter()
            .find(|w| w.id == id)
            .map(|w| w.name.clone())
            .unwrap_or_else(|| id.to_string());
        let modal = centered_rect(area, 56, 7);
        frame.render_widget(Clear, modal);
        frame.render_widget(
            Paragraph::new(format!("Delete workspace?\n\n{}", name))
                .alignment(Alignment::Left)
                .block(
                    Block::default()
                        .title("Confirm Delete")
                        .borders(Borders::ALL),
                ),
            modal,
        );
    }
}

/// Returns a centered rectangle within `area` at `width_pct` width and fixed `height`.
fn centered_rect(area: Rect, width_pct: u16, height: u16) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(height),
            Constraint::Min(1),
        ])
        .split(area);
    let horizontal = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - width_pct) / 2),
            Constraint::Percentage(width_pct),
            Constraint::Percentage((100 - width_pct) / 2),
        ])
        .split(vertical[1]);
    horizontal[1]
}

/// Returns the rectangle used by the add-workspace modal.
pub fn add_modal_rect(area: Rect) -> Rect {
    centered_rect(area, 70, 7)
}

/// Returns the rectangle used by the delete-confirmation modal.
pub fn delete_modal_rect(area: Rect) -> Rect {
    centered_rect(area, 56, 7)
}

/// Returns the rectangle occupied by the tile grid on the home screen.
pub fn grid_rect(area: Rect) -> Rect {
    home_chunks(area)[1]
}

/// Splits the home screen area into dashboard header, grid, and footer chunks.
fn home_chunks(area: Rect) -> Vec<Rect> {
    Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(2),
        ])
        .split(area)
        .to_vec()
}
