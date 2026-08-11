use crate::tui::app::{App, Sender, ToolApprovalRequest};
use crate::tui::{markdown, theme};
use kaji_core::sdd::StageStatus;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

pub fn draw(frame: &mut Frame, app: &App) {
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(frame.area());

    draw_header(frame, app, root[0]);

    if app.spec_panel_visible() {
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
    } else {
        let left = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(3)])
            .split(root[1]);

        draw_chat(frame, app, left[0]);
        draw_input(frame, app, left[1]);
    }

    if app.gate_open {
        draw_gate_modal(frame);
    } else if let Some(approval) = &app.tool_approval {
        draw_tool_approval_modal(frame, approval);
    }
}

fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(0), Constraint::Length(34)])
        .split(area);

    let mut left_spans = vec![
        Span::styled(theme::KAJI_GLYPH, theme::title()),
        Span::styled(format!(" kaji · {}", app.header), theme::dim()),
    ];
    if let Some(git) = &app.git_status {
        left_spans.push(Span::styled(format!(" · {git}"), theme::dim()));
    }
    frame.render_widget(Paragraph::new(Line::from(left_spans)), cols[0]);

    let right = Paragraph::new(header_status_text(app))
        .style(theme::dim())
        .alignment(Alignment::Right);
    frame.render_widget(right, cols[1]);
}

fn header_status_text(app: &App) -> String {
    let base = if app.turn_active {
        let elapsed = app.turn_started.map(|t| t.elapsed().as_secs()).unwrap_or(0);
        format!(
            "↑{} ↓{} · {elapsed}s",
            app.tokens_turn_in, app.tokens_turn_out
        )
    } else {
        format!("↑{} ↓{}", app.tokens_total_in, app.tokens_total_out)
    };
    match app.cost_total {
        Some(cost) => format!("{base} · ${cost:.2}"),
        None => base,
    }
}

fn chat_title(app: &App) -> String {
    let mut parts = vec!["chat".to_string()];
    if !app.status.is_empty() {
        parts.push(app.status.clone());
    }
    if app.scroll_offset > 0 {
        parts.push(format!(
            "{} défilement — End pour revenir",
            theme::SCROLL_INDICATOR
        ));
    }
    format!(" {} ", parts.join(" · "))
}

/// Chat reading width is capped so text stays legible on ultra-wide
/// terminals instead of running edge-to-edge across ~200 columns.
const CHAT_MAX_WIDTH: u16 = 102;

fn chat_width(inner_width: u16) -> u16 {
    inner_width.min(CHAT_MAX_WIDTH)
}

fn draw_chat(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme::border_inactive())
        .title(chat_title(app));
    let inner = block.inner(area);
    let chat_rect = Rect {
        width: chat_width(inner.width),
        ..inner
    };

    let mut lines: Vec<Line> = Vec::new();
    for chat_line in &app.chat {
        match chat_line.sender {
            Sender::Agent => push_agent_lines(&mut lines, &chat_line.text),
            Sender::User => push_plain_lines(
                &mut lines,
                &chat_line.text,
                theme::USER_PREFIX,
                theme::user(),
            ),
            Sender::System => push_system_line(&mut lines, chat_line),
        }
        lines.push(Line::from(""));
    }

    let wrapped_rows: usize = lines
        .iter()
        .map(|line| line_wrapped_rows(line, chat_rect.width))
        .sum();
    let base_scroll = wrapped_rows.saturating_sub(chat_rect.height as usize);
    app.chat_overflow.set(base_scroll as u16);
    let scroll = base_scroll.saturating_sub(app.scroll_offset as usize) as u16;

    frame.render_widget(block, area);
    let paragraph = Paragraph::new(Text::from(lines))
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    frame.render_widget(paragraph, chat_rect);
}

fn push_agent_lines(lines: &mut Vec<Line<'static>>, text: &str) {
    let mut md_lines = markdown::render_markdown(text);
    if md_lines.is_empty() {
        md_lines.push(Line::from(""));
    }
    if let Some(first) = md_lines.first_mut() {
        let mut spans = vec![Span::styled(theme::AGENT_PREFIX, theme::agent())];
        spans.append(&mut first.spans);
        *first = Line::from(spans);
    }
    lines.extend(md_lines);
}

fn push_plain_lines(lines: &mut Vec<Line<'static>>, text: &str, prefix: &str, style: Style) {
    for (i, raw_line) in text.split('\n').enumerate() {
        let content = if i == 0 {
            format!("{prefix}{raw_line}")
        } else {
            raw_line.to_string()
        };
        lines.push(Line::from(Span::styled(content, style)));
    }
}

fn push_system_line(lines: &mut Vec<Line<'static>>, chat_line: &crate::tui::app::ChatLine) {
    if let Some(tool) = &chat_line.tool {
        let spinner = theme::spinner_frame(tool.started.elapsed());
        let content = format!("{}⚙ {} {spinner}", theme::SYSTEM_PREFIX, tool.name);
        lines.push(Line::from(Span::styled(content, theme::system())));
        return;
    }
    if let Some(rendered) = &chat_line.rendered {
        push_rendered_lines(lines, rendered);
        return;
    }
    push_plain_lines(
        lines,
        &chat_line.text,
        theme::SYSTEM_PREFIX,
        theme::system(),
    );
}

/// On-demand report blocks (`/cost`, `/docker`) — already fully styled by
/// `crate::tui::report`; only the leading `SYSTEM_PREFIX` marker is spliced
/// onto the first line so it still reads as a system message.
fn push_rendered_lines(lines: &mut Vec<Line<'static>>, rendered: &[Line<'static>]) {
    let mut iter = rendered.iter();
    if let Some(first) = iter.next() {
        let mut spans = vec![Span::styled(theme::SYSTEM_PREFIX, theme::dim())];
        spans.extend(first.spans.iter().cloned());
        lines.push(Line::from(spans));
    }
    lines.extend(iter.cloned());
}

fn line_wrapped_rows(line: &Line, width: u16) -> usize {
    let width = width.max(1) as usize;
    let chars: usize = line.spans.iter().map(|s| s.content.chars().count()).sum();
    chars.max(1).div_ceil(width)
}

fn draw_input(frame: &mut Frame, app: &App, area: Rect) {
    let (title, title_style) = if app.turn_active {
        let elapsed = app.turn_started.map(|t| t.elapsed().as_secs()).unwrap_or(0);
        (format!(" ⏳ {elapsed}s — Esc annule "), theme::accent())
    } else {
        (" message ".to_string(), theme::title())
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme::border_inactive())
        .title(Span::styled(title, title_style));
    let inner = block.inner(area);

    let paragraph = if app.input.is_empty() && !app.turn_active {
        Paragraph::new("écris ici…").style(theme::dim())
    } else {
        Paragraph::new(app.input.as_str()).style(theme::text())
    };
    let scroll_x = app.input_scroll_x(inner.width.max(1));
    frame.render_widget(paragraph.block(block).scroll((0, scroll_x)), area);

    let cursor_x =
        inner.x + (app.input_cursor_chars() - scroll_x).min(inner.width.saturating_sub(1));
    frame.set_cursor_position((cursor_x, inner.y));
}

fn draw_spec(frame: &mut Frame, app: &App, area: Rect) {
    let title = app
        .spec
        .as_ref()
        .map(|spec| spec.title.clone())
        .unwrap_or_else(|| "aucune SPEC".to_string());

    let mut lines = vec![Line::from(Span::styled(title, theme::title()))];
    if app.spec.is_none() {
        lines.push(Line::from(Span::styled(
            "SPEC.md dans le dossier courant ou kaji tui --spec <fichier>",
            theme::dim(),
        )));
    }
    lines.push(Line::from(""));

    for (stage, status) in app.pass.stages() {
        let (symbol, style) = match status {
            StageStatus::Pending => ("·", theme::dim()),
            StageStatus::Running => ("▶", theme::accent()),
            StageStatus::Done => ("✓", theme::title()),
            StageStatus::Failed => ("✗", theme::accent()),
        };
        lines.push(Line::from(Span::styled(
            format!("{symbol} {}", stage.label()),
            style,
        )));
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme::border_inactive())
        .title(" SPEC ");
    frame.render_widget(Paragraph::new(Text::from(lines)).block(block), area);
}

fn draw_gate_modal(frame: &mut Frame) {
    let area = centered_rect(60, 20, frame.area());
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme::title())
        .title(format!(
            " {} Gate — approuver la SPEC ? (y/n) ",
            theme::GATE_SYMBOL
        ));
    let paragraph = Paragraph::new("y = approuver   n / Esc = refuser")
        .block(block)
        .wrap(Wrap { trim: true });
    frame.render_widget(Clear, area);
    frame.render_widget(paragraph, area);
}

fn draw_tool_approval_modal(frame: &mut Frame, approval: &ToolApprovalRequest) {
    let area = centered_rect(60, 20, frame.area());
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme::border_active())
        .title(format!(
            " {} confirmation d'outil — {} (y/n) ",
            theme::GATE_SYMBOL,
            approval.tool_name
        ));
    let body = approval
        .prompt
        .clone()
        .unwrap_or_else(|| "y = autoriser une fois   n / Esc = refuser".to_string());
    let paragraph = Paragraph::new(body).block(block).wrap(Wrap { trim: true });
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_measure_is_capped_at_102_columns() {
        assert_eq!(chat_width(200), 102);
        assert_eq!(chat_width(102), 102);
        assert_eq!(chat_width(80), 80);
    }
}
