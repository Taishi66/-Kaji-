use crate::tui::app::{App, ChatLine, Focus, RoledLine, Sender, ToolApprovalRequest};
use crate::tui::explorer::ExplorerState;
use crate::tui::viewer::{self, Viewer};
use crate::tui::{markdown, statusbar, theme};
use kaji_core::sdd::StageStatus;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};

pub fn draw(frame: &mut Frame, app: &App) {
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(frame.area());

    draw_header(frame, app, root[0]);
    draw_status_bar(frame, app, root[2]);

    // Three columns at most: explorer | chat/composer | viewer-or-SPEC. Both
    // side widths are decided against the full body width before anything is
    // split, so the viewer keeps its share whatever the explorer takes.
    let viewer_cols = if app.viewer.is_some() {
        viewer_width(root[1].width, app.explorer.is_some())
    } else {
        0
    };
    // The SPEC panel is percentage-sized rather than fixed, but the explorer
    // still owes it a reservation before claiming its own share.
    let right_cols = if viewer_cols > 0 {
        viewer_cols
    } else if app.spec_panel_visible() {
        (u32::from(root[1].width) * u32::from(SPEC_PERCENT) / 100) as u16
    } else {
        0
    };
    let explorer_cols = if app.explorer.is_some() {
        explorer_width(root[1].width, right_cols)
    } else {
        0
    };
    let (explorer_area, body) = if explorer_cols > 0 {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(explorer_cols), Constraint::Min(0)])
            .split(root[1]);
        (Some(cols[0]), cols[1])
    } else {
        (None, root[1])
    };

    // One right column, two possible tenants: the file viewer takes the slot
    // while it is open and hands it straight back to the SPEC panel on close —
    // nothing to save and restore, the panel's own visibility answers again.
    // A focused viewer takes no right column at all: it folds the chat away and
    // reads in its place (task 21), the explorer keeping the width it had.
    let right = if app.zoomed_viewer().is_some() {
        None
    } else if app.viewer.is_some() {
        Some([Constraint::Min(0), Constraint::Length(viewer_cols)])
    } else if app.spec_panel_visible() {
        Some([
            Constraint::Percentage(100 - SPEC_PERCENT),
            Constraint::Percentage(SPEC_PERCENT),
        ])
    } else {
        None
    };
    let (main_area, right_area) = match right {
        Some(constraints) => {
            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints(constraints)
                .split(body);
            (cols[0], Some(cols[1]))
        }
        None => (body, None),
    };

    // The composer stays under whichever pane owns the column, so a message in
    // progress and the turn's chrono remain visible while reading.
    let column = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(3)])
        .split(main_area);

    match app.zoomed_viewer() {
        Some(viewer) => draw_viewer(frame, app, viewer, column[0]),
        None => draw_chat(frame, app, column[0]),
    }
    draw_input(frame, app, column[1]);
    draw_palette(frame, app, column[1]);
    draw_mentions(frame, app, column[1]);
    if let Some(area) = right_area {
        match &app.viewer {
            Some(viewer) => draw_viewer(frame, app, viewer, area),
            None => draw_spec(frame, app, area),
        }
    }
    if let (Some(area), Some(explorer)) = (explorer_area, &app.explorer) {
        draw_explorer(frame, app, explorer, area);
    }
    draw_finder(frame, app);
    draw_theme_picker(frame, app);
    draw_editor_picker(frame, app);

    if app.gate_open {
        draw_gate_modal(frame);
    } else if app.pending_restore.is_some() {
        draw_restore_confirm_modal(frame, app.pending_restore_files_only);
    } else if let Some(approval) = &app.tool_approval {
        draw_tool_approval_modal(frame, approval, app.approval_detail.as_deref());
    }
}

/// The session and the running goal, nothing else: the telemetry belongs to the
/// status bar ([`statusbar::render`]) and used to read twice.
fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let mut spans = vec![
        Span::styled(theme::KAJI_GLYPH, theme::title()),
        Span::styled(format!(" kaji · {}", app.header), theme::dim()),
    ];
    if let Some(badge) = goal_badge(app) {
        spans.push(Span::styled(badge, theme::accent()));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_status_bar(frame: &mut Frame, app: &App, area: Rect) {
    frame.render_widget(Paragraph::new(statusbar::render(app, area.width)), area);
}

/// Condition width in the header badge — the full text stays available under
/// `/goal`; the badge only has to say which goal is running.
const GOAL_BADGE_CONDITION_CHARS: usize = 28;

/// `None` once the loop is idle: a finished goal keeps its state for `/goal`,
/// but must stop claiming the header.
fn goal_badge(app: &App) -> Option<String> {
    let goal = app.goal.as_ref().filter(|goal| goal.is_active())?;
    let condition = sanitize_for_display(&goal.condition.replace('\n', "␊"));
    let condition = if condition.chars().count() > GOAL_BADGE_CONDITION_CHARS {
        let head: String = condition
            .chars()
            .take(GOAL_BADGE_CONDITION_CHARS - 1)
            .collect();
        format!("{head}…")
    } else {
        condition
    };
    Some(format!(
        " · 目標 {condition} · it {}/{} · {}",
        goal.iteration,
        goal.max_iterations,
        goal.phase.label()
    ))
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
        let content = format!(
            "{}{} {} {spinner}",
            theme::SYSTEM_PREFIX,
            theme::TOOL_GLYPH,
            tool.name
        );
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

/// On-demand report blocks (`/cost`, `/docker`) — each span carries a
/// semantic role that becomes a `Style` HERE, at every frame, so a `/theme`
/// in session re-colours blocks pushed before it. Only the leading
/// `SYSTEM_PREFIX` marker is spliced onto the first line so the block still
/// reads as a system message.
fn push_rendered_lines(lines: &mut Vec<Line<'static>>, rendered: &[RoledLine]) {
    for (index, roled) in rendered.iter().enumerate() {
        let mut spans = Vec::with_capacity(roled.len() + 1);
        if index == 0 {
            spans.push(Span::styled(theme::SYSTEM_PREFIX, theme::dim()));
        }
        spans.extend(
            roled
                .iter()
                .map(|span| Span::styled(span.text.clone(), theme::style(span.role))),
        );
        lines.push(Line::from(spans));
    }
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
        (
            format!(" {} {elapsed}s — Esc annule ", theme::ELAPSED_GLYPH),
            theme::accent(),
        )
    } else {
        (" message ".to_string(), theme::title())
    };
    let (title, title_style) = if app.steer_len() > 0 {
        (
            format!(
                "{title} {} {} en file — Ctrl+S ",
                theme::STEER_GLYPH,
                app.steer_len()
            ),
            theme::accent(),
        )
    } else {
        (title, title_style)
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme::border_inactive())
        .title(Span::styled(title, title_style));
    let inner = block.inner(area);
    // Left breathing room so the text/cursor doesn't collide with the border,
    // mirroring the chat's `CHAT_HORIZONTAL_MARGIN`.
    let content = Rect {
        x: inner.x + 1,
        width: inner.width.saturating_sub(1),
        ..inner
    };

    let paragraph = if app.input.is_empty() && !app.turn_active {
        if app.suggestion_loading {
            Paragraph::new("suggestion…").style(theme::dim())
        } else if let Some(suggestion) = app.suggestion.as_ref() {
            Paragraph::new(suggestion.clone()).style(theme::dim())
        } else {
            Paragraph::new("écris ici…").style(theme::dim())
        }
    } else {
        Paragraph::new(app.input.as_str()).style(theme::text())
    };
    let scroll_x = app.input_scroll_x(content.width.max(1));
    frame.render_widget(block, area);
    frame.render_widget(paragraph.scroll((0, scroll_x)), content);

    let cursor_x =
        content.x + (app.input_cursor_chars() - scroll_x).min(content.width.saturating_sub(1));
    frame.set_cursor_position((cursor_x, content.y));
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

/// @-mention completion dropdown (item 4 ante) — same anchored-above-input
/// overlay as the command palette, listing path completions for the live
/// `@` fragment. Selection is cyclic ↑/↓, Tab/Enter confirms.
fn draw_mentions(frame: &mut Frame, app: &App, input_area: Rect) {
    let indexing = app.mention_indexing_visible();
    if !app.mention_dropdown_visible() && !indexing {
        return;
    }
    let empty: Vec<String> = Vec::new();
    let matches = if indexing {
        &empty
    } else {
        &app.mention_matches
    };
    let hint = if indexing {
        Some("indexation…")
    } else if app.mention_index_truncated() {
        Some("… index tronqué (20 000 entrées)")
    } else {
        None
    };
    let inner_w = matches
        .iter()
        .map(|m| m.chars().count())
        .chain(hint.map(|h| h.chars().count()))
        .max()
        .unwrap_or(0) as u16;
    let width = (inner_w + 4)
        .max(20)
        .min(input_area.width.saturating_sub(2));
    let height = ((matches.len() + usize::from(hint.is_some())) as u16 + 2).min(input_area.y);
    if width < 4 || height < 3 {
        return;
    }
    let rows = (height - 2) as usize;
    // The hint owns the last row; the matches scroll inside what's left.
    let rows = if hint.is_some() {
        rows.saturating_sub(1)
    } else {
        rows
    };
    let first = app.mention_selected.saturating_sub(rows.saturating_sub(1));
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
        .title(" fichiers ")
        .title_bottom(Line::from(" ↑↓ choisir · ⏎/Tab compléter · esc ").style(theme::dim()));
    let mut lines: Vec<Line> = matches
        .iter()
        .enumerate()
        .skip(first)
        .take(rows)
        .map(|(i, path)| {
            let selected = i == app.mention_selected;
            let marker = if selected { "▸ " } else { "  " };
            let style = if selected {
                theme::accent()
            } else {
                theme::text()
            };
            Line::from(vec![
                Span::styled(marker, theme::accent()),
                Span::styled(path.clone(), style),
            ])
        })
        .collect();
    if let Some(hint) = hint {
        lines.push(Line::from(Span::styled(hint, theme::dim())));
    }
    frame.render_widget(Paragraph::new(Text::from(lines)).block(block), area);
}

const FINDER_MAX_WIDTH: u16 = 100;
const FINDER_MAX_HEIGHT: u16 = 24;
const FINDER_PROMPT: &str = "› ";

/// Fuzzy file finder (`Ctrl+P`, `/files`) — centered, unlike the palette and
/// the mention dropdown which hug the composer: this one is a place you go to,
/// not a completion of what you are already typing. Drawn after the panes and
/// before the y/n modals, which stay on top of everything.
fn draw_finder(frame: &mut Frame, app: &App) {
    let Some(finder) = &app.finder else {
        return;
    };
    let full = frame.area();
    let width = (u32::from(full.width) * 90 / 100).min(FINDER_MAX_WIDTH.into()) as u16;
    let height = (u32::from(full.height) * 70 / 100).min(FINDER_MAX_HEIGHT.into()) as u16;
    if width < 12 || height < 5 {
        return;
    }
    let area = Rect {
        x: full.x + (full.width - width) / 2,
        y: full.y + (full.height - height) / 2,
        width,
        height,
    };
    let total = finder.results.len();
    let position = if total == 0 { 0 } else { finder.selected + 1 };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme::border_active())
        .title(Span::styled(" ⌕ fichiers ", theme::title()))
        .title_bottom(
            Line::from(format!(
                " Enter ouvrir · Tab attacher @ · Esc fermer · {position}/{total} "
            ))
            .style(theme::dim()),
        );
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(inner);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(FINDER_PROMPT, theme::accent()),
            Span::styled(sanitize_for_display(&finder.query), theme::text()),
        ])),
        rows[0],
    );
    let cursor_x = rows[0].x + (FINDER_PROMPT.chars().count() + finder.cursor) as u16;
    frame.set_cursor_position((cursor_x.min(rows[0].right().saturating_sub(1)), rows[0].y));

    let hint = if app.finder_indexing() {
        Some("indexation…")
    } else if app.mention_index_truncated() {
        Some("… index tronqué (20 000 entrées)")
    } else {
        None
    };
    let list_rows = (rows[1].height as usize).saturating_sub(usize::from(hint.is_some()));
    // Sliding window: the selection stays visible however far down the list it
    // has walked.
    let first = finder.selected.saturating_sub(list_rows.saturating_sub(1));
    let mut lines: Vec<Line> = finder
        .results
        .iter()
        .enumerate()
        .skip(first)
        .take(list_rows)
        .map(|(i, path)| {
            let selected = i == finder.selected;
            let marker = if selected { "▸ " } else { "  " };
            let style = if selected {
                theme::accent()
            } else {
                theme::text()
            };
            Line::from(vec![
                Span::styled(marker, theme::accent()),
                Span::styled(sanitize_for_display(path), style),
            ])
        })
        .collect();
    if let Some(hint) = hint {
        lines.push(Line::from(Span::styled(hint, theme::dim())));
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), rows[1]);
}

/// Assez large pour que le pied tienne d'un tenant dans le cadre, et jamais
/// plus de 90 % du terminal.
const THEME_PICKER_WIDTH: u16 = 41;
const THEME_PICKER_FOOTER: &str = "↑↓ aperçu · Enter valider · Esc annuler";

/// Sélecteur de thème (`/theme`) — une liste nue, sans pastilles de couleur :
/// la palette sélectionnée est déjà appliquée, donc tout ce qui entoure la
/// boîte est l'aperçu. Même z-order que le finder.
fn draw_theme_picker(frame: &mut Frame, app: &App) {
    let Some(picker) = &app.theme_picker else {
        return;
    };
    let full = frame.area();
    let width = (u32::from(full.width) * 90 / 100).min(THEME_PICKER_WIDTH.into()) as u16;
    let height = theme::THEMES.len() as u16 + 3;
    if width < 12 || full.height < height {
        return;
    }
    let area = Rect {
        x: full.x + (full.width - width) / 2,
        y: full.y + (full.height - height) / 2,
        width,
        height,
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme::border_active())
        .title(Span::styled(" thème ", theme::title()));
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);

    let mut lines: Vec<Line> = theme::THEMES
        .iter()
        .enumerate()
        .map(|(i, palette)| {
            let selected = i == picker.selected;
            let marker = if selected { "▸ " } else { "  " };
            let style = if selected {
                theme::accent()
            } else {
                theme::text()
            };
            let mut spans = vec![
                Span::styled(marker, theme::accent()),
                Span::styled(palette.name, style),
            ];
            if i == picker.initial {
                spans.push(Span::styled(" (actuel)", theme::dim()));
            }
            Line::from(spans)
        })
        .collect();
    lines.push(Line::from(Span::styled(THEME_PICKER_FOOTER, theme::dim())));
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

/// Plus large que le sélecteur de thème : une ligne y porte une commande, pas
/// un nom de palette.
const EDITOR_PICKER_WIDTH: u16 = 52;
const EDITOR_PICKER_FOOTER: &str = "↑↓ · Enter choisir · Esc annuler";

/// Sélecteur d'éditeur (`/editor`) — les éditeurs détectés sur le `PATH`, plus
/// `$VISUAL`/`$EDITOR` quand l'environnement en propose un. Rien ne s'applique
/// avant Enter : contrairement au thème, il n'y a pas d'aperçu à donner.
fn draw_editor_picker(frame: &mut Frame, app: &App) {
    let Some(picker) = &app.editor_picker else {
        return;
    };
    let full = frame.area();
    let width = (u32::from(full.width) * 90 / 100).min(EDITOR_PICKER_WIDTH.into()) as u16;
    let height = picker.rows.len() as u16 + 3;
    if width < 12 || full.height < height {
        return;
    }
    let area = Rect {
        x: full.x + (full.width - width) / 2,
        y: full.y + (full.height - height) / 2,
        width,
        height,
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme::border_active())
        .title(Span::styled(" éditeur ", theme::title()));
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);

    let mut lines: Vec<Line> = picker
        .rows
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let selected = i == picker.selected;
            let marker = if selected { "▸ " } else { "  " };
            let style = if selected {
                theme::accent()
            } else {
                theme::text()
            };
            let mut spans = vec![
                Span::styled(marker, theme::accent()),
                Span::styled(row.name().to_string(), style),
            ];
            if let Some(detail) = row.detail() {
                spans.push(Span::styled(format!("  {detail}"), theme::dim()));
            }
            if picker.current == Some(i) {
                spans.push(Span::styled(" (actuel)", theme::dim()));
            }
            Line::from(spans)
        })
        .collect();
    lines.push(Line::from(Span::styled(EDITOR_PICKER_FOOTER, theme::dim())));
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

/// Share of the body the SPEC panel takes when it owns the right column.
const SPEC_PERCENT: u16 = 28;

const VIEWER_PERCENT: u16 = 45;
/// Share of the body the viewer takes once the explorer tree is also open.
/// Lower than [`VIEWER_PERCENT`] because the chat, not the reader, is why the
/// side panes exist — a second pane opening must not grow the reader's cut of
/// the space the explorer just freed, or the chat keeps shrinking every time
/// another pane joins it.
const VIEWER_PERCENT_WITH_EXPLORER: u16 = 40;
const VIEWER_MIN_WIDTH: u16 = 40;
/// Columns the chat keeps whatever the side panes ask for — a layout that eats
/// the whole terminal would hide the conversation they are opened for.
const MIN_CHAT_WIDTH: u16 = 20;

/// Percentage of the body the viewer claims depends on whether the explorer
/// tree is also open: with the tree open, the viewer is sized off what is
/// left after the tree's share rather than the full body, and at a lower
/// percentage — see [`VIEWER_PERCENT_WITH_EXPLORER`].
fn viewer_width(total: u16, explorer_open: bool) -> u16 {
    let (base, percent) = if explorer_open {
        (
            total.saturating_sub(explorer_target(total)),
            VIEWER_PERCENT_WITH_EXPLORER,
        )
    } else {
        (total, VIEWER_PERCENT)
    };
    let target = (u32::from(base) * u32::from(percent) / 100) as u16;
    target
        .max(VIEWER_MIN_WIDTH)
        .min(total.saturating_sub(MIN_CHAT_WIDTH))
}

const EXPLORER_PERCENT: u16 = 22;
const EXPLORER_MIN_WIDTH: u16 = 24;
const EXPLORER_MAX_WIDTH: u16 = 40;

fn explorer_target(total: u16) -> u16 {
    ((u32::from(total) * u32::from(EXPLORER_PERCENT) / 100) as u16)
        .clamp(EXPLORER_MIN_WIDTH, EXPLORER_MAX_WIDTH)
}

/// Sacrifice order on a narrow terminal: the explorer gives way first, then the
/// viewer, and the chat never drops under [`MIN_CHAT_WIDTH`]. `right` is what
/// the viewer already reserved, so the tree only ever takes what is left; `0`
/// means the pane stays open but unpainted rather than crushing the chat.
fn explorer_width(total: u16, right: u16) -> u16 {
    let room = total.saturating_sub(right).saturating_sub(MIN_CHAT_WIDTH);
    if room < EXPLORER_MIN_WIDTH {
        return 0;
    }
    explorer_target(total).min(room)
}

/// Left-column file tree (task 9). The pane keeps no scroll offset of its own:
/// the window slides to keep the cursor row visible, the way the finder's list
/// does.
fn draw_explorer(frame: &mut Frame, app: &App, explorer: &ExplorerState, area: Rect) {
    let root_label = explorer
        .root
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| explorer.root.to_string_lossy().into_owned());
    let footer = if explorer.filter.is_empty() {
        " . dotfiles · a attacher · q fermer ".to_string()
    } else {
        format!(
            " ⌕ {} · {} éléments · Esc vide le filtre ",
            sanitize_for_display(&explorer.filter),
            explorer.entry_count()
        )
    };
    let border = if app.focus == Focus::Explorer {
        theme::border_active()
    } else {
        theme::border_inactive()
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border)
        .title(Span::styled(
            format!(
                " {} {} · {} ",
                theme::EXPLORER_GLYPH,
                sanitize_for_display(&root_label),
                explorer.entry_count()
            ),
            theme::title(),
        ))
        .title_bottom(Line::from(footer).style(theme::dim()));

    let rows = usize::from(area.height.saturating_sub(2)).max(1);
    let visible = explorer.visible();
    let position = visible
        .iter()
        .position(|index| *index == explorer.cursor)
        .unwrap_or(0);
    let first = position.saturating_sub(rows.saturating_sub(1));
    let lines: Vec<Line> = visible
        .iter()
        .enumerate()
        .skip(first)
        .take(rows)
        .map(|(row, index)| {
            let node = &explorer.nodes[*index];
            let (glyph, label) = match node.overflow {
                Some(dropped) => ("  ", format!("… +{dropped}")),
                None if node.is_dir => (
                    if node.expanded { "▾ " } else { "▸ " },
                    sanitize_for_display(&node.name),
                ),
                None => ("  ", sanitize_for_display(&node.name)),
            };
            let style = if row == position {
                theme::accent()
            } else if node.overflow.is_some() {
                theme::dim()
            } else {
                theme::text()
            };
            let indent = "  ".repeat(node.depth);
            Line::from(Span::styled(format!("{indent}{glyph}{label}"), style))
        })
        .collect();
    frame.render_widget(Paragraph::new(Text::from(lines)).block(block), area);
}

/// Read-only file pane (task 8). Lines are already sanitized and tab-expanded
/// by `viewer::load`; long ones are clipped rather than wrapped, so line
/// numbers keep matching the file's.
fn draw_viewer(frame: &mut Frame, app: &App, viewer: &Viewer, area: Rect) {
    app.viewer_area.set(area);
    let visible = area.height.saturating_sub(2) as usize;
    let total = viewer.lines.len();
    let first = viewer.scroll.min(total.saturating_sub(1));
    let title = if viewer.binary {
        format!(
            " {} {} ",
            theme::VIEWER_GLYPH,
            sanitize_for_display(&viewer.path)
        )
    } else {
        let last = (first + visible).min(total);
        let start = if total == 0 { 0 } else { first + 1 };
        format!(
            " {} {} · L{start}-{last}/{total} ",
            theme::VIEWER_GLYPH,
            sanitize_for_display(&viewer.path)
        )
    };
    // `Ctrl+O` is only worth naming while the chat is folded behind the pane —
    // it is what brings it back.
    let focused = app.focus == Focus::Viewer;
    let keys = if focused {
        "j/k défiler · e éditer · r recharger · a attacher @ · q fermer · Ctrl+O chat"
    } else {
        "j/k défiler · e éditer · r recharger · a attacher @ · q fermer"
    };
    let footer = if viewer.truncated {
        format!(" … tronqué ({} lus) · {keys} ", viewer::read_limit_label())
    } else {
        format!(" {keys} ")
    };
    let border = if focused {
        theme::border_active()
    } else {
        theme::border_inactive()
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border)
        .title(Span::styled(title, theme::title()))
        .title_bottom(Line::from(footer).style(theme::dim()));

    let lines: Vec<Line> = if viewer.binary {
        viewer
            .lines
            .iter()
            .map(|text| Line::from(Span::styled(text.clone(), theme::dim())))
            .collect()
    } else {
        let number_width = total.to_string().len();
        viewer
            .lines
            .iter()
            .enumerate()
            .skip(first)
            .take(visible)
            .map(|(i, text)| {
                Line::from(vec![
                    Span::styled(format!("{:>number_width$} ", i + 1), theme::dim()),
                    Span::styled(text.clone(), theme::text()),
                ])
            })
            .collect()
    };
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

/// The `/restore` y/n confirmation. When the target is a pre-restore
/// safety net (`files_only`), the modal must say so: the tree is rewound
/// but the conversation is left untouched, and announcing anything else
/// would be a lie.
fn draw_restore_confirm_modal(frame: &mut Frame, files_only: bool) {
    let area = centered_rect(60, 20, frame.area());
    let title = if files_only {
        format!(" {} restaurer le filet ? (y/n) ", theme::GATE_GLYPH)
    } else {
        format!(" {} restaurer le checkpoint ? (y/n) ", theme::GATE_GLYPH)
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme::title())
        .title(title);
    let body = if files_only {
        "Filet de sécurité (pre-restore) : FICHIERS SEULS.
L'arbre de travail sera rembobiné; la conversation est laissée telle quelle, ses messages supprimés sont irrécupérables."
    } else {
        "L'arbre de travail et la conversation seront ramenés à l'état de ce checkpoint."
    };
    let paragraph = Paragraph::new(body).block(block).wrap(Wrap { trim: true });
    frame.render_widget(Clear, area);
    frame.render_widget(paragraph, area);
}

fn draw_gate_modal(frame: &mut Frame) {
    let area = centered_rect(60, 20, frame.area());
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme::title())
        .title(format!(
            " {} Gate — approuver la SPEC ? (y/n) ",
            theme::GATE_GLYPH
        ));
    let paragraph = Paragraph::new("y = approuver   n / Esc = refuser")
        .block(block)
        .wrap(Wrap { trim: true });
    frame.render_widget(Clear, area);
    frame.render_widget(paragraph, area);
}

/// Anti-masquage : rend visible tout caractère qui pourrait cacher une
/// partie d'une commande à l'humain qui approuve — tabs qui poussent du
/// contenu hors champ, contrôles C0/C1 et ESC (tue les séquences ANSI à la
/// source), zero-width/invisibles, overrides et isolats bidi qui
/// inversent l'ordre visuel. `\n` est préservé : le rendu multi-ligne de
/// `Paragraph` (`Text::from(&str)`, qui découpe sur `\n`) en dépend.
pub(crate) fn sanitize_for_display(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\n' => out.push(c),
            '\u{0000}'..='\u{001F}' => {
                // Unicode "Control Pictures" block mirrors C0 1:1 at
                // 0x2400 + code point (TAB 0x09 → ␉ U+2409, ESC 0x1B → ␛
                // U+241B, CR 0x0D → ␍ U+240D, ...).
                out.push(char::from_u32(0x2400 + c as u32).expect("C0 control picture"));
            }
            '\u{007F}' => out.push('\u{2421}'),
            '\u{0080}'..='\u{009F}' => out.push('\u{FFFD}'),
            '\u{200B}' | '\u{200C}' | '\u{200D}' | '\u{2060}' | '\u{FEFF}' => out.push('·'),
            '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' => out.push_str("‹bidi›"),
            _ => out.push(c),
        }
    }
    out
}

/// Measures wrapped rows against the wrap mode the caller will actually
/// render with — same idiom as `line_wrapped_rows`, targeted at this modal's
/// own wrap settings rather than the chat's. The detail panel renders
/// untrimmed (a diff's leading spaces are content), so measuring it as trimmed
/// would under-count rows and let the tail clip silently.
fn modal_wrapped_rows(text: &str, width: u16, trim: bool) -> usize {
    Paragraph::new(text)
        .wrap(Wrap { trim })
        .line_count(width.max(1))
}

/// `Paragraph` neither scrolls nor truncates on its own: content past
/// `height` wrapped rows is simply not painted — confirmed against this
/// exact modal by `approval_modal_renders_hostile_command_with_visible_markers`
/// before this function existed (hostile separators vanished with no
/// indication anything was cut). Anti-masquage forbids a silent hidden
/// tail, so this clips the sanitized body itself and appends an explicit
/// `… (+N car.)` marker sized to what's actually hidden. Bounds the search
/// to `width * height` chars up front — the maximum ever paintable
/// regardless of wrapping — so a pathologically long hostile string can't
/// turn this into quadratic work.
fn truncate_for_modal(text: &str, width: u16, height: u16, trim: bool) -> String {
    let width = width.max(1);
    let height = height.max(1);
    if modal_wrapped_rows(text, width, trim) <= height as usize {
        return text.to_string();
    }
    let total_chars = text.chars().count();
    let budget = (width as usize).saturating_mul(height as usize);
    let chars: Vec<char> = text.chars().take(budget.min(total_chars)).collect();

    let fits = |cut: usize| -> bool {
        let hidden = total_chars - cut;
        let candidate = format!(
            "{}… (+{hidden} car.)",
            chars[..cut].iter().collect::<String>()
        );
        modal_wrapped_rows(&candidate, width, trim) <= height as usize
    };

    let mut lo = 0usize;
    let mut hi = chars.len();
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        if fits(mid) {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    let hidden = total_chars - lo;
    format!(
        "{}… (+{hidden} car.)",
        chars[..lo].iter().collect::<String>()
    )
}

/// The four answers plus the derived grant `s`/`a` would persist — an approver
/// must be able to read what a session-wide or permanent grant would cover
/// before pressing the key that writes it.
fn approval_answers(approval: &ToolApprovalRequest, detail_open: bool) -> String {
    let detail_key = if detail_open {
        "Tab = masquer le détail"
    } else {
        "Tab = détail"
    };
    format!(
        "y = une fois · n / Esc = refuser · {detail_key}\ns = pour la session · a = toujours : {}",
        sanitize_for_display(&approval.grant_label())
    )
}

fn draw_tool_approval_modal(
    frame: &mut Frame,
    approval: &ToolApprovalRequest,
    detail: Option<&str>,
) {
    let area = match detail {
        Some(_) => centered_rect(80, 60, frame.area()),
        None => centered_rect(60, 20, frame.area()),
    };
    let tool_name = sanitize_for_display(&approval.tool_name);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme::border_active())
        .title(format!(
            " {} confirmation d'outil — {} (y/s/a/n) ",
            theme::GATE_GLYPH,
            tool_name
        ));
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);

    let mut head = String::new();
    if let Some(prompt) = &approval.prompt {
        head.push_str(&sanitize_for_display(prompt));
        head.push('\n');
    }
    head.push_str(&approval_answers(approval, detail.is_some()));

    let Some(detail) = detail else {
        let body = truncate_for_modal(&head, inner.width, inner.height, true);
        frame.render_widget(Paragraph::new(body).wrap(Wrap { trim: true }), inner);
        return;
    };

    // The answers must never be the half that gets clipped: they get the rows
    // they need up to half the modal, and the detail takes what is left.
    let head_rows = modal_wrapped_rows(&head, inner.width, true)
        .clamp(1, (inner.height / 2).max(1) as usize) as u16;
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(head_rows), Constraint::Min(0)])
        .split(inner);
    frame.render_widget(
        Paragraph::new(truncate_for_modal(
            &head,
            rows[0].width,
            rows[0].height,
            true,
        ))
        .wrap(Wrap { trim: true }),
        rows[0],
    );
    let body = truncate_for_modal(
        &sanitize_for_display(detail),
        rows[1].width,
        rows[1].height,
        false,
    );
    frame.render_widget(
        Paragraph::new(body)
            .style(theme::dim())
            .wrap(Wrap { trim: false }),
        rows[1],
    );
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
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

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
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

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
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

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
        let _theme = theme::test_guard();
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
    /// must render a `theme::BLADE_FRAMES` glyph, styled `theme::accent()`,
    /// appended right after the agent's own text on the last rendered
    /// `Line` of that block.
    #[test]
    fn streaming_agent_line_carries_the_blade_cursor() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let _theme = theme::test_guard();
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
            theme::accent_color(),
            "the blade must be styled with the palette accent"
        );

        let row_text: String = (0..buffer.area.width)
            .map(|x| buffer[(x, by)].symbol().chars().next().unwrap_or(' '))
            .collect();
        assert!(
            row_text.contains("réponse"),
            "the blade must trail the streaming agent line's own text, got: {row_text:?}"
        );
    }

    /// Buffer du chat seul dessiné dans une grille 80×20 — ce que le terminal
    /// affiche, pas les `Line` construites.
    fn drawn_chat(app: &App) -> ratatui::buffer::Buffer {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).expect("test backend terminal");
        terminal
            .draw(|frame| draw_chat(frame, app, frame.area()))
            .expect("draw must succeed against a TestBackend");
        terminal.backend().buffer().clone()
    }

    fn row_containing(buffer: &ratatui::buffer::Buffer, needle: &str) -> u16 {
        for y in 0..buffer.area.height {
            let row: String = (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol().chars().next().unwrap_or(' '))
                .collect();
            if row.contains(needle) {
                return y;
            }
        }
        panic!("aucune ligne du chat ne contient {needle:?}");
    }

    fn cell_starting_with(buffer: &ratatui::buffer::Buffer, row: u16, head: char) -> u16 {
        (0..buffer.area.width)
            .find(|&x| buffer[(x, row)].symbol().starts_with(head))
            .unwrap_or_else(|| panic!("aucune cellule de la ligne {row} ne commence par {head:?}"))
    }

    /// Couleur de la première cellule portant l'initiale de `needle`, dans la
    /// première ligne du chat dessiné qui contient `needle`.
    fn drawn_fg(app: &App, needle: &str) -> ratatui::style::Color {
        let buffer = drawn_chat(app);
        let row = row_containing(&buffer, needle);
        let head = needle.chars().next().expect("non-empty needle");
        buffer[(cell_starting_with(&buffer, row, head), row)].fg
    }

    /// Rectangle du texte de chat pour un buffer dessiné par [`drawn_chat`] —
    /// mêmes bordures et marges que [`draw_chat`].
    fn drawn_chat_rect(buffer: &ratatui::buffer::Buffer) -> Rect {
        chat_content_rect(Block::default().borders(Borders::ALL).inner(buffer.area))
    }

    /// Sans la bande de fond, un prompt et une réponse sont indiscernables en
    /// `mono`, où toutes les teintes de texte se ressemblent.
    #[test]
    fn a_user_prompt_carries_the_palette_band_and_an_agent_line_does_not() {
        let _theme = theme::test_guard();

        for name in ["mono", "zen"] {
            theme::set_active(name).expect("thème intégré");
            let mut app = App::new(None);
            app.push_user("bonjour");
            app.apply_agent_event(&text_message("m1", "réponse"));

            let buffer = drawn_chat(&app);
            let band = theme::active().user_bg;
            let prompt_row = row_containing(&buffer, "bonjour");
            let prefix_x = cell_starting_with(&buffer, prompt_row, 'v');
            let text_x = cell_starting_with(&buffer, prompt_row, 'b');

            assert_eq!(buffer[(prefix_x, prompt_row)].bg, band, "{name} : préfixe");
            assert_eq!(buffer[(text_x, prompt_row)].bg, band, "{name} : texte");
            assert_eq!(
                buffer[(text_x, prompt_row)].fg,
                theme::user_color(),
                "{name}"
            );

            let agent_row = row_containing(&buffer, "réponse");
            let agent_x = cell_starting_with(&buffer, agent_row, 'r');
            assert_ne!(
                buffer[(agent_x, agent_row)].bg,
                band,
                "{name} : la réponse reste hors bande"
            );
        }
    }

    /// `Paragraph` ne stylise que les cellules portant un grapheme (ratatui
    /// 0.30, `render_line`) : le fond d'une `Line` ne peut pas atteindre le
    /// bord droit du chat, la bande s'arrête donc au dernier caractère.
    #[test]
    fn the_user_band_covers_the_prompt_text_and_stops_there() {
        let _theme = theme::test_guard();
        theme::set_active("zen").expect("zen is a built-in theme");
        let mut app = App::new(None);
        app.push_user("bonjour");

        let buffer = drawn_chat(&app);
        let band = theme::active().user_bg;
        let row = row_containing(&buffer, "bonjour");
        let chat_rect = drawn_chat_rect(&buffer);
        let text_end = cell_starting_with(&buffer, row, 'b') + "bonjour".len() as u16;

        for x in chat_rect.x..text_end {
            assert_eq!(buffer[(x, row)].bg, band, "colonne {x}");
        }
        assert_ne!(
            buffer[(chat_rect.x + chat_rect.width - 1, row)].bg,
            band,
            "la bande ne va pas jusqu'au bord du chat"
        );
    }

    #[test]
    fn a_user_prompt_band_follows_a_theme_change_after_the_fact() {
        let _theme = theme::test_guard();
        theme::set_active("zen").expect("zen is a built-in theme");
        let mut app = App::new(None);
        app.push_user("bonjour");
        let zen_band = theme::active().user_bg;

        theme::set_active("mono").expect("mono is a built-in theme");

        let buffer = drawn_chat(&app);
        let row = row_containing(&buffer, "bonjour");
        let x = cell_starting_with(&buffer, row, 'b');

        assert_eq!(buffer[(x, row)].bg, theme::active().user_bg);
        assert_ne!(
            buffer[(x, row)].bg,
            zen_band,
            "la palette du push ne doit rien figer"
        );
    }

    /// Un bloc `rendered` poussé sous un thème doit se re-colorer au draw
    /// suivant : après `/theme`, la couleur figée au push serait celle de
    /// l'ancienne palette.
    #[test]
    fn a_rendered_system_line_follows_a_theme_change_after_the_fact() {
        let _theme = theme::test_guard();
        theme::set_active("zen").expect("zen is a built-in theme");
        let mut app = App::new(None);
        app.push_error("boom");
        let zen_alert = theme::accent_color();

        theme::set_active("nord").expect("nord is a built-in theme");

        let fg = drawn_fg(&app, "boom");
        assert_eq!(fg, theme::accent_color(), "l'erreur suit la palette active");
        assert_ne!(fg, zen_alert, "la palette du push ne doit rien figer");
    }

    /// Même contrat pour un bloc rapport aligné (`/cost`) — son titre est le
    /// span le plus visiblement teinté du bloc.
    #[test]
    fn a_cost_block_is_drawn_with_the_theme_active_at_draw_time() {
        use crate::tui::report;
        use kaji::session::{UsageAggregate, UsageWindows};

        let aggregate = |input: i64, output: i64| UsageAggregate {
            input_tokens: input,
            output_tokens: output,
            total_tokens: input + output,
            cost: None,
        };
        let windows = UsageWindows {
            session: aggregate(10, 2),
            last_5h: aggregate(100, 20),
            last_7d: aggregate(1_000, 200),
        };

        let _theme = theme::test_guard();
        theme::set_active("zen").expect("zen is a built-in theme");
        let mut app = App::new(None);
        app.push_system_lines(report::cost_table_lines(
            &windows, "ollama", "qwen", None, None,
        ));
        let zen_title = theme::gold_color();

        theme::set_active("gruvbox").expect("gruvbox is a built-in theme");

        let fg = drawn_fg(&app, "/cost");
        assert_eq!(fg, theme::gold_color(), "le titre suit la palette active");
        assert_ne!(fg, zen_title, "la palette du push ne doit rien figer");
    }

    #[test]
    fn blade_cursor_absent_once_turn_has_finished() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

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
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

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
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

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
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

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
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

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
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

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
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

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

    /// Anti-masquage — tabs (push content out of the box) and ESC (kills
    /// ANSI sequences at the source) must become visible control pictures;
    /// the raw bytes must never survive into what the approver reads.
    #[test]
    fn sanitize_reveals_tabs_and_controls() {
        let hostile = "echo ok\t\x1b[2K && rm -rf /";
        let sanitized = sanitize_for_display(hostile);

        assert!(
            sanitized.contains('␉'),
            "tab must surface as a visible control picture, got: {sanitized:?}"
        );
        assert!(
            sanitized.contains('␛'),
            "ESC must surface as a visible control picture, got: {sanitized:?}"
        );
        assert!(!sanitized.contains('\t'), "raw tab must not survive");
        assert!(!sanitized.contains('\x1b'), "raw ESC must not survive");
    }

    /// Zero-width chars (invisible padding/joiners) and bidi overrides
    /// (visual reordering) are the other two masking primitives from the
    /// Claude Code 2.1.223 bypass set — both must leave a visible trace.
    #[test]
    fn sanitize_reveals_zero_width_and_bidi() {
        let hostile = "rm\u{200B} -rf\u{202E} /tmp";
        let sanitized = sanitize_for_display(hostile);

        assert!(
            sanitized.contains('·'),
            "zero-width space must surface as a visible marker, got: {sanitized:?}"
        );
        assert!(
            sanitized.contains("‹bidi›"),
            "RLO bidi override must surface as a visible marker, got: {sanitized:?}"
        );
        assert!(
            !sanitized.contains('\u{200B}'),
            "raw zero-width space must not survive"
        );
        assert!(
            !sanitized.contains('\u{202E}'),
            "raw RLO override must not survive"
        );
    }

    /// The helper must be a no-op for legitimate text — plain ASCII
    /// commands and non-Latin scripts (CJK here) pass through byte-for-byte
    /// identical, or this would break every ordinary approval prompt.
    #[test]
    fn sanitize_leaves_legitimate_text_untouched() {
        assert_eq!(
            sanitize_for_display("cargo test -p kaji-cli"),
            "cargo test -p kaji-cli"
        );
        assert_eq!(sanitize_for_display("鍛冶"), "鍛冶");
    }

    /// End-to-end: a hostile tool confirmation (tab + zero-width space in
    /// the params/prompt) must render with visible markers in the actual
    /// modal buffer — not just in the pure helper. This is what an approver
    /// would actually see on screen.
    #[test]
    fn approval_modal_renders_hostile_command_with_visible_markers() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = App::new(None);
        let hostile_prompt = "Exécuter `echo ok\t\u{200B}rm -rf /`\u{202E} ?".to_string();
        let msg = Message::assistant().with_action_required(
            "req-hostile".to_string(),
            "shell".to_string(),
            Default::default(),
            Some(hostile_prompt),
        );
        app.apply_agent_event(&AgentEvent::Message(msg));
        assert!(
            app.tool_approval.is_some(),
            "test setup: modal must be open"
        );

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("test backend terminal");
        terminal
            .draw(|f| draw(f, &app))
            .expect("draw must succeed against a TestBackend");
        let content = buffer_as_string(terminal.backend().buffer());

        assert!(
            content.contains('␉'),
            "rendered modal must reveal the tab, got:\n{content}"
        );
        assert!(
            content.contains('·'),
            "rendered modal must reveal the zero-width space, got:\n{content}"
        );
        assert!(
            content.contains("‹bidi›"),
            "rendered modal must reveal the bidi override, got:\n{content}"
        );
        assert!(
            !content.contains('\t'),
            "raw tab must never reach the rendered buffer"
        );
    }

    /// `Paragraph` silently clips content past its rendered height — proven
    /// by `approval_modal_renders_hostile_command_with_visible_markers`'s
    /// pre-fix buffer dump above (a hostile string's tail vanished with no
    /// on-screen indication). A long prompt must instead surface an
    /// explicit `… (+N car.)` marker so the approver knows text was cut,
    /// rather than unknowingly approving a truncated view of the command.
    #[test]
    fn approval_modal_marks_truncation_explicitly_instead_of_clipping_silently() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = App::new(None);
        let long_prompt = format!("rm -rf {}", "x".repeat(2000));
        let msg = Message::assistant().with_action_required(
            "req-long".to_string(),
            "shell".to_string(),
            Default::default(),
            Some(long_prompt),
        );
        app.apply_agent_event(&AgentEvent::Message(msg));
        assert!(
            app.tool_approval.is_some(),
            "test setup: modal must be open"
        );

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("test backend terminal");
        terminal
            .draw(|f| draw(f, &app))
            .expect("draw must succeed against a TestBackend");
        let content = buffer_as_string(terminal.backend().buffer());

        assert!(
            content.contains("car.)"),
            "an overflowing prompt must carry an explicit truncation marker, got:\n{content}"
        );
    }

    fn render_approval_at(
        tool_name: &str,
        arguments: rmcp::model::JsonObject,
        detail: bool,
        size: (u16, u16),
    ) -> String {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = App::new(None);
        let msg = Message::assistant().with_action_required(
            "req-render".to_string(),
            tool_name.to_string(),
            arguments,
            None,
        );
        app.apply_agent_event(&AgentEvent::Message(msg));
        assert!(
            app.tool_approval.is_some(),
            "test setup: modal must be open"
        );
        if detail {
            app.toggle_approval_detail();
        }

        let backend = TestBackend::new(size.0, size.1);
        let mut terminal = Terminal::new(backend).expect("test backend terminal");
        terminal
            .draw(|f| draw(f, &app))
            .expect("draw must succeed against a TestBackend");
        buffer_as_string(terminal.backend().buffer())
    }

    fn render_approval(
        tool_name: &str,
        arguments: rmcp::model::JsonObject,
        detail: bool,
    ) -> String {
        render_approval_at(tool_name, arguments, detail, (100, 30))
    }

    /// An approver pressing `s` or `a` writes a permission list entry — the
    /// modal has to show which one before the key is pressed, not after.
    #[test]
    fn approval_modal_offers_four_answers_and_names_the_grant_they_would_write() {
        let content = render_approval(
            "shell",
            rmcp::object!({ "command": "cargo test -p kaji-cli" }),
            false,
        );
        for expected in [
            "(y/s/a/n)",
            "y = une fois",
            "n / Esc = refuser",
            "Tab = détail",
            "s = pour la session",
            "a = toujours",
            "cargo test *",
        ] {
            assert!(
                content.contains(expected),
                "modal must show {expected:?}, got:\n{content}"
            );
        }
    }

    #[test]
    fn the_detail_panel_shows_the_whole_shell_command() {
        let content = render_approval(
            "shell",
            rmcp::object!({ "command": "rm -rf /tmp/kaji-scratch" }),
            true,
        );
        assert!(
            content.contains("rm -rf /tmp/kaji-scratch"),
            "got:\n{content}"
        );
        assert!(
            content.contains("Tab = masquer le détail"),
            "an open panel must advertise how to close it, got:\n{content}"
        );
    }

    #[test]
    fn the_detail_panel_renders_an_edit_as_signed_lines() {
        let content = render_approval(
            "developer__edit",
            rmcp::object!({ "path": "a.rs", "before": "let a = 1;", "after": "let a = 2;" }),
            true,
        );
        assert!(content.contains("-let a = 1;"), "got:\n{content}");
        assert!(content.contains("+let a = 2;"), "got:\n{content}");
    }

    /// The panel carries the same anti-masquage contract as the prompt above
    /// it: a hostile command must not be able to hide half of itself behind a
    /// control character, and an overlong one must say it was cut.
    #[test]
    fn the_detail_panel_sanitizes_and_marks_what_it_cannot_show() {
        let content = render_approval(
            "shell",
            rmcp::object!({ "command": "echo ok\t\u{200B}rm -rf /\u{202E}" }),
            true,
        );
        assert!(content.contains('␉'), "got:\n{content}");
        assert!(content.contains("‹bidi›"), "got:\n{content}");
        assert!(!content.contains('\t'), "got:\n{content}");

        let content = render_approval(
            "shell",
            rmcp::object!({ "command": format!("rm -rf {}", "x".repeat(4000)) }),
            true,
        );
        assert!(
            content.contains("car.)"),
            "a clipped detail must be marked, got:\n{content}"
        );
    }

    /// Splitting the modal in two leaves the detail pane with zero rows on a
    /// cramped terminal — it must degrade to showing nothing, never panic.
    #[test]
    fn the_detail_panel_survives_a_terminal_too_small_to_hold_it() {
        for size in [(12u16, 4u16), (20, 6), (40, 10)] {
            render_approval_at(
                "developer__edit",
                rmcp::object!({ "path": "a.rs", "before": "a", "after": "b" }),
                true,
                size,
            );
        }
    }

    /// The mode badge moved to the status bar with task 15 — the header must
    /// not carry it twice — and became a seal with the « hanko & forge » bar.
    #[test]
    fn the_status_bar_carries_the_current_mode_and_the_header_no_longer_does() {
        let mut app = App::new(None);
        app.kaji_mode = kaji::config::KajiMode::SmartApprove;

        let content = rendered(&app, 100, 10);
        let mut rows = content.lines();
        let header = rows.next().expect("ligne de header");
        let bar = rows.next_back().expect("barre d'état");

        assert!(bar.contains("智"), "got:\n{content}");
        assert!(!bar.contains("smart"), "got:\n{content}");
        assert!(!header.contains("智"), "got:\n{content}");
    }

    /// The telemetry lives on the bar alone since the « hanko & forge » task:
    /// the header keeps the session and the goal.
    #[test]
    fn the_telemetry_left_the_header_for_the_status_bar() {
        let mut app = App::new(None);
        app.header = "abcdef".to_string();
        app.model = "claude-fable-5".to_string();
        app.cost_total = Some(0.42);

        let content = rendered(&app, 120, 10);
        let mut rows = content.lines();
        let header = rows.next().expect("ligne de header");
        let bar = rows.next_back().expect("barre d'état");

        assert!(header.contains("abcdef"), "got:\n{content}");
        for gone in ["↑", "$", "claude-fable-5"] {
            assert!(!header.contains(gone), "{gone} : got:\n{content}");
        }
        assert!(bar.contains("claude-fable-5"), "got:\n{content}");
        assert!(bar.contains("$0.42"), "got:\n{content}");
    }

    #[test]
    fn the_status_bar_shows_the_working_directory_and_the_repository_state() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new(None);
        app.set_working_dir(dir.path().to_path_buf());
        app.git_status = Some(crate::tui::gitstatus::GitStatus {
            branch: "feat/kaji-init".to_string(),
            modified: 2,
            ..crate::tui::gitstatus::GitStatus::default()
        });

        let content = rendered(&app, 120, 10);
        let bar = content.lines().next_back().expect("barre d'état");

        assert!(bar.contains(theme::DIR_GLYPH), "got:\n{content}");
        assert!(bar.contains("feat/kaji-init"), "got:\n{content}");
        assert!(bar.contains("✚2"), "got:\n{content}");
        assert!(
            bar.contains(crate::tui::app::kaji_mode_seal(app.kaji_mode)),
            "got:\n{content}"
        );
        assert!(
            !content.lines().next().expect("header").contains('±'),
            "le header a rendu l'état git à la barre, got:\n{content}"
        );
    }

    #[test]
    fn the_header_carries_the_live_goal_but_drops_it_once_finished() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = App::new(None);
        app.goal_set("les tests passent", 10);

        let badge = goal_badge(&app).expect("un but actif a un bandeau");
        assert!(badge.contains("目標"), "{badge}");
        assert!(badge.contains("les tests passent"), "{badge}");
        assert!(badge.contains("it 1/10"), "{badge}");
        assert!(badge.contains("travail"), "{badge}");

        let backend = TestBackend::new(120, 10);
        let mut terminal = Terminal::new(backend).expect("test backend terminal");
        terminal
            .draw(|f| draw(f, &app))
            .expect("draw must succeed against a TestBackend");
        let content = buffer_as_string(terminal.backend().buffer());
        assert!(content.contains("les tests passent"), "got:\n{content}");

        app.goal_clear();
        assert!(
            goal_badge(&app).is_none(),
            "un but terminé libère le header"
        );
    }

    #[test]
    fn the_goal_badge_bounds_and_sanitizes_the_condition() {
        let mut app = App::new(None);
        app.goal_set(&format!("{}\n\u{1b}malicious", "x".repeat(60)), 10);

        let badge = goal_badge(&app).expect("un but actif a un bandeau");

        assert!(!badge.contains('\n'), "{badge}");
        assert!(!badge.contains('\u{1b}'), "{badge}");
        assert!(badge.chars().count() < 60, "{badge}");
    }

    fn rendered(app: &App, width: u16, height: u16) -> String {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("test backend terminal");
        terminal
            .draw(|frame| draw(frame, app))
            .expect("draw must succeed against a TestBackend");
        buffer_as_string(terminal.backend().buffer())
    }

    /// An App rooted on a tempdir holding one file, index already delivered.
    fn app_on_a_project(file: &str, content: &str) -> (App, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(file), content).unwrap();
        let mut app = App::new(None);
        app.set_working_dir(dir.path().to_path_buf());
        app.on_mention_index_ready(crate::tui::mentions::MentionIndex::build(
            dir.path().to_path_buf(),
        ));
        (app, dir)
    }

    #[test]
    fn the_finder_overlay_lists_matches_with_its_key_hints() {
        let (mut app, _dir) = app_on_a_project("README.md", "x");
        app.open_finder();

        let content = rendered(&app, 80, 20);

        assert!(content.contains("fichiers"), "got:\n{content}");
        assert!(content.contains("README.md"), "got:\n{content}");
        assert!(content.contains("Tab attacher @"), "got:\n{content}");
        assert!(content.contains("▸"), "marqueur de sélection visible");
    }

    #[test]
    fn the_theme_picker_lists_every_palette_and_marks_the_active_one() {
        let _theme = theme::test_guard();
        theme::set_active("nord").expect("nord is a built-in theme");
        let mut app = App::new(None);
        app.open_theme_picker();

        let content = rendered(&app, 80, 20);

        assert!(content.contains("thème"), "got:\n{content}");
        for palette in &theme::THEMES {
            assert!(
                content.contains(palette.name),
                "{} manquant, got:\n{content}",
                palette.name
            );
        }
        assert!(content.contains("nord (actuel)"), "got:\n{content}");
        assert!(content.contains("▸ nord"), "sélection sur l'actif");
        assert!(content.contains("Enter valider"), "got:\n{content}");
    }

    #[test]
    fn the_editor_picker_lists_what_was_detected_and_the_environment() {
        let _theme = theme::test_guard();
        let mut app = App::new(None);
        app.editors = crate::tui::editors::EditorState {
            detected: crate::tui::editors::EDITORS
                .iter()
                .filter(|spec| matches!(spec.id, "nvim" | "emacs"))
                .collect(),
            visual: Some("nvim".to_string()),
            ..Default::default()
        };
        app.open_editor_picker();

        let content = rendered(&app, 80, 20);

        assert!(content.contains("éditeur"), "got:\n{content}");
        assert!(content.contains("nvim"), "got:\n{content}");
        assert!(
            content.contains("▸ (env)"),
            "$VISUAL l'emporte sans KAJI_EDITOR, got:\n{content}"
        );
        assert!(
            content.contains("emacs  emacs -nw"),
            "la commande complète quand elle diffère de l'id, got:\n{content}"
        );
        assert!(content.contains("(env)"), "got:\n{content}");
        assert!(
            content.contains("$VISUAL = nvim (actuel)"),
            "got:\n{content}"
        );
        assert!(content.contains("Enter choisir"), "got:\n{content}");
    }

    #[test]
    fn the_viewer_takes_the_spec_slot_and_hands_it_back_on_close() {
        let (mut app, _dir) = app_on_a_project("a.rs", "fn main() {}\n");
        app.toggle_spec_panel();
        assert!(app.spec_panel_visible());
        assert!(rendered(&app, 100, 20).contains("SPEC"));

        app.open_viewer("a.rs");
        let content = rendered(&app, 100, 20);
        assert!(content.contains("a.rs"), "got:\n{content}");
        assert!(content.contains("fn main()"), "got:\n{content}");
        assert!(
            content.contains("j/k défiler · e éditer · r recharger"),
            "got:\n{content}"
        );
        assert!(
            !content.contains("SPEC"),
            "le lecteur occupe la colonne du volet SPEC, got:\n{content}"
        );

        app.close_viewer();
        assert!(
            rendered(&app, 100, 20).contains("SPEC"),
            "le volet SPEC revient à la fermeture"
        );
    }

    #[test]
    fn the_viewer_numbers_its_lines_and_scrolls_to_the_offset() {
        let file: String = (1..=60).map(|i| format!("ligne{i}\n")).collect();
        let (mut app, _dir) = app_on_a_project("n.txt", &file);
        app.open_viewer("n.txt");
        assert!(rendered(&app, 100, 20).contains(" 1 ligne1"));

        app.viewer.as_mut().unwrap().scroll = 40;
        let content = rendered(&app, 100, 20);
        assert!(content.contains("41 ligne41"), "got:\n{content}");
        assert!(
            !content.contains(" 1 ligne1"),
            "la première ligne est passée au-dessus, got:\n{content}"
        );
        assert!(content.contains("L41-"), "got:\n{content}");
    }

    #[test]
    fn viewer_width_keeps_a_floor_for_both_columns() {
        assert_eq!(viewer_width(120, false), 54);
        assert_eq!(viewer_width(60, false), 40, "plancher de 40 colonnes");
        assert_eq!(viewer_width(50, false), 30, "le chat garde ses 20 colonnes");
    }

    #[test]
    fn layout_widths_follow_the_agreed_table() {
        let cases = [
            (200u16, 40u16, 64u16, 96u16),
            (120, 26, 40, 54),
            (100, 24, 40, 36),
            (80, 0, 40, 40),
        ];
        for (total, expected_explorer, expected_viewer, expected_chat) in cases {
            let viewer = viewer_width(total, true);
            let explorer = explorer_width(total, viewer);
            let chat = total - explorer - viewer;
            assert_eq!(viewer, expected_viewer, "lecteur @ {total}");
            assert_eq!(explorer, expected_explorer, "explorateur @ {total}");
            assert_eq!(chat, expected_chat, "chat @ {total}");
        }

        assert_eq!(
            viewer_width(200, false),
            90,
            "45 % inchangé sans explorateur"
        );
    }

    #[test]
    fn chat_never_drops_below_its_floor_across_terminal_widths() {
        for total in 0..=400u16 {
            for explorer_open in [false, true] {
                let viewer = viewer_width(total, explorer_open);
                let explorer = if explorer_open {
                    explorer_width(total, viewer)
                } else {
                    0
                };
                let chat = total.saturating_sub(explorer).saturating_sub(viewer);
                assert!(
                    chat >= total.min(MIN_CHAT_WIDTH),
                    "chat={chat} plancher={} @ total={total}, explorer_open={explorer_open}",
                    total.min(MIN_CHAT_WIDTH)
                );
            }
        }
    }

    #[test]
    fn an_empty_file_renders_without_a_line_range_and_without_panicking() {
        let (mut app, _dir) = app_on_a_project("vide.txt", "");
        app.open_viewer("vide.txt");
        assert!(app.viewer.as_ref().unwrap().lines.is_empty());
        let content = rendered(&app, 100, 20);
        assert!(content.contains("L0-0/0"), "got:\n{content}");
    }

    #[test]
    fn explorer_width_gives_way_before_the_viewer_and_the_chat() {
        assert_eq!(explorer_width(120, 0), 26, "22 % sans lecteur");
        assert_eq!(
            explorer_width(120, viewer_width(120, true)),
            26,
            "22 % avec lecteur"
        );
        assert_eq!(explorer_width(200, 0), EXPLORER_MAX_WIDTH, "plafond de 40");
        assert_eq!(explorer_width(50, 0), EXPLORER_MIN_WIDTH, "plancher de 24");
        assert_eq!(
            explorer_width(80, viewer_width(80, true)),
            0,
            "l'explorateur cède en premier plutôt que d'écraser le chat"
        );
        assert_eq!(explorer_width(40, 0), 0);
    }

    #[test]
    fn the_explorer_takes_the_left_column_next_to_the_viewer() {
        let (mut app, dir) = app_on_a_project("README.md", "hello\n");
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        app.toggle_explorer();
        app.open_viewer("README.md");

        let content = rendered(&app, 120, 20);
        assert!(content.contains("▸ src"), "dossier replié, got:\n{content}");
        assert!(
            content.contains(". dotfiles · a attacher"),
            "got:\n{content}"
        );

        let row = content
            .lines()
            .find(|line| line.contains("▸ src"))
            .expect("ligne de l'arbre");
        let tree = row
            .chars()
            .position(|c| c == '▸')
            .expect("glyphe de dossier");
        let file = row
            .split_once("hello")
            .map(|(before, _)| before.chars().count())
            .expect("le lecteur reste à droite");
        assert!(
            tree < 36 && tree < file,
            "arbre à gauche ({tree}), lecteur à droite ({file}) : {row:?}"
        );
    }

    #[test]
    fn the_explorer_gives_way_rather_than_crush_the_chat_next_to_the_spec_panel() {
        let (mut app, dir) = app_on_a_project("README.md", "x");
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        app.toggle_spec_panel();
        app.toggle_explorer();

        let wide = rendered(&app, 120, 12);
        assert!(wide.contains("▸ src"), "got:\n{wide}");
        assert!(wide.contains("SPEC"), "got:\n{wide}");

        let narrow = rendered(&app, 50, 12);
        assert!(
            !narrow.contains("▸ src"),
            "l'explorateur cède en premier, got:\n{narrow}"
        );
        assert!(narrow.contains("SPEC"), "got:\n{narrow}");
    }

    /// The entry count belongs to the title so the status line stays short
    /// enough to survive the pane's own 48-column ceiling uncut.
    #[test]
    fn the_explorer_status_line_names_its_keys() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let (mut app, dir) = app_on_a_project("README.md", "x");
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        app.toggle_explorer();

        let backend = TestBackend::new(EXPLORER_MAX_WIDTH, 8);
        let mut terminal = Terminal::new(backend).expect("test backend terminal");
        terminal
            .draw(|frame| {
                draw_explorer(
                    frame,
                    &app,
                    app.explorer.as_ref().expect("explorateur ouvert"),
                    Rect::new(0, 0, EXPLORER_MAX_WIDTH, 8),
                )
            })
            .expect("draw_explorer must succeed against a TestBackend");
        let content = buffer_as_string(terminal.backend().buffer());

        let title = content.lines().next().expect("bordure haute");
        assert!(title.contains(theme::EXPLORER_GLYPH), "got:\n{content}");
        assert!(title.contains(" · 2 "), "compte au titre, got:\n{content}");

        let status = content.lines().next_back().expect("bordure basse");
        assert_eq!(
            status.trim_matches(|c: char| c == '└' || c == '┘' || c == '─' || c == ' '),
            ". dotfiles · a attacher · q fermer",
            "got:\n{content}"
        );
    }

    #[test]
    fn an_expanded_directory_marks_its_children_by_indentation() {
        let (mut app, dir) = app_on_a_project("README.md", "x");
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "x").unwrap();
        app.toggle_explorer();
        app.explorer.as_mut().unwrap().toggle_selected();

        let content = rendered(&app, 120, 20);
        assert!(content.contains("▾ src"), "got:\n{content}");
        assert!(
            content.contains("    main.rs"),
            "indentation 2 col, got:\n{content}"
        );
    }

    /// The chat block's own top-left corner, which is what tells a folded chat
    /// (task 21) from the `Ctrl+O chat` hint the folded reader's footer carries.
    fn chat_frame_row(content: &str) -> Option<&str> {
        content.lines().find(|line| line.contains("┌ chat"))
    }

    /// Column a needle sits at in a rendered row. Wide glyphs count as one
    /// column here rather than two, so distances are compared with slack.
    fn column_of(row: &str, needle: &str) -> usize {
        let (before, _) = row
            .split_once(needle)
            .unwrap_or_else(|| panic!("{needle} absent de {row:?}"));
        before.chars().count()
    }

    fn ctrl(code: KeyCode) -> Event {
        Event::Key(KeyEvent {
            code,
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        })
    }

    fn app_with_a_focused_viewer(explorer: bool) -> (App, tempfile::TempDir) {
        let (mut app, dir) = app_on_a_project("README.md", "hello\n");
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        if explorer {
            app.toggle_explorer();
        }
        app.open_viewer("README.md");
        assert_eq!(app.focus, Focus::Viewer);
        (app, dir)
    }

    #[test]
    fn the_three_columns_stand_while_the_composer_holds_the_focus() {
        let (mut app, _dir) = app_with_a_focused_viewer(true);
        app.focus = Focus::Composer;

        let content = rendered(&app, 200, 40);
        let row = chat_frame_row(&content)
            .unwrap_or_else(|| panic!("le chat garde sa colonne, got:\n{content}"));
        assert!(row.contains(theme::EXPLORER_GLYPH), "got:\n{content}");
        assert!(row.contains(theme::VIEWER_GLYPH), "got:\n{content}");
        assert!(
            column_of(row, theme::VIEWER_GLYPH) - column_of(row, "┌ chat") >= 90,
            "colonne de chat trop étroite : {row:?}"
        );
    }

    #[test]
    fn the_focused_viewer_folds_the_chat_and_keeps_the_composer() {
        let (app, _dir) = app_with_a_focused_viewer(false);

        let content = rendered(&app, 200, 40);
        assert!(
            chat_frame_row(&content).is_none(),
            "le chat est replié, got:\n{content}"
        );
        let row = content
            .lines()
            .find(|line| line.contains(theme::VIEWER_GLYPH))
            .expect("cadre du lecteur");
        assert!(
            column_of(row, "┐") >= 150,
            "le lecteur prend la place du chat : {row:?}"
        );
        assert!(content.contains("hello"), "got:\n{content}");
        assert!(content.contains("┌ message"), "composer, got:\n{content}");
        assert!(content.contains("Ctrl+O chat"), "got:\n{content}");
    }

    #[test]
    fn the_explorer_keeps_its_column_beside_a_folded_chat() {
        let (app, _dir) = app_with_a_focused_viewer(true);

        let content = rendered(&app, 200, 40);
        assert!(
            chat_frame_row(&content).is_none(),
            "le chat est replié, got:\n{content}"
        );
        let row = content
            .lines()
            .find(|line| line.contains(theme::VIEWER_GLYPH))
            .expect("cadre du lecteur");
        assert!(row.contains(theme::EXPLORER_GLYPH), "got:\n{content}");
        assert!(
            column_of(row, theme::EXPLORER_GLYPH) < column_of(row, theme::VIEWER_GLYPH),
            "arbre à gauche, lecteur à droite : {row:?}"
        );
        assert!(content.contains("▸ src"), "got:\n{content}");
    }

    #[test]
    fn ctrl_o_out_of_the_viewer_unfolds_the_chat() {
        let (mut app, _dir) = app_with_a_focused_viewer(false);
        assert!(chat_frame_row(&rendered(&app, 200, 40)).is_none());

        app.on_event(&ctrl(KeyCode::Char('o')));

        assert_eq!(app.focus, Focus::Composer);
        let content = rendered(&app, 200, 40);
        assert!(
            chat_frame_row(&content).is_some(),
            "le chat revient avec le focus, got:\n{content}"
        );
        assert!(content.contains(theme::VIEWER_GLYPH), "got:\n{content}");
    }

    #[test]
    fn attaching_from_the_viewer_unfolds_the_chat() {
        let (mut app, _dir) = app_with_a_focused_viewer(false);

        app.on_event(&key(KeyCode::Char('a')));

        assert_eq!(app.focus, Focus::Composer);
        assert!(app.input.contains("README.md"), "input : {:?}", app.input);
        let content = rendered(&app, 200, 40);
        assert!(
            chat_frame_row(&content).is_some(),
            "le chat revient avec le focus, got:\n{content}"
        );
    }

    #[test]
    fn the_folded_viewer_keeps_a_column_across_terminal_widths() {
        for total in 0..=400u16 {
            for explorer_open in [false, true] {
                let explorer = if explorer_open {
                    explorer_width(total, viewer_width(total, true))
                } else {
                    0
                };
                let folded = total.saturating_sub(explorer);
                assert!(
                    folded > 0 || total == 0,
                    "lecteur replié à 0 @ total={total}, explorateur={explorer}"
                );
                if total >= VIEWER_MIN_WIDTH + explorer {
                    assert!(
                        folded >= VIEWER_MIN_WIDTH,
                        "lecteur={folded} @ total={total}, explorateur={explorer}"
                    );
                }
            }
        }

        let (app, _dir) = app_with_a_focused_viewer(true);
        for width in [40u16, 60, 100, 200] {
            let content = rendered(&app, width, 12);
            assert!(
                content.contains(theme::VIEWER_GLYPH),
                "lecteur absent @ {width}, got:\n{content}"
            );
        }
    }
}
