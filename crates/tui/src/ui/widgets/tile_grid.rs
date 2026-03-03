use protocol::{AttentionLevel, WorkspaceSummary};
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::Line,
    widgets::{Block, Borders, Paragraph},
    Frame,
};

pub const TILE_W: u16 = 38;
pub const TILE_H: u16 = 10;

pub fn render(
    frame: &mut Frame,
    area: Rect,
    items: &[WorkspaceSummary],
    selected: usize,
    flash_on: bool,
) {
    let orange = Color::Rgb(255, 165, 0);

    if items.is_empty() {
        frame.render_widget(
            Paragraph::new("No workspaces yet. Press `n` to add current directory.").block(
                Block::default()
                    .title("Workspaces")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::DarkGray)),
            ),
            area,
        );
        return;
    }

    let cols = (area.width / TILE_W).max(1) as usize;
    for (i, ws) in items.iter().enumerate() {
        let row = i / cols;
        let col = i % cols;
        let x = area.x + (col as u16 * TILE_W);
        let y = area.y + (row as u16 * TILE_H);
        let tile = Rect {
            x,
            y,
            width: TILE_W.min(area.width.saturating_sub(col as u16 * TILE_W)),
            height: TILE_H.min(area.height.saturating_sub(row as u16 * TILE_H)),
        };

        if tile.width < 8 || tile.height < 5 {
            continue;
        }

        let is_selected = i == selected;
        let needs_attention = matches!(
            ws.attention,
            AttentionLevel::NeedsInput | AttentionLevel::Error
        );

        let mut border_style = Style::default().fg(Color::DarkGray);
        if matches!(ws.attention, AttentionLevel::Error) {
            border_style = if flash_on {
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::LightRed)
            };
        } else if matches!(ws.attention, AttentionLevel::NeedsInput) {
            border_style = if flash_on {
                Style::default().fg(orange).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            };
        } else if ws.agent_running {
            border_style = Style::default().fg(Color::Green);
        }
        if is_selected {
            border_style = if needs_attention {
                if flash_on {
                    // Keep attention color while selected; pulse by toggling between orange and cyan.
                    border_style
                } else {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                }
            } else {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            };
        }

        // Pulse the body emphasis too for attention tiles.
        let body_style = if matches!(ws.attention, AttentionLevel::NeedsInput) && flash_on {
            Style::default().fg(orange).add_modifier(Modifier::BOLD)
        } else if matches!(ws.attention, AttentionLevel::Error) && flash_on {
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };

        let status = match ws.attention {
            AttentionLevel::None => "ok",
            AttentionLevel::Notice => "notice",
            AttentionLevel::NeedsInput => "needs-input",
            AttentionLevel::Error => "error",
        };
        let agent = if ws.agent_running { "on" } else { "off" };

        let body = vec![
            Line::from(format!("{}  [agent:{}]", ws.name, agent)),
            Line::from(format!("branch: {}", ws.branch.as_deref().unwrap_or("-"))),
            Line::from(format!("dirty: {}    status: {}", ws.dirty_files, status)),
            Line::from(truncate_middle(
                &ws.path,
                tile.width.saturating_sub(4) as usize,
            )),
        ];

        frame.render_widget(
            Paragraph::new(body).style(body_style).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(border_style),
            ),
            tile,
        );
    }
}

pub fn index_at(area: Rect, x: u16, y: u16, item_count: usize) -> Option<usize> {
    if item_count == 0 {
        return None;
    }
    if x < area.x || y < area.y || x >= area.right() || y >= area.bottom() {
        return None;
    }
    let rel_x = x - area.x;
    let rel_y = y - area.y;
    let cols = (area.width / TILE_W).max(1) as usize;
    let col = (rel_x / TILE_W) as usize;
    let row = (rel_y / TILE_H) as usize;
    let idx = row * cols + col;
    if idx < item_count {
        Some(idx)
    } else {
        None
    }
}

fn truncate_middle(input: &str, max: usize) -> String {
    if input.chars().count() <= max {
        return input.to_string();
    }
    if max <= 5 {
        return input.chars().take(max).collect();
    }
    let keep = (max - 3) / 2;
    let head = input.chars().take(keep).collect::<String>();
    let tail = input
        .chars()
        .rev()
        .take(keep)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    format!("{head}...{tail}")
}
