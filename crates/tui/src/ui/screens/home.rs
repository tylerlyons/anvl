use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};

use crate::app::TuiApp;
use crate::ui::widgets::tile_grid;

pub fn render(frame: &mut Frame, area: Rect, app: &TuiApp) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(5), Constraint::Length(2)])
        .split(area);

    tile_grid::render(
        frame,
        chunks[0],
        &app.workspaces,
        app.home_selected,
        app.flash_on,
    );

    let footer = "Home: arrows/hjkl move | Enter open | n add workspace | D delete workspace | ! toggle attention | q quit";
    frame.render_widget(
        Paragraph::new(footer)
            .block(Block::default().borders(Borders::TOP))
            .style(Style::default().fg(Color::Gray)),
        chunks[1],
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
