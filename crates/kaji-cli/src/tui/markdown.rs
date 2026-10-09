use crate::tui::gitstatus::display_width;
use crate::tui::theme;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use serde::Deserialize;

/// Renders a maison markdown subset (headings, bold, italic, inline code,
/// fenced code blocks, lists, blockquotes) into styled ratatui lines. No
/// external dependency: kept minimal on purpose, tuned for LLM chat output
/// rather than full CommonMark compliance.
///
/// `width` is the actual measure the caller will render into (e.g. the chat
/// pane's rect width) — table and chart budgets scale down from it so
/// box-drawing never wraps onto the next terminal row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct SourcePosition {
    pub line: usize,
    pub cell: usize,
    /// Unicode scalar offset in the styled source cell, before line folding.
    pub start: usize,
    pub end: usize,
}

#[derive(Default)]
pub(super) struct MarkdownLayout {
    pub lines: Vec<Line<'static>>,
    pub positions: Vec<Vec<SourcePosition>>,
}

impl MarkdownLayout {
    fn push(&mut self, line: Line<'static>, positions: Vec<SourcePosition>) {
        self.lines.push(line);
        self.positions.push(positions);
    }

    fn folded(&mut self, line: Line<'static>, width: u16, source: usize, cell: usize) {
        let mut offset = 0;
        for line in wrap_line(line, width) {
            let end = offset
                + line
                    .spans
                    .iter()
                    .map(|span| span.content.chars().count())
                    .sum::<usize>();
            self.push(
                line,
                vec![SourcePosition {
                    line: source,
                    cell,
                    start: offset,
                    end,
                }],
            );
            offset = end;
        }
    }

    fn extend(&mut self, other: Self) {
        self.lines.extend(other.lines);
        self.positions.extend(other.positions);
    }
}

pub fn render_markdown(input: &str, width: u16) -> Vec<Line<'static>> {
    render_markdown_layout(input, width).lines
}

pub(super) fn render_markdown_layout(input: &str, width: u16) -> MarkdownLayout {
    let mut out = MarkdownLayout::default();
    let mut in_code = false;
    let mut chart_start = 0;
    let mut chart: Option<Vec<&str>> = None;
    let mut table: Vec<(usize, &str)> = Vec::new();
    for (source, raw) in input.lines().enumerate() {
        let trimmed = raw.trim_start();
        if trimmed.starts_with("```") {
            flush_table_buffer(&mut table, &mut out, width);
            if let Some(chart_lines) = chart.take() {
                let previous = out
                    .lines
                    .iter()
                    .rev()
                    .map(line_plain_text)
                    .find(|text| !text.trim().is_empty());
                for (cell, line) in render_chart_block(&chart_lines, width, previous.as_deref())
                    .into_iter()
                    .enumerate()
                {
                    out.folded(line, width, chart_start, cell);
                }
            } else if in_code {
                in_code = false;
            } else if trimmed.trim_start_matches('`').trim() == "kaji-chart" {
                chart_start = source;
                chart = Some(Vec::new());
            } else {
                in_code = true;
            }
            continue;
        }
        if let Some(chart) = &mut chart {
            chart.push(raw);
            continue;
        }
        if in_code {
            out.folded(render_code_line(raw), width, source, 0);
            continue;
        }
        if trimmed.starts_with('|') {
            table.push((source, raw));
            continue;
        }
        flush_table_buffer(&mut table, &mut out, width);
        out.folded(render_line(raw), width.min(88), source, 0);
    }
    if let Some(chart) = chart {
        for (index, raw) in chart.into_iter().enumerate() {
            out.folded(render_code_line(raw), width, chart_start + index + 1, 0);
        }
    }
    flush_table_buffer(&mut table, &mut out, width);
    out
}

fn line_plain_text(line: &Line<'static>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

fn render_line(raw_line: &str) -> Line<'static> {
    if let Some(heading) = render_heading(raw_line) {
        return heading;
    }
    if let Some(quote) = render_blockquote(raw_line) {
        return quote;
    }
    if let Some(item) = render_list_item(raw_line) {
        return item;
    }
    Line::from(render_inline_spans(raw_line))
}

fn flush_table_buffer(table: &mut Vec<(usize, &str)>, out: &mut MarkdownLayout, width: u16) {
    if table.is_empty() {
        return;
    }
    let raw: Vec<&str> = table.iter().map(|(_, raw)| *raw).collect();
    match try_render_table(&raw, width, table[0].0) {
        Some(layout) => out.extend(layout),
        None => {
            for &(source, raw) in table.iter() {
                out.folded(render_line(raw), width.min(88), source, 0);
            }
        }
    }
    table.clear();
}

const TABLE_MIN_COLUMN: usize = 18;

fn table_budget_for_width(width: u16) -> usize {
    usize::from(width)
}

/// Detects and renders a markdown pipe table into box-drawing lines. The
/// second buffered line must be a separator row (`-`/`:` cells only); any
/// other shape (no separator, inconsistent column counts) — or a table that
/// can't fit within budget even at 1-char columns — returns `None` so the
/// caller falls back to rendering the raw buffered lines untouched —
/// arbitrary LLM output must never panic here. `width` scales the budget
/// down on narrow terminals (see `table_budget_for_width`), so a table too
/// wide for the actual chat measure falls back to raw instead of wrapping
/// its box-drawing onto the next row.
fn try_render_table(lines: &[&str], width: u16, source: usize) -> Option<MarkdownLayout> {
    if lines.len() < 2 {
        return None;
    }
    let rows: Vec<Vec<String>> = lines.iter().map(|line| split_table_row(line)).collect();
    if !is_separator_row(&rows[1]) {
        return None;
    }
    let num_cols = rows[0].len();
    if num_cols == 0 || rows.iter().any(|row| row.len() != num_cols) {
        return None;
    }

    let header = &rows[0];
    let data_rows = &rows[2..];

    let mut natural_widths = vec![1usize; num_cols];
    for row in std::iter::once(header).chain(data_rows.iter()) {
        for (i, cell) in row.iter().enumerate() {
            natural_widths[i] = natural_widths[i].max(table_cell_width(cell));
        }
    }
    let budget = table_budget_for_width(width);
    if (num_cols == 2
        && budget < 48
        && natural_widths.iter().any(|&width| width > TABLE_MIN_COLUMN))
        || budget
            < natural_widths
                .iter()
                .map(|width| (*width).min(TABLE_MIN_COLUMN))
                .sum::<usize>()
                + 3 * num_cols
                + 1
    {
        return Some(table_records(header, data_rows, width, source));
    }
    let measured: Vec<usize> = natural_widths
        .iter()
        .enumerate()
        .map(|(column, &width)| width.min(if column == 0 && num_cols == 2 { 28 } else { 96 }))
        .collect();
    let col_widths = fit_table_to_budget(&measured, budget)?;

    let mut out = MarkdownLayout::default();
    out.extend(table_row_lines(header, &col_widths, true, source));
    out.push(table_header_rule(&col_widths), Vec::new());
    for (index, row) in data_rows.iter().enumerate() {
        if index > 0 {
            out.push(Line::from(""), Vec::new());
        }
        out.extend(table_row_lines(row, &col_widths, false, source + index + 2));
    }
    Some(out)
}

fn split_table_row(line: &str) -> Vec<String> {
    let mut parts: Vec<&str> = line.trim().split('|').collect();
    if parts.first() == Some(&"") {
        parts.remove(0);
    }
    if parts.last() == Some(&"") {
        parts.pop();
    }
    parts.iter().map(|p| p.trim().to_string()).collect()
}

fn is_separator_row(cells: &[String]) -> bool {
    !cells.is_empty() && cells.iter().all(|cell| is_separator_cell(cell))
}

fn is_separator_cell(cell: &str) -> bool {
    !cell.is_empty() && cell.contains('-') && cell.chars().all(|c| c == '-' || c == ':')
}

/// Scales natural column widths down proportionally so the rendered table
/// (borders + 1-space padding per side) fits within `total_budget` columns.
/// Returns `None` when the table can't fit even at 1-char-wide columns
/// (borders + per-column floor alone exceed the budget) — the caller then
/// falls back to raw lines instead of emitting a table wider than the
/// reading budget. Otherwise never exceeds the budget; may undershoot by a
/// column or two on rounding.
fn fit_table_to_budget(natural_widths: &[usize], total_budget: usize) -> Option<Vec<usize>> {
    let num_cols = natural_widths.len();
    let overhead = 3 * num_cols.saturating_sub(1);
    if overhead + num_cols > total_budget {
        return None;
    }
    let available = total_budget - overhead;
    let natural_sum: usize = natural_widths.iter().sum();
    if natural_sum <= available {
        return Some(natural_widths.to_vec());
    }

    let floor = if total_budget >= 100 {
        24
    } else {
        TABLE_MIN_COLUMN
    };
    let floor = if natural_widths
        .iter()
        .map(|width| (*width).min(floor))
        .sum::<usize>()
        <= available
    {
        floor
    } else {
        TABLE_MIN_COLUMN
    };
    let mut widths: Vec<usize> = natural_widths
        .iter()
        .map(|&w| (w * available / natural_sum).max(w.min(floor)))
        .collect();
    while widths.iter().sum::<usize>() > available
        && widths
            .iter()
            .enumerate()
            .any(|(i, &w)| w > natural_widths[i].min(floor))
    {
        if let Some((idx, _)) = widths
            .iter()
            .enumerate()
            .filter(|&(i, &w)| w > natural_widths[i].min(floor))
            .max_by_key(|&(_, &w)| w)
        {
            widths[idx] -= 1;
        }
    }
    Some(widths)
}

fn table_cell_width(cell: &str) -> usize {
    render_inline_spans(cell)
        .iter()
        .map(|span| display_width(&span.content))
        .sum()
}

/// Fold styled graphemes at word boundaries, including long unbroken values.
/// Shared by prose and cells so measured rows are also the rows we paint.
pub(super) fn wrap_line(line: Line<'static>, width: u16) -> Vec<Line<'static>> {
    let width = usize::from(width.max(2));
    let glyphs: Vec<(&str, Style)> = line
        .spans
        .iter()
        .flat_map(|span| {
            span.styled_graphemes(line.style)
                .map(|glyph| (glyph.symbol, glyph.style))
        })
        .collect();
    if glyphs.is_empty() {
        return vec![line];
    }
    let mut result = Vec::new();
    let mut start = 0;
    while start < glyphs.len() {
        let mut end = start;
        let mut cells = 0;
        let mut word_break = None;
        while end < glyphs.len() {
            let next = display_width(glyphs[end].0);
            if cells + next > width {
                break;
            }
            cells += next;
            if glyphs[end].0.chars().all(char::is_whitespace) {
                word_break = Some(end + 1);
            }
            end += 1;
        }
        if end < glyphs.len() {
            if let Some(boundary) = word_break.filter(|&boundary| boundary > start) {
                end = boundary;
            }
        }
        end = end.max(start + 1);
        let mut spans: Vec<Span<'static>> = Vec::new();
        for (symbol, style) in &glyphs[start..end] {
            if let Some(last) = spans.last_mut().filter(|last| last.style == *style) {
                last.content.to_mut().push_str(symbol);
            } else {
                spans.push(Span::styled(symbol.to_string(), *style));
            }
        }
        result.push(Line::from(spans));
        start = end;
    }
    result
}

fn table_records(
    header: &[String],
    rows: &[Vec<String>],
    width: u16,
    source: usize,
) -> MarkdownLayout {
    let mut out = MarkdownLayout::default();
    if header.len() == 2 && !rows.is_empty() {
        let labels: Vec<String> = header
            .iter()
            .map(|label| {
                render_inline_spans(label)
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect();
        let schema = labels.join(" · ");
        let starts = [0, labels[0].chars().count() + 3];
        let mut offset = 0;
        for line in wrap_line(Line::from(Span::styled(schema, theme::dim())), width) {
            let end = offset
                + line
                    .spans
                    .iter()
                    .map(|span| span.content.chars().count())
                    .sum::<usize>();
            let positions = labels
                .iter()
                .enumerate()
                .filter_map(|(cell, label)| {
                    let start = offset.max(starts[cell]);
                    let stop = end.min(starts[cell] + label.chars().count());
                    (start < stop).then_some(SourcePosition {
                        line: source,
                        cell,
                        start: start.saturating_sub(starts[cell]),
                        end: stop.saturating_sub(starts[cell]),
                    })
                })
                .collect();
            out.push(line, positions);
            offset = end;
        }
        out.push(Line::from(""), Vec::new());
    }
    for (index, row) in rows.iter().enumerate() {
        if index > 0 {
            out.push(Line::from(""), Vec::new());
        }
        for (cell, (label, value)) in header.iter().zip(row).enumerate() {
            if header.len() != 2 {
                let line = Line::from(Span::styled(label.clone(), theme::dim()));
                if index == 0 {
                    out.folded(line, width, source, cell);
                } else {
                    for line in wrap_line(line, width) {
                        out.push(line, Vec::new());
                    }
                }
            }
            let mut spans = render_inline_spans(value);
            if header.len() == 2 && cell == 0 {
                for span in &mut spans {
                    span.style = span.style.add_modifier(Modifier::BOLD);
                }
            }
            out.folded(Line::from(spans), width, source + index + 2, cell);
        }
    }
    if rows.is_empty() {
        for (cell, label) in header.iter().enumerate() {
            out.folded(render_line(label), width, source, cell);
        }
    }
    out
}

fn table_row_lines(
    cells: &[String],
    col_widths: &[usize],
    header: bool,
    source: usize,
) -> MarkdownLayout {
    let folded: Vec<MarkdownLayout> = cells
        .iter()
        .zip(col_widths)
        .enumerate()
        .map(|(cell, (text, &width))| {
            let mut layout = MarkdownLayout::default();
            layout.folded(
                Line::from(render_inline_spans(text)),
                width as u16,
                source,
                cell,
            );
            layout
        })
        .collect();
    let height = folded
        .iter()
        .map(|layout| layout.lines.len())
        .max()
        .unwrap_or(1);
    let mut out = MarkdownLayout::default();
    for row in 0..height {
        let mut spans = Vec::new();
        let mut positions = Vec::new();
        for (column, &width) in col_widths.iter().enumerate() {
            let line = folded[column].lines.get(row);
            let used = line.map(|line| line.width()).unwrap_or(0);
            if let Some(line) = line {
                spans.extend(line.spans.iter().cloned().map(|mut span| {
                    if header {
                        span.style = span.style.add_modifier(Modifier::BOLD);
                    }
                    span
                }));
                positions.extend(folded[column].positions[row].iter().copied());
            }
            if column + 1 < col_widths.len() {
                spans.push(Span::raw(" ".repeat(width.saturating_sub(used) + 3)));
            }
        }
        out.push(Line::from(spans), positions);
    }
    out
}

fn table_header_rule(widths: &[usize]) -> Line<'static> {
    Line::from(Span::styled(
        widths
            .iter()
            .map(|&width| "─".repeat(width))
            .collect::<Vec<_>>()
            .join("   "),
        theme::border_inactive(),
    ))
}

fn render_code_line(raw_line: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled("│ ", theme::dim()),
        Span::styled(raw_line.to_string(), theme::code_block()),
    ])
}

const CHART_LABEL_CAP: usize = 24;
const CHART_BAR_MAX_WIDTH: usize = 40;
fn pie_colors() -> [Color; 4] {
    [
        theme::accent_color(),
        theme::gold_color(),
        theme::user_color(),
        theme::chart_alt_color(),
    ]
}

/// Reserved columns around the label+bar pair: pie's `"● "` bullet (2,
/// reserved even for bar charts to keep one rule for both), the `"  "` gap
/// after the label (2), the `"  "` gap after the bar (2), and the rendered
/// value/percentage (up to 10 chars — covers `"100 %"` and the largest
/// `format_chart_value` output, a 10-digit integer just under the 1e9
/// scientific-notation cutoff).
const CHART_CHROME_RESERVE: usize = 2 + 2 + 2 + 10;

/// Scales the bar's max width down from the caller's actual measure so
/// `label + chrome + bar` never exceeds it. Never exceeds `CHART_BAR_MAX_WIDTH`
/// either, so wide terminals keep today's bar length. Floors at 1 so a bar is
/// always drawn, even on a pathologically narrow terminal.
fn chart_bar_max_width(width: u16) -> usize {
    let available = (width as usize).saturating_sub(CHART_CHROME_RESERVE);
    CHART_BAR_MAX_WIDTH.min(available / 2).max(1)
}

/// Scales the label cap down from the same budget the bar already claimed,
/// so `label + chrome + bar` fits within `width`. Never exceeds
/// `CHART_LABEL_CAP`. Floors at 1 for the same reason as `chart_bar_max_width`.
fn chart_label_cap(width: u16, bar_max: usize) -> usize {
    let available = (width as usize).saturating_sub(CHART_CHROME_RESERVE);
    CHART_LABEL_CAP
        .min(available.saturating_sub(bar_max))
        .max(1)
}

#[derive(Deserialize)]
struct ChartSpec {
    #[serde(rename = "type")]
    kind: ChartKind,
    title: Option<String>,
    items: Vec<ChartItem>,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum ChartKind {
    Bar,
    Pie,
}

#[derive(Deserialize)]
struct ChartItem {
    label: String,
    value: f64,
}

/// Renders a `kaji-chart` fenced block: parses the buffered JSON body and
/// draws a bar or pie chart. Any failure (invalid JSON, empty items,
/// negative or non-finite values, zero total/max) falls back to the same
/// raw rendering as an ordinary fenced code block — arbitrary LLM output
/// must never panic here.
fn render_chart_block(
    body: &[&str],
    width: u16,
    preceding_text: Option<&str>,
) -> Vec<Line<'static>> {
    match parse_chart_spec(body) {
        Some(spec) => render_chart(&spec, width, preceding_text),
        None => body.iter().map(|line| render_code_line(line)).collect(),
    }
}

fn parse_chart_spec(body: &[&str]) -> Option<ChartSpec> {
    let json = body.join("\n");
    let spec: ChartSpec = serde_json::from_str(&json).ok()?;
    if spec.items.is_empty() {
        return None;
    }
    if spec
        .items
        .iter()
        .any(|item| !item.value.is_finite() || item.value < 0.0)
    {
        return None;
    }
    let total: f64 = spec.items.iter().map(|item| item.value).sum();
    if !total.is_finite() {
        return None;
    }
    match spec.kind {
        ChartKind::Bar => {
            let max = spec
                .items
                .iter()
                .fold(0.0_f64, |acc, item| acc.max(item.value));
            if max <= 0.0 {
                return None;
            }
        }
        ChartKind::Pie => {
            if total <= 0.0 {
                return None;
            }
        }
    }
    Some(spec)
}

/// Renders a parsed chart spec. `preceding_text` is the plain-text content of
/// the last non-empty line already emitted before this chart (typically a
/// markdown heading) — when the chart's own title matches it (trimmed,
/// case-insensitive), the model already wrote the title as a heading, so the
/// chart's title line is suppressed to avoid rendering the same text twice.
fn render_chart(spec: &ChartSpec, width: u16, preceding_text: Option<&str>) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    if let Some(title) = &spec.title {
        let duplicates_preceding_text = preceding_text
            .is_some_and(|text| text.trim().to_lowercase() == title.trim().to_lowercase());
        if !duplicates_preceding_text {
            out.push(Line::from(Span::styled(title.clone(), theme::title())));
        }
    }

    let bar_max = chart_bar_max_width(width);
    let label_cap = chart_label_cap(width, bar_max);
    let label_width = spec
        .items
        .iter()
        .map(|item| truncate_label(&item.label, label_cap).chars().count())
        .max()
        .unwrap_or(0)
        .min(label_cap);

    match spec.kind {
        ChartKind::Bar => {
            let max = spec
                .items
                .iter()
                .fold(0.0_f64, |acc, item| acc.max(item.value));
            for item in &spec.items {
                out.push(render_bar_line(item, max, label_width, label_cap, bar_max));
            }
        }
        ChartKind::Pie => {
            let total: f64 = spec.items.iter().map(|item| item.value).sum();
            for (i, item) in spec.items.iter().enumerate() {
                out.push(render_pie_line(
                    item,
                    total,
                    label_width,
                    label_cap,
                    bar_max,
                    i,
                ));
            }
        }
    }
    out
}

fn render_bar_line(
    item: &ChartItem,
    max: f64,
    label_width: usize,
    label_cap: usize,
    bar_max: usize,
) -> Line<'static> {
    let bar = "█".repeat(chart_bar_width(item.value, max, bar_max));
    Line::from(vec![
        Span::styled(
            pad_label(&item.label, label_width, label_cap),
            theme::text(),
        ),
        Span::raw("  "),
        Span::styled(bar, theme::accent()),
        Span::raw("  "),
        Span::styled(format_chart_value(item.value), theme::text()),
    ])
}

fn render_pie_line(
    item: &ChartItem,
    total: f64,
    label_width: usize,
    label_cap: usize,
    bar_max: usize,
    idx: usize,
) -> Line<'static> {
    let colors = pie_colors();
    let color = Style::default().fg(colors[idx % colors.len()]);
    let bar = "█".repeat(chart_bar_width(item.value, total, bar_max));
    let pct = (item.value / total * 100.0).round() as i64;
    Line::from(vec![
        Span::styled("●", color),
        Span::raw(" "),
        Span::styled(
            pad_label(&item.label, label_width, label_cap),
            theme::text(),
        ),
        Span::raw("  "),
        Span::styled(bar, color),
        Span::raw("  "),
        Span::styled(format!("{pct} %"), theme::text()),
    ])
}

fn chart_bar_width(value: f64, denom: f64, bar_max: usize) -> usize {
    if denom <= 0.0 || value <= 0.0 {
        return 0;
    }
    let bar_max = bar_max.max(1);
    let width = ((value / denom) * bar_max as f64).round() as usize;
    width.clamp(1, bar_max)
}

fn format_chart_value(value: f64) -> String {
    if value.abs() > 1e9 {
        return format!("{value:.1e}");
    }
    if value.fract() == 0.0 {
        return (value as i64).to_string();
    }
    let s = format!("{value:.2}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

fn truncate_label(label: &str, cap: usize) -> String {
    let chars: Vec<char> = label.chars().collect();
    if chars.len() <= cap {
        chars.into_iter().collect()
    } else if cap == 0 {
        String::new()
    } else {
        let truncated: String = chars[..cap - 1].iter().collect();
        format!("{truncated}…")
    }
}

fn pad_label(label: &str, width: usize, cap: usize) -> String {
    let truncated = truncate_label(label, cap);
    let len = truncated.chars().count();
    if len >= width {
        truncated
    } else {
        format!("{truncated}{}", " ".repeat(width - len))
    }
}

fn render_heading(line: &str) -> Option<Line<'static>> {
    let trimmed = line.trim_start();
    let hashes = trimmed.chars().take_while(|&c| c == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest: String = trimmed.chars().skip(hashes).collect();
    if !rest.starts_with(' ') {
        return None;
    }
    Some(Line::from(Span::styled(
        rest.trim_start().to_string(),
        theme::heading(),
    )))
}

fn render_blockquote(line: &str) -> Option<Line<'static>> {
    let trimmed = line.trim_start();
    let rest = trimmed.strip_prefix('>')?;
    let mut spans = vec![Span::styled("▎ ", theme::dim())];
    spans.extend(render_inline_spans(rest.trim_start()));
    Some(Line::from(spans))
}

fn render_list_item(line: &str) -> Option<Line<'static>> {
    let trimmed = line.trim_start();
    let indent = " ".repeat(line.len() - trimmed.len());

    if let Some(rest) = trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
        .or_else(|| trimmed.strip_prefix("+ "))
    {
        let mut spans = vec![Span::raw(format!("{indent}• "))];
        spans.extend(render_inline_spans(rest));
        return Some(Line::from(spans));
    }

    let (num, rest) = trimmed.split_once(". ")?;
    if num.is_empty() || !num.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let mut spans = vec![Span::raw(format!("{indent}{num}. "))];
    spans.extend(render_inline_spans(rest));
    Some(Line::from(spans))
}

fn flush(buf: &mut String, spans: &mut Vec<Span<'static>>) {
    if !buf.is_empty() {
        spans.push(Span::styled(std::mem::take(buf), theme::text()));
    }
}

/// Parses `**bold**`, `*italic*` and `` `code` `` spans out of a single
/// plain-text line. Unterminated delimiters fall back to literal text.
fn render_inline_spans(text: &str) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut buf = String::new();
    let mut chars = text.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '*' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut inner = String::new();
                let mut closed = false;
                while let Some(&nc) = chars.peek() {
                    if nc == '*' {
                        chars.next();
                        if chars.peek() == Some(&'*') {
                            chars.next();
                            closed = true;
                            break;
                        }
                        inner.push('*');
                    } else {
                        inner.push(nc);
                        chars.next();
                    }
                }
                if closed {
                    flush(&mut buf, &mut spans);
                    spans.push(Span::styled(
                        inner,
                        theme::text().add_modifier(Modifier::BOLD),
                    ));
                } else {
                    buf.push_str("**");
                    buf.push_str(&inner);
                }
            }
            '*' => {
                let mut inner = String::new();
                let mut closed = false;
                while let Some(&nc) = chars.peek() {
                    if nc == '*' {
                        chars.next();
                        closed = true;
                        break;
                    }
                    inner.push(nc);
                    chars.next();
                }
                if closed {
                    flush(&mut buf, &mut spans);
                    spans.push(Span::styled(
                        inner,
                        theme::text().add_modifier(Modifier::ITALIC),
                    ));
                } else {
                    buf.push('*');
                    buf.push_str(&inner);
                }
            }
            '`' => {
                let mut inner = String::new();
                let mut closed = false;
                while let Some(&nc) = chars.peek() {
                    if nc == '`' {
                        chars.next();
                        closed = true;
                        break;
                    }
                    inner.push(nc);
                    chars.next();
                }
                if closed {
                    flush(&mut buf, &mut spans);
                    spans.push(Span::styled(inner, theme::code_inline()));
                } else {
                    buf.push('`');
                    buf.push_str(&inner);
                }
            }
            _ => buf.push(c),
        }
    }
    flush(&mut buf, &mut spans);
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain_text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn renders_bold_text() {
        let lines = render_markdown("hello **world**", 100);
        assert_eq!(lines.len(), 1);
        assert_eq!(plain_text(&lines[0]), "hello world");
        let bold = lines[0]
            .spans
            .iter()
            .find(|s| s.content == "world")
            .expect("bold span");
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn renders_italic_text() {
        let lines = render_markdown("un mot *souligné* ici", 100);
        let italic = lines[0]
            .spans
            .iter()
            .find(|s| s.content == "souligné")
            .expect("italic span");
        assert!(italic.style.add_modifier.contains(Modifier::ITALIC));
    }

    #[test]
    fn renders_inline_code() {
        let _theme = theme::test_guard();
        let lines = render_markdown("lance `cargo test` maintenant", 100);
        let code = lines[0]
            .spans
            .iter()
            .find(|s| s.content == "cargo test")
            .expect("code span");
        assert_eq!(code.style, theme::code_inline());
        assert!(!code.style.add_modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn renders_fenced_code_block_without_the_fence_markers() {
        let lines = render_markdown("texte\n```\nlet x = 1;\nlet y = 2;\n```\nfin", 100);
        assert_eq!(lines.len(), 4);
        assert_eq!(plain_text(&lines[0]), "texte");
        assert!(plain_text(&lines[1]).contains("let x = 1;"));
        assert!(plain_text(&lines[2]).contains("let y = 2;"));
        assert_eq!(plain_text(&lines[3]), "fin");
        assert!(!lines.iter().any(|l| plain_text(l).contains("```")));
    }

    #[test]
    fn renders_bullet_and_ordered_list_items() {
        let lines = render_markdown("- premier\n* second\n1. troisième", 100);
        assert_eq!(lines.len(), 3);
        assert!(plain_text(&lines[0]).starts_with('•'));
        assert!(plain_text(&lines[0]).contains("premier"));
        assert!(plain_text(&lines[1]).starts_with('•'));
        assert!(plain_text(&lines[2]).starts_with("1."));
        assert!(plain_text(&lines[2]).contains("troisième"));
    }

    #[test]
    fn renders_blockquote_with_dim_marker() {
        let lines = render_markdown("> une citation", 100);
        assert!(plain_text(&lines[0]).contains('▎'));
        assert!(plain_text(&lines[0]).contains("une citation"));
    }

    #[test]
    fn renders_heading_bold_without_underline() {
        let lines = render_markdown("# Titre principal", 100);
        assert_eq!(plain_text(&lines[0]), "Titre principal");
        let span = &lines[0].spans[0];
        assert!(span.style.add_modifier.contains(Modifier::BOLD));
        assert!(!span.style.add_modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn renders_mixed_text_paragraph_untouched() {
        let lines = render_markdown("texte normal sans formatage", 100);
        assert_eq!(lines.len(), 1);
        assert_eq!(plain_text(&lines[0]), "texte normal sans formatage");
    }

    #[test]
    fn unterminated_delimiters_fall_back_to_literal_text() {
        let lines = render_markdown("texte **incomplet sans fermeture", 100);
        assert_eq!(plain_text(&lines[0]), "texte **incomplet sans fermeture");
    }

    #[test]
    fn table_has_a_quiet_header_separator_and_styled_cells() {
        let _theme = theme::test_guard();
        let lines = render_markdown("| a | bb |\n| - | -- |\n| **bold** | `code` |", 100);
        assert_eq!(lines.len(), 3);
        assert!(plain_text(&lines[1]).starts_with('─'));
        let bold = lines[2]
            .spans
            .iter()
            .find(|span| span.content == "bold")
            .unwrap();
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
        let code = lines[2]
            .spans
            .iter()
            .find(|span| span.content == "code")
            .unwrap();
        assert_eq!(code.style, theme::code_inline());
        assert!(lines.iter().all(|line| !plain_text(line).contains('│')));
    }

    #[test]
    fn responsive_cells_preserve_complete_text_styles_and_graphemes() {
        let _theme = theme::test_guard();
        let value = format!("{} FINAL_SENTINEL", "鍛冶 👩‍💻 café weather ".repeat(20));
        let label = "Navigation and verification with tools LABEL_SENTINEL";
        let input =
            format!("| Capability | What it means |\n| - | - |\n| **{label}** | `{value}` |");
        for width in [40, 80, 120, 200] {
            let layout = render_markdown_layout(&input, width);
            assert_eq!(layout.lines.len(), layout.positions.len());
            for line in &layout.lines {
                assert!(
                    line.width() <= usize::from(width),
                    "{width}: {}",
                    plain_text(line)
                );
            }
            for (cell, expected) in [(0, label), (1, value.as_str())] {
                let recovered: String = layout
                    .lines
                    .iter()
                    .zip(&layout.positions)
                    .filter_map(|(line, positions)| {
                        let _position = positions
                            .iter()
                            .find(|position| position.line == 2 && position.cell == cell)?;
                        Some(
                            line.spans
                                .iter()
                                .filter(|span| {
                                    if cell == 0 {
                                        span.style.add_modifier.contains(Modifier::BOLD)
                                    } else {
                                        span.style == theme::code_inline()
                                    }
                                })
                                .map(|span| span.content.as_ref())
                                .collect::<String>(),
                        )
                    })
                    .collect();
                assert_eq!(recovered, expected, "cell {cell} at width {width}");
            }
            assert!(!layout
                .lines
                .iter()
                .any(|line| plain_text(line).contains('…')));
        }
    }

    #[test]
    fn narrow_many_column_tables_become_complete_records() {
        let input = "| one | two | three |\n| - | - | - |\n| alpha FINAL_A | beta FINAL_B | gamma FINAL_C |";
        let lines = render_markdown(input, 40);
        let text = lines.iter().map(plain_text).collect::<Vec<_>>().join("\n");
        for marker in ["FINAL_A", "FINAL_B", "FINAL_C"] {
            assert!(text.contains(marker));
        }
        assert!(!text.contains('│'));
    }

    #[test]
    fn malformed_table_falls_back_to_raw_lines() {
        let input = "| a | b |\n| - | - |\n| c | d | e |";
        let lines = render_markdown(input, 100);
        let rendered: Vec<String> = lines.iter().map(plain_text).collect();
        assert_eq!(rendered, vec!["| a | b |", "| - | - |", "| c | d | e |"]);
    }

    #[test]
    fn prose_is_comfortable_while_tables_can_use_wide_panes() {
        let prose = "readable words ".repeat(40);
        let lines = render_markdown(&prose, 200);
        assert!(lines.iter().all(|line| line.width() <= 88));
        assert_eq!(lines.iter().map(plain_text).collect::<String>(), prose);
        let input = format!(
            "| heading | explanation |\n| - | - |\n| short | {} |",
            "content ".repeat(50)
        );
        let table = render_markdown(&input, 200);
        assert!(table.iter().any(|line| line.width() > 96));
    }

    #[test]
    fn renders_bar_chart_block() {
        let input = "```kaji-chart\n{\"type\":\"bar\",\"items\":[{\"label\":\"a\",\"value\":10},{\"label\":\"bb\",\"value\":20},{\"label\":\"ccc\",\"value\":5}]}\n```";
        let lines = render_markdown(input, 200);
        assert_eq!(lines.len(), 3);
        assert_eq!(
            plain_text(&lines[0]),
            format!("a    {}  10", "█".repeat(20))
        );
        assert_eq!(
            plain_text(&lines[1]),
            format!("bb   {}  20", "█".repeat(40))
        );
        assert_eq!(plain_text(&lines[2]), format!("ccc  {}  5", "█".repeat(10)));
    }

    #[test]
    fn renders_pie_chart_with_percentages() {
        let _theme = theme::test_guard();
        let input = "```kaji-chart\n{\"type\":\"pie\",\"items\":[{\"label\":\"x\",\"value\":1},{\"label\":\"y\",\"value\":1},{\"label\":\"z\",\"value\":3}]}\n```";
        let lines = render_markdown(input, 200);
        assert_eq!(lines.len(), 3);
        assert_eq!(
            plain_text(&lines[0]),
            format!("● x  {}  20 %", "█".repeat(8))
        );
        assert_eq!(
            plain_text(&lines[1]),
            format!("● y  {}  20 %", "█".repeat(8))
        );
        assert_eq!(
            plain_text(&lines[2]),
            format!("● z  {}  60 %", "█".repeat(24))
        );

        let dot0 = lines[0]
            .spans
            .iter()
            .find(|s| s.content == "●")
            .expect("pastille span");
        assert_eq!(dot0.style.fg, Some(theme::accent_color()));
        let dot1 = lines[1]
            .spans
            .iter()
            .find(|s| s.content == "●")
            .expect("pastille span");
        assert_eq!(dot1.style.fg, Some(theme::gold_color()));
    }

    #[test]
    fn chart_title_matching_preceding_heading_is_not_duplicated() {
        let input = "### Parts fictives\n\n```kaji-chart\n{\"type\":\"pie\",\"title\":\"Parts fictives\",\"items\":[{\"label\":\"x\",\"value\":1},{\"label\":\"y\",\"value\":1}]}\n```";
        let lines = render_markdown(input, 200);
        let occurrences = lines
            .iter()
            .filter(|line| plain_text(line) == "Parts fictives")
            .count();
        assert_eq!(
            occurrences,
            1,
            "expected exactly one 'Parts fictives' line, got {occurrences} in {:?}",
            lines.iter().map(plain_text).collect::<Vec<_>>()
        );
    }

    #[test]
    fn chart_title_differing_from_preceding_text_still_renders() {
        let input = "### Répartition\n\n```kaji-chart\n{\"type\":\"pie\",\"title\":\"Parts\",\"items\":[{\"label\":\"x\",\"value\":1},{\"label\":\"y\",\"value\":1}]}\n```";
        let lines = render_markdown(input, 200);
        assert!(lines.iter().any(|line| plain_text(line) == "Répartition"));
        assert!(lines.iter().any(|line| plain_text(line) == "Parts"));
    }

    #[test]
    fn chart_title_without_preceding_heading_renders() {
        let input = "```kaji-chart\n{\"type\":\"pie\",\"title\":\"Parts fictives\",\"items\":[{\"label\":\"x\",\"value\":1},{\"label\":\"y\",\"value\":1}]}\n```";
        let lines = render_markdown(input, 200);
        assert_eq!(plain_text(&lines[0]), "Parts fictives");
    }

    #[test]
    fn invalid_chart_json_falls_back_to_raw_block() {
        let input = "```kaji-chart\nnot json\n```";
        let lines = render_markdown(input, 100);
        assert_eq!(lines.len(), 1);
        assert_eq!(plain_text(&lines[0]), "│ not json");
    }

    #[test]
    fn empty_items_falls_back() {
        let input = "```kaji-chart\n{\"type\":\"bar\",\"items\":[]}\n```";
        let lines = render_markdown(input, 100);
        assert_eq!(lines.len(), 1);
        assert!(plain_text(&lines[0]).contains("\"items\":[]"));
    }

    #[test]
    fn negative_values_fall_back() {
        let input =
            "```kaji-chart\n{\"type\":\"bar\",\"items\":[{\"label\":\"a\",\"value\":-1}]}\n```";
        let lines = render_markdown(input, 100);
        assert_eq!(lines.len(), 1);
        assert!(plain_text(&lines[0]).contains("\"value\":-1"));
    }

    #[test]
    fn unterminated_chart_fence_falls_back_to_raw_lines() {
        let input = "```kaji-chart\n{\"type\":\"bar\",\n\"items\":[{\"label\":\"a\"";
        let lines = render_markdown(input, 100);
        assert_eq!(lines.len(), 2);
        assert_eq!(plain_text(&lines[0]), "│ {\"type\":\"bar\",");
        assert_eq!(plain_text(&lines[1]), "│ \"items\":[{\"label\":\"a\"");
    }

    #[test]
    fn giant_bar_value_falls_back_to_scientific_notation() {
        let input =
            "```kaji-chart\n{\"type\":\"bar\",\"items\":[{\"label\":\"a\",\"value\":1e308}]}\n```";
        let lines = render_markdown(input, 200);
        assert_eq!(lines.len(), 1);
        let text = plain_text(&lines[0]);
        let expected_value = format!("{:.1e}", 1e308_f64);
        assert_eq!(text, format!("a  {}  {expected_value}", "█".repeat(40)));
        assert!(
            text.chars().count() < 80,
            "line too long: {} chars",
            text.chars().count()
        );
        assert!(text.contains('e'));
    }

    #[test]
    fn pie_color_rotation_wraps_after_four_colors() {
        let _theme = theme::test_guard();
        let input = "```kaji-chart\n{\"type\":\"pie\",\"items\":[{\"label\":\"a\",\"value\":1},{\"label\":\"b\",\"value\":1},{\"label\":\"c\",\"value\":1},{\"label\":\"d\",\"value\":1},{\"label\":\"e\",\"value\":1}]}\n```";
        let lines = render_markdown(input, 200);
        assert_eq!(lines.len(), 5);
        let dot4 = lines[4]
            .spans
            .iter()
            .find(|s| s.content == "●")
            .expect("pastille span");
        assert_eq!(dot4.style.fg, Some(theme::accent_color()));
    }

    #[test]
    fn many_header_only_columns_preserve_labels_as_records() {
        let header = format!("| {} |", vec!["Heading"; 30].join(" | "));
        let separator = format!("| {} |", vec!["-"; 30].join(" | "));
        let lines = render_markdown(&format!("{header}\n{separator}"), 40);
        assert_eq!(
            lines
                .iter()
                .map(plain_text)
                .filter(|text| text == "Heading")
                .count(),
            30
        );
        assert!(lines.iter().all(|line| line.width() <= 40));
    }

    #[test]
    fn narrow_table_wraps_without_shortening_content() {
        let text = "b".repeat(100);
        let input = format!("| label | explanation |\n| - | - |\n| short | {text} |");
        for width in [40, 60, 100] {
            let lines = render_markdown(&input, width);
            assert!(lines.iter().all(|line| line.width() <= usize::from(width)));
            assert_eq!(
                lines
                    .iter()
                    .map(plain_text)
                    .collect::<String>()
                    .matches('b')
                    .count(),
                101
            );
        }
    }

    #[test]
    fn narrow_width_scales_chart_bars() {
        let input = "```kaji-chart\n{\"type\":\"bar\",\"items\":[{\"label\":\"a very long label indeed\",\"value\":10},{\"label\":\"bb\",\"value\":20},{\"label\":\"ccc\",\"value\":5}]}\n```";
        let lines = render_markdown(input, 50);
        for line in &lines {
            let len = plain_text(line).chars().count();
            assert!(len <= 50, "chart line exceeds width 50 ({len} chars)");
        }
    }
}
