use crate::tui::app::{App, ChatLine, Sender, ToolApprovalRequest};
use crate::tui::{markdown, theme};
use kaji_core::sdd::StageStatus;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
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
        draw_palette(frame, app, left[1]);
        draw_spec(frame, app, cols[1]);
    } else {
        let left = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(3)])
            .split(root[1]);

        draw_chat(frame, app, left[0]);
        draw_input(frame, app, left[1]);
        draw_palette(frame, app, left[1]);
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

/// Horizontal breathing room so chat text doesn't collide with the block
/// borders — applied on both sides, inside the width cap above.
const CHAT_HORIZONTAL_MARGIN: u16 = 1;

fn chat_width(inner_width: u16) -> u16 {
    inner_width.min(CHAT_MAX_WIDTH)
}

/// Applies the width cap, then the horizontal margin inside it — every
/// downstream measurement (wrapped_rows, chat_overflow) must read
/// `chat_rect.width` rather than recompute it, so they stay in sync.
fn chat_content_rect(inner: Rect) -> Rect {
    Rect {
        x: inner.x + CHAT_HORIZONTAL_MARGIN,
        width: chat_width(inner.width).saturating_sub(CHAT_HORIZONTAL_MARGIN * 2),
        ..inner
    }
}

fn scroll_offset_u16(value: usize) -> u16 {
    value.min(u16::MAX as usize) as u16
}

fn draw_chat(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme::border_inactive())
        .title(chat_title(app));
    let inner = block.inner(area);
    let chat_rect = chat_content_rect(inner);

    app.user_turn_rows.borrow_mut().clear();
    let mut running_rows: u16 = 0;
    let mut lines: Vec<Line> = Vec::new();
    let streaming_idx = app.streaming_agent_line();
    let elapsed = app.turn_started.map(|t| t.elapsed()).unwrap_or_default();
    for (i, chat_line) in app.chat.iter().enumerate() {
        if chat_line.sender == Sender::User {
            app.user_turn_rows.borrow_mut().push(running_rows);
        }
        let blade = (streaming_idx == Some(i)).then(|| theme::blade_frame(elapsed));
        let start = lines.len();
        push_chat_line(&mut lines, chat_line, chat_rect.width, blade);
        lines.push(Line::from(""));
        for line in &lines[start..] {
            running_rows =
                running_rows.saturating_add(line_wrapped_rows(line, chat_rect.width) as u16);
        }
    }
    if let Some(loader) = loader_line(app) {
        lines.push(loader);
    }

    let wrapped_rows: usize = lines
        .iter()
        .map(|line| line_wrapped_rows(line, chat_rect.width))
        .sum();
    let base_scroll = wrapped_rows.saturating_sub(chat_rect.height as usize);
    app.chat_overflow.set(scroll_offset_u16(base_scroll));
    let scroll = base_scroll.saturating_sub(app.scroll_offset as usize) as u16;

    frame.render_widget(block, area);
    let paragraph = Paragraph::new(Text::from(lines))
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    frame.render_widget(paragraph, chat_rect);
}

/// `blade` is the ninja-cursor glyph (T4) to append to this chat line's last
/// rendered `Line`, when `App::streaming_agent_line` says this is the one —
/// only the `Agent` arm ever consumes it, so it's silently ignored for every
/// other sender (a `Thinking` line, for instance, must never carry it).
fn push_chat_line(
    lines: &mut Vec<Line<'static>>,
    chat_line: &ChatLine,
    width: u16,
    blade: Option<char>,
) {
    match chat_line.sender {
        Sender::Agent => push_agent_lines(lines, &chat_line.text, width, blade),
        Sender::User => push_plain_lines(lines, &chat_line.text, theme::USER_PREFIX, theme::user()),
        Sender::System => push_system_line(lines, chat_line),
        Sender::Thinking => push_plain_lines(
            lines,
            &chat_line.text,
            theme::THINKING_PREFIX,
            theme::thinking(),
        ),
    }
}

/// Loader zen — the chat's trailing `{ensō} 思考中 · {N}s` line while a turn
/// is in flight with nothing readable yet (`App::show_loader`). `None` once
/// the first visible chunk lands or the turn ends; no special redraw is
/// needed for the animation since the 250ms tick already re-renders
/// whenever a turn is active or pending.
fn loader_line(app: &App) -> Option<Line<'static>> {
    if !app.show_loader() {
        return None;
    }
    let elapsed = app.turn_started.map(|t| t.elapsed()).unwrap_or_default();
    let content = format!(
        "{} 思考中 · {}s",
        theme::enso_frame(elapsed),
        elapsed.as_secs()
    );
    Some(Line::from(Span::styled(content, theme::dim())))
}

fn push_agent_lines(lines: &mut Vec<Line<'static>>, text: &str, width: u16, blade: Option<char>) {
    let mut md_lines = markdown::render_markdown(text, width);
    if md_lines.is_empty() {
        md_lines.push(Line::from(""));
    }
    if let Some(first) = md_lines.first_mut() {
        let mut spans = vec![Span::styled(theme::AGENT_PREFIX, theme::agent())];
        spans.append(&mut first.spans);
        *first = Line::from(spans);
    }
    // Appended before `lines.extend` below, i.e. before `draw_chat` measures
    // wrapped rows for this block — the blade is part of what gets measured,
    // not tacked on after the fact.
    if let Some(glyph) = blade {
        if let Some(last) = md_lines.last_mut() {
            last.spans
                .push(Span::styled(glyph.to_string(), theme::accent()));
        }
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

fn push_system_line(lines: &mut Vec<Line<'static>>, chat_line: &ChatLine) {
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

/// Row count for one already-built `Line`, measured with the SAME
/// `WordWrapper` ratatui's renderer uses (`Paragraph::line_count`, gated
/// behind the `unstable-rendered-line-info` feature) instead of a
/// hand-rolled char-count/width division — that div_ceil undercounted CJK
/// text (unicode-width cells, not `chars().count()`) and ignored word-break
/// boundaries entirely. `WordWrapper` wraps each source `Line` of a
/// `Paragraph` independently (it never merges adjacent lines), so measuring
/// one `Line` at a time here and summing gives the same total as measuring
/// the whole chat `Text` at once the way `draw_chat` renders it.
fn line_wrapped_rows(line: &Line, width: u16) -> usize {
    let width = width.max(1);
    Paragraph::new(Text::from(vec![line.clone()]))
        .wrap(Wrap { trim: false })
        .line_count(width)
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

/// Command palette (T5) — overlay anchored just above the input box, drawn
/// after `draw_input` like the y/n modals: it reads `input_area` only to
/// position itself and never touches the chat's own `chat_overflow`/
/// `user_turn_rows` measurement.
fn draw_palette(frame: &mut Frame, app: &App, input_area: Rect) {
    if !app.palette_visible() {
        return;
    }
    let matches = app.palette_matches();
    let name_w = matches.iter().map(|c| c.name.len()).max().unwrap_or(0);
    let inner_w = matches
        .iter()
        .map(|c| name_w + 2 + c.desc.chars().count() + 4)
        .max()
        .unwrap_or(0) as u16;
    // Ceiling last: the 20-column floor must never win over the space
    // actually available, or the box is drawn wider than `input_area` and
    // spills into neighboring UI (or past the terminal on narrower widths).
    let width = (inner_w + 2)
        .max(20)
        .min(input_area.width.saturating_sub(2));
    let height = (matches.len() as u16 + 2).min(input_area.y);
    if width < 4 || height < 3 {
        return;
    }
    let rows = (height - 2) as usize;
    // Sliding window: keep the selection visible when the filtered list is
    // taller than the space available above the input.
    let first = app.palette_selected.saturating_sub(rows.saturating_sub(1));
    let area = Rect {
        x: input_area.x + 1,
        y: input_area.y - height,
        width,
        height,
    };
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(" commandes ")
        .title_bottom(Line::from(" ↑↓ choisir · ⏎ valider · esc ").style(theme::dim()));
    let lines: Vec<Line> = matches
        .iter()
        .enumerate()
        .skip(first)
        .take(rows)
        .map(|(i, cmd)| {
            let selected = i == app.palette_selected;
            let marker = if selected { "▸ " } else { "  " };
            let name_style = if selected {
                theme::accent()
            } else {
                Style::default()
            };
            Line::from(vec![
                Span::styled(marker, theme::accent()),
                Span::styled(format!("{:<name_w$}", cmd.name), name_style),
                Span::styled(format!("  {}", cmd.desc), theme::dim()),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(Text::from(lines)).block(block), area);
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
    use ratatui::crossterm::event::{
        Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers,
    };

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        })
    }

    fn buffer_as_string(buffer: &ratatui::buffer::Buffer) -> String {
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn palette_renders_filtered_commands_above_the_input() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut app = App::new(None);
        for c in "/s".chars() {
            app.on_event(&key(KeyCode::Char(c)));
        }

        let backend = TestBackend::new(80, 14);
        let mut terminal = Terminal::new(backend).expect("test backend terminal");
        terminal.draw(|f| draw(f, &app)).expect("draw");
        let content = buffer_as_string(terminal.backend().buffer());

        assert!(content.contains("commandes"));
        assert!(content.contains("/sdd"));
        assert!(content.contains("/spec"));
        assert!(!content.contains("/quit"), "filtré hors de la liste");
        assert!(content.contains("▸"), "marqueur de sélection visible");
    }

    #[test]
    fn palette_is_absent_without_slash_input_and_without_matches() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        for input in ["", "hello", "/xyz"] {
            let mut app = App::new(None);
            for c in input.chars() {
                app.on_event(&key(KeyCode::Char(c)));
            }
            let backend = TestBackend::new(80, 14);
            let mut terminal = Terminal::new(backend).expect("test backend terminal");
            terminal.draw(|f| draw(f, &app)).expect("draw");
            let content = buffer_as_string(terminal.backend().buffer());

            assert!(
                !content.contains("commandes"),
                "input {input:?} ne doit pas ouvrir la palette"
            );
        }
    }

    /// Regression for the wrap-measure divergence: `line_wrapped_rows` must
    /// count what ratatui actually renders, not `chars().count()` — CJK
    /// graphemes are 2 terminal cells wide, so 10 of them (20 cells) at a
    /// 12-cell width wrap onto 2 rows, not the 1 row a char-count/width
    /// division would report. Ground truth comes from an independent
    /// TestBackend render (not from `line_wrapped_rows` itself) so the test
    /// can't pass by construction.
    #[test]
    fn line_wrapped_rows_matches_ratatui_actual_wrap_for_cjk_text() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let text = "鍛冶".repeat(5); // 10 CJK chars, 20 cells wide
        let line = Line::from(text.clone());
        let width = 12u16;

        let backend = TestBackend::new(width, 8);
        let mut terminal = Terminal::new(backend).expect("test backend terminal");
        terminal
            .draw(|frame| {
                let paragraph =
                    Paragraph::new(Text::from(vec![line.clone()])).wrap(Wrap { trim: false });
                frame.render_widget(paragraph, frame.area());
            })
            .expect("draw must succeed against a TestBackend");
        let buffer = terminal.backend().buffer();
        let rendered_rows = (0..buffer.area.height)
            .filter(|&y| (0..buffer.area.width).any(|x| buffer[(x, y)].symbol() != " "))
            .count();

        assert_eq!(
            rendered_rows, 2,
            "sanity check on the oracle itself: 20 cells at width 12 must take 2 rows"
        );
        assert_eq!(
            line_wrapped_rows(&line, width),
            rendered_rows,
            "line_wrapped_rows must match the row count ratatui actually renders"
        );
    }

    #[test]
    fn chat_measure_is_capped_at_102_columns() {
        assert_eq!(chat_width(200), 102);
        assert_eq!(chat_width(102), 102);
        assert_eq!(chat_width(80), 80);
    }

    /// `running_rows` already saturates (`saturating_add`) rather than
    /// wrapping past `u16::MAX` — `base_scroll` (a `usize`) must clamp the
    /// same way when narrowed to `u16` for `chat_overflow`, instead of `as
    /// u16` truncating (e.g. 70000 → 4464) and silently under-reporting how
    /// far the chat can scroll.
    #[test]
    fn scroll_offset_u16_saturates_instead_of_truncating() {
        assert_eq!(scroll_offset_u16(70_000), u16::MAX);
        assert_eq!(scroll_offset_u16(30), 30);
    }

    #[test]
    fn chat_content_rect_applies_horizontal_margin_inside_the_width_cap() {
        let wide = Rect {
            x: 1,
            y: 0,
            width: 200,
            height: 10,
        };
        let rect = chat_content_rect(wide);
        assert_eq!(rect.x, 2);
        assert_eq!(rect.width, 100);

        let narrow = Rect {
            x: 1,
            y: 0,
            width: 40,
            height: 10,
        };
        let rect = chat_content_rect(narrow);
        assert_eq!(rect.x, 2);
        assert_eq!(rect.width, 38);
    }

    use kaji::agents::AgentEvent;
    use kaji::conversation::message::Message;

    fn thinking_message(id: &str, text: &str) -> AgentEvent {
        let mut m = Message::assistant().with_thinking(text, "");
        m.id = Some(id.to_string());
        AgentEvent::Message(m)
    }

    fn text_message(id: &str, text: &str) -> AgentEvent {
        let mut m = Message::assistant().with_text(text);
        m.id = Some(id.to_string());
        AgentEvent::Message(m)
    }

    /// Scans the whole rendered buffer for any ninja-cursor glyph
    /// (`theme::BLADE_FRAMES`) — used by the absence tests below, which only
    /// care that the blade never appears anywhere for their scenario.
    fn any_blade_glyph(buffer: &ratatui::buffer::Buffer) -> bool {
        (0..buffer.area.height).any(|y| {
            (0..buffer.area.width).any(|x| {
                theme::BLADE_FRAMES.contains(&buffer[(x, y)].symbol().chars().next().unwrap_or(' '))
            })
        })
    }

    #[test]
    fn thinking_chat_line_renders_with_prefix_and_dim_italic_style() {
        let mut lines = Vec::new();
        let chat_line = ChatLine {
            sender: Sender::Thinking,
            text: "raisonnement".to_string(),
            tool: None,
            rendered: None,
        };
        push_chat_line(&mut lines, &chat_line, 80, None);

        assert_eq!(lines.len(), 1);
        let content: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(content.starts_with(theme::THINKING_PREFIX));
        assert!(content.contains("raisonnement"));
        assert_eq!(lines[0].spans[0].style, theme::thinking());
    }

    #[test]
    fn loader_line_absent_while_idle() {
        let app = App::new(None);
        assert!(loader_line(&app).is_none());
    }

    #[test]
    fn loader_line_present_while_turn_pending_and_nothing_visible() {
        let mut app = App::new(None);
        app.turn_pending = true;

        let line = loader_line(&app).expect("loader must show while nothing is visible yet");
        let content: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(content.contains("思考中"));
    }

    #[test]
    fn loader_line_absent_once_turn_output_is_visible() {
        let mut app = App::new(None);
        app.turn_active = true;
        app.turn_has_visible_output = true;

        assert!(loader_line(&app).is_none());
    }

    #[test]
    fn loader_line_absent_when_thinking_already_displayed_and_enabled() {
        let mut app = App::new(None);
        app.turn_active = true;
        app.show_thinking = true;
        app.apply_agent_event(&thinking_message("m1", "raisonnement"));

        assert!(loader_line(&app).is_none());
    }

    /// T4 — ninja cursor. `turn_active` plus a streaming agent text line
    /// must render a `theme::BLADE_FRAMES` glyph, styled `theme::accent()`
    /// (vermillon), appended right after the agent's own text on the last
    /// rendered `Line` of that block.
    #[test]
    fn streaming_agent_line_carries_the_blade_cursor() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut app = App::new(None);
        app.begin_turn();
        app.apply_agent_event(&text_message("m1", "réponse"));

        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).expect("test backend terminal");
        terminal
            .draw(|frame| draw_chat(frame, &app, frame.area()))
            .expect("draw must succeed against a TestBackend");

        let buffer = terminal.backend().buffer();
        let blade_cells: Vec<(u16, u16)> = (0..buffer.area.height)
            .flat_map(|y| (0..buffer.area.width).map(move |x| (x, y)))
            .filter(|&(x, y)| {
                theme::BLADE_FRAMES.contains(&buffer[(x, y)].symbol().chars().next().unwrap_or(' '))
            })
            .collect();

        assert_eq!(
            blade_cells.len(),
            1,
            "exactly one blade glyph must be rendered"
        );
        let (bx, by) = blade_cells[0];
        assert_eq!(
            buffer[(bx, by)].fg,
            theme::VERMILLON,
            "the blade must be styled vermillon"
        );

        let row_text: String = (0..buffer.area.width)
            .map(|x| buffer[(x, by)].symbol().chars().next().unwrap_or(' '))
            .collect();
        assert!(
            row_text.contains("réponse"),
            "the blade must trail the streaming agent line's own text, got: {row_text:?}"
        );
    }

    #[test]
    fn blade_cursor_absent_once_turn_has_finished() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut app = App::new(None);
        app.begin_turn();
        app.apply_agent_event(&text_message("m1", "réponse"));
        app.finish_turn();

        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).expect("test backend terminal");
        terminal
            .draw(|frame| draw_chat(frame, &app, frame.area()))
            .expect("draw must succeed against a TestBackend");

        assert!(
            !any_blade_glyph(terminal.backend().buffer()),
            "no blade once the turn has ended"
        );
    }

    #[test]
    fn blade_cursor_absent_for_tool_line() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut app = App::new(None);
        app.begin_turn();
        let mut req = Message::assistant()
            .with_tool_request("t1", Ok(rmcp::model::CallToolRequestParams::new("shell")));
        req.id = Some("m1".to_string());
        app.apply_agent_event(&AgentEvent::Message(req));

        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).expect("test backend terminal");
        terminal
            .draw(|frame| draw_chat(frame, &app, frame.area()))
            .expect("draw must succeed against a TestBackend");

        assert!(
            !any_blade_glyph(terminal.backend().buffer()),
            "tool lines never carry the blade"
        );
    }

    #[test]
    fn blade_cursor_absent_for_thinking_line() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut app = App::new(None);
        app.begin_turn();
        app.show_thinking = true;
        app.apply_agent_event(&thinking_message("m1", "raisonnement"));

        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).expect("test backend terminal");
        terminal
            .draw(|frame| draw_chat(frame, &app, frame.area()))
            .expect("draw must succeed against a TestBackend");

        assert!(
            !any_blade_glyph(terminal.backend().buffer()),
            "thinking lines never carry the blade"
        );
    }

    #[test]
    fn blade_cursor_absent_for_system_line() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut app = App::new(None);
        app.begin_turn();
        app.push_system("intro");

        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).expect("test backend terminal");
        terminal
            .draw(|frame| draw_chat(frame, &app, frame.area()))
            .expect("draw must succeed against a TestBackend");

        assert!(
            !any_blade_glyph(terminal.backend().buffer()),
            "system lines never carry the blade"
        );
    }

    /// `draw_chat` is the only place that measures where each user turn
    /// starts (in wrapped-row coordinates) — Ctrl+↑/↓ (`App::jump_prev_turn`
    /// / `jump_next_turn`) reads `app.user_turn_rows` and has no other way
    /// to learn these offsets. Asserts the exact offsets (not just their
    /// order) with a first message long enough to genuinely wrap across
    /// several rows at the TestBackend's width, so a regression that
    /// undercounts wrapped rows (the char-count/width bug this replaced)
    /// would move these numbers, not just their relative order.
    #[test]
    fn draw_chat_records_a_row_offset_for_every_user_turn() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut app = App::new(None);
        app.push_system("intro");
        app.push_user(&"m".repeat(80));
        for i in 0..5 {
            app.push_system(&format!("filler {i}"));
        }
        app.push_user("second message");

        let backend = TestBackend::new(80, 10);
        let mut terminal = Terminal::new(backend).expect("test backend terminal");
        terminal
            .draw(|frame| draw(frame, &app))
            .expect("draw must succeed against a TestBackend");

        let rows = app.user_turn_rows.borrow();
        assert_eq!(rows.len(), 2, "one row offset per user turn");
        // Golden values measured against ratatui's real WordWrapper output
        // (chat width 76 for an 80-column TestBackend — see
        // `chat_content_rect`) — not recomputed via `line_wrapped_rows`
        // here, so the assertion can't pass by construction. Row math:
        // "intro" (1) + blank (1) = 2 → first offset. The 80-`m` message
        // (with the 7-cell `vous ▸ ` prefix) wraps across 3 rows + its
        // blank (4), then 5 filler lines at 2 rows each (content + blank,
        // 10) → 2 + 4 + 10 = 16, the second offset.
        assert_eq!(rows[0], 2);
        assert_eq!(rows[1], 16);
    }

    #[test]
    fn draw_chat_clears_stale_user_turn_rows_when_no_user_lines_remain() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut app = App::new(None);
        app.user_turn_rows.borrow_mut().push(7);
        app.push_system("only system output this draw");

        let backend = TestBackend::new(80, 10);
        let mut terminal = Terminal::new(backend).expect("test backend terminal");
        terminal
            .draw(|frame| draw(frame, &app))
            .expect("draw must succeed against a TestBackend");

        assert!(app.user_turn_rows.borrow().is_empty());
    }

    /// Regression: the width clamp used to apply `.max(20)` *after*
    /// `.min(available)`, so the floor could win over the ceiling — on a
    /// narrow column (e.g. the left side of the 72/28 SPEC-panel split on a
    /// small terminal) the palette was drawn wider than the space actually
    /// available and spilled past its own `input_area` into neighboring UI.
    /// `input_area` here is computed with the exact same `Layout` calls
    /// `draw()` uses for a 24x12 terminal with the SPEC panel visible (left
    /// column narrows to 17 columns), so the assertion can't pass by
    /// construction. `draw_palette` is driven directly (not the full
    /// `draw()`) because the SPEC panel is rendered *after* the palette in
    /// `draw()` and would silently paint over any overflow into its own
    /// column, masking the very bug this test targets.
    #[test]
    fn palette_does_not_panic_nor_overflow_on_a_narrow_terminal() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut app = App::new(None);
        for c in "/s".chars() {
            app.on_event(&key(KeyCode::Char(c)));
        }

        let root = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(0)])
            .split(Rect::new(0, 0, 24, 12));
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(72), Constraint::Percentage(28)])
            .split(root[1]);
        let left = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(3)])
            .split(cols[0]);
        let input_area = left[1];

        let backend = TestBackend::new(24, 12);
        let mut terminal = Terminal::new(backend).expect("test backend terminal");
        terminal
            .draw(|f| draw_palette(f, &app, input_area))
            .expect("draw_palette must not panic on a narrow input_area");

        let buffer = terminal.backend().buffer();
        let boundary = input_area.x + input_area.width - 1;
        for y in 0..buffer.area.height {
            for x in (boundary + 1)..buffer.area.width {
                assert_eq!(
                    buffer[(x, y)].symbol(),
                    " ",
                    "palette cell at ({x},{y}) spills past input_area's right edge (boundary {boundary})"
                );
            }
        }

        // Full-pipeline sanity check: the exact scenario the fix targets
        // (SPEC panel visible on a 24x12 terminal) must never panic ratatui.
        let mut full_app = App::new(None);
        full_app.toggle_spec_panel();
        for c in "/s".chars() {
            full_app.on_event(&key(KeyCode::Char(c)));
        }
        let backend = TestBackend::new(24, 12);
        let mut terminal = Terminal::new(backend).expect("test backend terminal");
        terminal
            .draw(|f| draw(f, &full_app))
            .expect("draw() must not panic on a narrow terminal with the SPEC panel visible");
    }
}
