use crate::tui::app::{App, Sender};
use kaji_core::sdd::StageStatus;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

pub fn draw(frame: &mut Frame, app: &App) {
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(frame.area());

    draw_header(frame, app, root[0]);

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(72), Constraint::Percentage(28)])
        .split(root[1]);

    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(3)])
        .split(cols[0]);

    draw_chat(frame, app, left[0]);
    draw_input(frame, app, left[1]);
    draw_spec(frame, app, cols[1]);

    if app.gate_open {
        draw_gate_modal(frame);
    }
}

fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let paragraph =
        Paragraph::new(app.header.as_str()).style(Style::default().add_modifier(Modifier::BOLD));
    frame.render_widget(paragraph, area);
}

fn draw_chat(frame: &mut Frame, app: &App, area: Rect) {
    let title = if app.turn_active {
        " chat — ⏳ tour en cours (Esc annule) ".to_string()
    } else if !app.status.is_empty() {
        format!(" chat — {} ", app.status)
    } else {
        " chat ".to_string()
    };
    let block = Block::default().borders(Borders::ALL).title(title);
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
    let block = Block::default().borders(Borders::ALL).title(" message ");
    let inner = block.inner(area);

    let paragraph = if app.input.is_empty() && !app.turn_active {
        Paragraph::new("écris ici…").style(Style::default().fg(Color::DarkGray))
    } else {
        Paragraph::new(app.input.as_str())
    };
    frame.render_widget(paragraph.block(block), area);

    let cursor_x = inner.x + (app.input.chars().count() as u16).min(inner.width.saturating_sub(1));
    frame.set_cursor_position((cursor_x, inner.y));
}

fn draw_spec(frame: &mut Frame, app: &App, area: Rect) {
    let title = app
        .spec
        .as_ref()
        .map(|spec| spec.title.clone())
        .unwrap_or_else(|| "aucune SPEC".to_string());

    let mut lines = vec![Line::from(Span::styled(
        title,
        Style::default().add_modifier(Modifier::BOLD),
    ))];
    if app.spec.is_none() {
        lines.push(Line::from(Span::styled(
            "SPEC.md dans le dossier courant ou kaji tui --spec <fichier>",
            Style::default().fg(Color::DarkGray),
        )));
    }
    lines.push(Line::from(""));

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

fn draw_gate_modal(frame: &mut Frame) {
    let area = centered_rect(60, 20, frame.area());
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Gate — approuver la SPEC ? (y/n) ");
    let paragraph = Paragraph::new("y = approuver   n / Esc = refuser")
        .block(block)
        .wrap(Wrap { trim: true });
    frame.render_widget(Clear, area);
    frame.render_widget(paragraph, area);
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}
