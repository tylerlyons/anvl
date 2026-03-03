use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};

use crate::app::TuiApp;
use crate::ui::widgets::tile_grid;

pub fn render(frame: &mut Frame, area: Rect, app: &TuiApp) {
    let chunks = home_chunks(area);

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                "multiws ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("command center"),
            Span::raw("    / filter   n new   D delete   Enter open"),
        ]))
        .block(Block::default().borders(Borders::BOTTOM)),
        chunks[0],
    );

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
    let stats = format!(
        "Needs Input: {}   Errors: {}   Dirty Files: {}   Running Agents: {}",
        needs_input, errors, dirty, running_agents
    );
    frame.render_widget(
        Paragraph::new(stats)
            .style(Style::default().fg(Color::Yellow))
            .block(Block::default().title("Status").borders(Borders::BOTTOM)),
        chunks[1],
    );

    tile_grid::render(
        frame,
        chunks[2],
        &app.workspaces,
        app.home_selected,
        app.flash_on,
    );

    let footer = "Home: arrows/hjkl move | Enter open | n add workspace | D delete workspace | ! toggle attention | q quit";
    frame.render_widget(
        Paragraph::new(footer)
            .block(Block::default().borders(Borders::TOP))
            .style(Style::default().fg(Color::Gray)),
        chunks[3],
    );

    if let Some(path_input) = &app.add_workspace_path_input {
        let modal = centered_rect(area, 70, 7);
        frame.render_widget(Clear, modal);
        frame.render_widget(
            Paragraph::new(format!(
                "New Workspace Path\n\n{}\n\nEnter: create   Esc: cancel",
                path_input
            ))
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
            Paragraph::new(format!(
                "Delete workspace?\n\n{}\n\nY: delete   N: cancel",
                name
            ))
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

pub fn add_modal_rect(area: Rect) -> Rect {
    centered_rect(area, 70, 7)
}

pub fn delete_modal_rect(area: Rect) -> Rect {
    centered_rect(area, 56, 7)
}

pub fn grid_rect(area: Rect) -> Rect {
    home_chunks(area)[2]
}

fn home_chunks(area: Rect) -> Vec<Rect> {
    Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(2),
        ])
        .split(area)
        .to_vec()
}
