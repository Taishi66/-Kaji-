use crate::tui::app::{App, Sender};
use kaji_core::sdd::StageStatus;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

pub fn draw(frame: &mut Frame, app: &App) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(65), Constraint::Percentage(35)])
        .split(frame.area());

    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(3)])
        .split(cols[0]);

    draw_chat(frame, app, left[0]);
    draw_input(frame, app, left[1]);
    draw_spec(frame, app, cols[1]);
}

fn draw_chat(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default().borders(Borders::ALL).title(" chat ");
    let inner = block.inner(area);

    let mut lines: Vec<Line> = Vec::new();
    let mut wrapped_rows = 0usize;
    for chat_line in &app.chat {
        let (prefix, style) = match chat_line.sender {
            Sender::User => ("vous ▸ ", Style::default().fg(Color::Cyan)),
            Sender::Agent => ("kaji ▸ ", Style::default().fg(Color::Green)),
            Sender::System => ("· ", Style::default().fg(Color::DarkGray)),
        };
        for (i, raw_line) in chat_line.text.split('\n').enumerate() {
            let content = if i == 0 {
                format!("{prefix}{raw_line}")
            } else {
                raw_line.to_string()
            };
            wrapped_rows += wrapped_row_count(&content, inner.width);
            lines.push(Line::from(Span::styled(content, style)));
        }
    }

    let scroll = wrapped_rows.saturating_sub(inner.height as usize) as u16;
    let paragraph = Paragraph::new(Text::from(lines))
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    frame.render_widget(paragraph, area);
}

fn wrapped_row_count(line: &str, width: u16) -> usize {
    let width = width.max(1) as usize;
    line.chars().count().max(1).div_ceil(width)
}

fn draw_input(frame: &mut Frame, app: &App, area: Rect) {
    let title = if app.turn_active {
        " ⏳ tour en cours (Esc annule) "
    } else {
        " statut "
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    frame.render_widget(Paragraph::new(app.input.as_str()).block(block), area);

    let cursor_x = inner.x + (app.input.chars().count() as u16).min(inner.width.saturating_sub(1));
    frame.set_cursor_position((cursor_x, inner.y));
}

fn draw_spec(frame: &mut Frame, app: &App, area: Rect) {
    let title = app
        .spec
        .as_ref()
        .map(|spec| spec.title.clone())
        .unwrap_or_else(|| "aucune SPEC".to_string());

    let mut lines = vec![
        Line::from(Span::styled(
            title,
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];

    for (stage, status) in app.pass.stages() {
        let symbol = match status {
            StageStatus::Pending => "·",
            StageStatus::Running => "▶",
            StageStatus::Done => "✓",
            StageStatus::Failed => "✗",
        };
        lines.push(Line::from(format!("{symbol} {}", stage.label())));
    }

    let block = Block::default().borders(Borders::ALL).title(" SPEC ");
    frame.render_widget(Paragraph::new(Text::from(lines)).block(block), area);
}
