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
pub fn render_markdown(input: &str, width: u16) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut in_code_block = false;
    let mut in_chart_block = false;
    let mut chart_buf: Vec<&str> = Vec::new();
    let mut table_buf: Vec<&str> = Vec::new();

    for raw_line in input.lines() {
        let trimmed = raw_line.trim_start();
        if trimmed.starts_with("```") {
            flush_table_buffer(&mut table_buf, &mut lines, width);
            if in_chart_block {
                let preceding_text = lines
                    .iter()
                    .rev()
                    .map(line_plain_text)
                    .find(|text| !text.trim().is_empty());
                lines.extend(render_chart_block(
                    &chart_buf,
                    width,
                    preceding_text.as_deref(),
                ));
                chart_buf.clear();
                in_chart_block = false;
            } else if in_code_block {
                in_code_block = false;
            } else {
                let tag = trimmed.trim_start_matches('`').trim();
                if tag == "kaji-chart" {
                    in_chart_block = true;
                } else {
                    in_code_block = true;
                }
            }
            continue;
        }
        if in_chart_block {
            chart_buf.push(raw_line);
            continue;
        }
        if in_code_block {
            lines.push(render_code_line(raw_line));
            continue;
        }
        if trimmed.starts_with('|') {
            table_buf.push(raw_line);
            continue;
        }
        flush_table_buffer(&mut table_buf, &mut lines, width);
        lines.push(render_line(raw_line));
    }
    if in_chart_block {
        lines.extend(chart_buf.iter().map(|line| render_code_line(line)));
    }
    flush_table_buffer(&mut table_buf, &mut lines, width);

    lines
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

fn flush_table_buffer(table_buf: &mut Vec<&str>, lines: &mut Vec<Line<'static>>, width: u16) {
    if table_buf.is_empty() {
        return;
    }
    match try_render_table(table_buf, width) {
        Some(table_lines) => lines.extend(table_lines),
        None => {
            for raw_line in table_buf.iter() {
                lines.push(render_line(raw_line));
            }
        }
    }
    table_buf.clear();
}

const TABLE_BUDGET_COLS: usize = 100;

/// Caps the table budget at the caller's actual measure, minus a 2-column
/// safety margin (rounding/edge slack so the box-drawing never lands exactly
/// on the wrap boundary). Never exceeds `TABLE_BUDGET_COLS` either, so wide
/// terminals still get the existing reading-width cap.
fn table_budget_for_width(width: u16) -> usize {
    TABLE_BUDGET_COLS.min(width.saturating_sub(2) as usize)
}

enum TableBorder {
    Top,
    Mid,
    Bottom,
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
fn try_render_table(lines: &[&str], width: u16) -> Option<Vec<Line<'static>>> {
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
    let col_widths = fit_table_to_budget(&natural_widths, table_budget_for_width(width))?;

    let mut out = Vec::with_capacity(4 + data_rows.len());
    out.push(table_border_line(&col_widths, TableBorder::Top));
    out.push(table_row_line(header, &col_widths, true));
    out.push(table_border_line(&col_widths, TableBorder::Mid));
    for row in data_rows {
        out.push(table_row_line(row, &col_widths, false));
    }
    out.push(table_border_line(&col_widths, TableBorder::Bottom));
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
    let overhead = 3 * num_cols + 1;
    if overhead + num_cols > total_budget {
        return None;
    }
    let available = total_budget - overhead;
    let natural_sum: usize = natural_widths.iter().sum();
    if natural_sum <= available {
        return Some(natural_widths.to_vec());
    }

    let mut widths: Vec<usize> = natural_widths
        .iter()
        .map(|&w| (w * available / natural_sum).max(1))
        .collect();
    while widths.iter().sum::<usize>() > available && widths.iter().any(|&w| w > 1) {
        if let Some((idx, _)) = widths.iter().enumerate().max_by_key(|&(_, &w)| w) {
            widths[idx] -= 1;
        }
    }
    Some(widths)
}

const TABLE_ELLIPSIS: &str = "…";

/// Width a cell occupies once its inline markup is consumed, so `**abc**`
/// claims 3 columns rather than 7. Terminal cells, not chars: an emoji or a
/// kanji claims 2 columns, and measuring it as 1 drifts every border after it.
fn table_cell_width(cell: &str) -> usize {
    render_inline_spans(cell)
        .iter()
        .map(|span| display_width(&span.content))
        .sum()
}

/// Fits a cell's inline spans to `width` terminal cells: pads with trailing
/// spaces, or truncates and appends `…`, which inherits the style of the span
/// it cuts through. A double-width char that would overrun the budget is
/// dropped and its cell padded, so the cell always occupies exactly `width`.
fn fit_table_cell_spans(spans: Vec<Span<'static>>, width: usize) -> Vec<Span<'static>> {
    let content_width: usize = spans.iter().map(|span| display_width(&span.content)).sum();
    if content_width <= width {
        let mut spans = spans;
        if content_width < width {
            spans.push(Span::styled(
                " ".repeat(width - content_width),
                theme::text(),
            ));
        }
        return spans;
    }
    let ellipsis_width = display_width(TABLE_ELLIPSIS);
    if width < ellipsis_width {
        return Vec::new();
    }

    let budget = width - ellipsis_width;
    let mut fitted = Vec::new();
    let mut used = 0;
    let mut ellipsis_style = theme::text();
    let mut buffer = [0u8; 4];
    for span in spans {
        let span_width = display_width(&span.content);
        if used + span_width <= budget {
            used += span_width;
            fitted.push(span);
            continue;
        }
        ellipsis_style = span.style;
        let mut head = String::new();
        for c in span.content.chars() {
            let cell = display_width(c.encode_utf8(&mut buffer));
            if used + cell > budget {
                break;
            }
            used += cell;
            head.push(c);
        }
        if !head.is_empty() {
            fitted.push(Span::styled(head, span.style));
        }
        break;
    }
    fitted.push(Span::styled(TABLE_ELLIPSIS, ellipsis_style));
    if used < budget {
        fitted.push(Span::styled(" ".repeat(budget - used), theme::text()));
    }
    fitted
}

/// Renders one table row. Cells go through the same inline parser as ordinary
/// lines, so `**bold**`, `*italic*` and `` `code` `` are styled instead of
/// printed as markers; header cells add `BOLD` on top of the inline style each
/// span already carries.
fn table_row_line(cells: &[String], col_widths: &[usize], header: bool) -> Line<'static> {
    let pad_style = if header {
        theme::text().add_modifier(Modifier::BOLD)
    } else {
        theme::text()
    };
    let mut spans = vec![Span::styled("│", theme::dim())];
    for (i, &width) in col_widths.iter().enumerate() {
        let content = cells.get(i).map(String::as_str).unwrap_or("");
        let cell = fit_table_cell_spans(render_inline_spans(content), width);
        spans.push(Span::styled(" ", pad_style));
        spans.extend(cell.into_iter().map(|mut span| {
            if header {
                span.style = span.style.add_modifier(Modifier::BOLD);
            }
            span
        }));
        spans.push(Span::styled(" ", pad_style));
        spans.push(Span::styled("│", theme::dim()));
    }
    Line::from(spans)
}

fn table_border_line(col_widths: &[usize], kind: TableBorder) -> Line<'static> {
    let (left, mid, right) = match kind {
        TableBorder::Top => ('┌', '┬', '┐'),
        TableBorder::Mid => ('├', '┼', '┤'),
        TableBorder::Bottom => ('└', '┴', '┘'),
    };
    let mut s = String::new();
    s.push(left);
    for (i, &width) in col_widths.iter().enumerate() {
        s.push_str(&"─".repeat(width + 2));
        if i + 1 < col_widths.len() {
            s.push(mid);
        }
    }
    s.push(right);
    Line::from(Span::styled(s, theme::dim()))
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

    /// Terminal column of every box-drawing junction on the line. Two rows of
    /// the same table must yield the same vector, otherwise the borders are
    /// ragged on screen.
    fn border_columns(line: &Line) -> Vec<usize> {
        let mut columns = Vec::new();
        let mut column = 0;
        let mut buffer = [0u8; 4];
        for c in plain_text(line).chars() {
            if "┌┬┐├┼┤└┴┘│".contains(c) {
                columns.push(column);
            }
            column += display_width(c.encode_utf8(&mut buffer));
        }
        columns
    }

    fn assert_borders_aligned(lines: &[Line<'static>]) {
        let expected = border_columns(&lines[0]);
        for line in lines {
            assert_eq!(
                border_columns(line),
                expected,
                "borders drift on row: {}",
                plain_text(line)
            );
        }
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
    fn renders_heading_bold_underlined() {
        let lines = render_markdown("# Titre principal", 100);
        assert_eq!(plain_text(&lines[0]), "Titre principal");
        let span = &lines[0].spans[0];
        assert!(span.style.add_modifier.contains(Modifier::BOLD));
        assert!(span.style.add_modifier.contains(Modifier::UNDERLINED));
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
    fn renders_pipe_table_as_box_drawing() {
        let lines = render_markdown("| a | bb |\n| - | -- |\n| c | dd |", 100);
        assert_eq!(lines.len(), 5);
        assert_eq!(plain_text(&lines[0]), "┌───┬────┐");
        assert_eq!(plain_text(&lines[1]), "│ a │ bb │");
        assert_eq!(plain_text(&lines[2]), "├───┼────┤");
        assert_eq!(plain_text(&lines[3]), "│ c │ dd │");
        assert_eq!(plain_text(&lines[4]), "└───┴────┘");
        let header = &lines[1].spans[1];
        assert!(header.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn truncates_wide_table_cells_to_total_budget() {
        let wide_cell = "z".repeat(200);
        let input = format!("| a | b |\n| - | - |\n| x | {wide_cell} |");
        let lines = render_markdown(&input, 102);
        assert_eq!(lines.len(), 5);
        for line in &lines {
            assert!(
                plain_text(line).chars().count() <= 100,
                "line exceeds 100-column budget: {}",
                plain_text(line)
            );
        }
        assert!(plain_text(&lines[3]).contains('…'));
    }

    #[test]
    fn table_cell_renders_bold_inline_markup() {
        let lines = render_markdown(
            "| jour | ciel |\n| - | - |\n| **Jeu 20 août** | pluie |",
            100,
        );
        let row = &lines[3];
        let text = plain_text(row);
        assert!(text.contains("Jeu 20 août"), "cell not rendered: {text}");
        assert!(
            !text.contains("**"),
            "bold markers leaked into the cell: {text}"
        );
        let bold = row
            .spans
            .iter()
            .find(|s| s.content == "Jeu 20 août")
            .expect("bold cell span");
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn table_column_widths_ignore_inline_markers() {
        let lines = render_markdown("| h | x |\n| - | - |\n| **abc** | y |", 100);
        assert_eq!(plain_text(&lines[0]), "┌─────┬───┐");
        assert_eq!(plain_text(&lines[1]), "│ h   │ x │");
        assert_eq!(plain_text(&lines[3]), "│ abc │ y │");
    }

    #[test]
    fn table_truncates_styled_cell_span_aware() {
        let long = "z".repeat(200);
        let input = format!("| a | b |\n| - | - |\n| x | **{long}** |");
        let lines = render_markdown(&input, 102);
        assert_eq!(lines.len(), 5);
        let row = plain_text(&lines[3]);
        assert_eq!(row.chars().count(), plain_text(&lines[0]).chars().count());
        assert!(row.chars().count() <= 100, "row exceeds budget: {row}");
        assert!(
            row.ends_with("… │"),
            "row should end with an ellipsis: {row}"
        );
        assert!(
            !row.contains('*'),
            "bold markers leaked into the cell: {row}"
        );
        let truncated = lines[3]
            .spans
            .iter()
            .find(|s| s.content.starts_with("zz"))
            .expect("truncated bold span");
        assert!(truncated.style.add_modifier.contains(Modifier::BOLD));
        let ellipsis = lines[3]
            .spans
            .iter()
            .find(|s| s.content == "…")
            .expect("ellipsis span");
        assert!(ellipsis.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn table_column_widths_count_emoji_as_terminal_cells() {
        assert_eq!(table_cell_width("🌧 Pluie"), display_width("🌧 Pluie"));
        assert_eq!(table_cell_width("⛅ Nuageux"), display_width("⛅ Nuageux"));

        let lines = render_markdown(
            "| Temps | Temp |\n| - | - |\n| ⛅ Nuageux | 28°C |\n| 🌧 Pluie | 21°C |\n| Sec | 30°C |",
            100,
        );
        assert_borders_aligned(&lines);
    }

    #[test]
    fn table_column_widths_count_cjk_as_terminal_cells() {
        assert_eq!(table_cell_width("鍛冶場"), 6);

        let lines = render_markdown("| lieu | note |\n| - | - |\n| 鍛冶場 | forge |", 100);
        assert_borders_aligned(&lines);
        assert!(plain_text(&lines[3]).contains("鍛冶場"));
    }

    #[test]
    fn table_truncates_wide_chars_within_the_cell_budget() {
        let wide_cell = "⛅".repeat(80);
        let input = format!("| a | b |\n| - | - |\n| x | {wide_cell} |");
        let lines = render_markdown(&input, 102);
        assert_eq!(lines.len(), 5);
        assert_borders_aligned(&lines);
        for line in &lines {
            let width = display_width(&plain_text(line));
            assert!(width <= 100, "line exceeds the 100-cell budget: {width}");
        }

        let row = plain_text(&lines[3]);
        let cells: Vec<&str> = row.split('│').collect();
        assert!(
            cells[2].trim_end().ends_with('…'),
            "truncated cell should end with an ellipsis: {row}"
        );
    }

    #[test]
    fn table_cell_renders_inline_code() {
        let _theme = theme::test_guard();
        let lines = render_markdown("| a | b |\n| - | - |\n| `cargo test` | y |", 100);
        let row = &lines[3];
        assert!(!plain_text(row).contains('`'));
        let code = row
            .spans
            .iter()
            .find(|s| s.content == "cargo test")
            .expect("code cell span");
        assert_eq!(code.style, theme::code_inline());
    }

    #[test]
    fn table_header_cell_keeps_bold_on_top_of_inline_style() {
        let _theme = theme::test_guard();
        let lines = render_markdown("| **x** | `y` |\n| - | - |\n| a | b |", 100);
        let header = &lines[1];
        assert_eq!(plain_text(header), "│ x │ y │");
        let bold = header
            .spans
            .iter()
            .find(|s| s.content == "x")
            .expect("bold header span");
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
        let code = header
            .spans
            .iter()
            .find(|s| s.content == "y")
            .expect("code header span");
        assert_eq!(code.style.bg, theme::code_inline().bg);
        assert_eq!(code.style.fg, theme::code_inline().fg);
        assert!(code.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn malformed_table_falls_back_to_raw_lines() {
        let input = "| a | b |\n| - | - |\n| c | d | e |";
        let lines = render_markdown(input, 100);
        let rendered: Vec<String> = lines.iter().map(plain_text).collect();
        assert_eq!(rendered, vec!["| a | b |", "| - | - |", "| c | d | e |"]);
    }

    #[test]
    fn table_inside_other_content_renders_between_paragraphs() {
        let lines = render_markdown("avant\n| a | b |\n| - | - |\n| c | d |\napres", 100);
        assert_eq!(lines.len(), 7);
        assert_eq!(plain_text(&lines[0]), "avant");
        assert!(plain_text(&lines[1]).starts_with('┌'));
        assert!(plain_text(&lines[5]).starts_with('└'));
        assert_eq!(plain_text(&lines[6]), "apres");
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
    fn table_too_wide_for_budget_falls_back_to_raw_lines() {
        let cols = 30;
        let make_row = |filler: &str| -> String {
            let cells = vec![filler; cols];
            format!("| {} |", cells.join(" | "))
        };
        let header = make_row("a");
        let separator = make_row("-");
        let input = format!("{header}\n{separator}");

        let lines = render_markdown(&input, 102);

        let rendered: Vec<String> = lines.iter().map(plain_text).collect();
        assert_eq!(rendered, vec![header, separator]);
        assert!(!rendered.iter().any(|l| l.starts_with('┌')));
    }

    #[test]
    fn narrow_width_shrinks_table_budget() {
        let col1 = "a".repeat(10);
        let col2 = "b".repeat(50);
        let header = format!("| {col1} | {col2} |");
        let separator = "| - | - |".to_string();
        let input = format!("{header}\n{separator}");

        let wide = render_markdown(&input, 100);
        let wide_text: Vec<String> = wide.iter().map(plain_text).collect();
        assert!(
            wide_text[0].starts_with('┌'),
            "should render as a table at width 100"
        );
        assert!(
            wide_text.iter().all(|l| !l.contains('…')),
            "natural widths should fit unscaled at width 100: {wide_text:?}"
        );

        let narrow = render_markdown(&input, 60);
        let narrow_text: Vec<String> = narrow.iter().map(plain_text).collect();
        if narrow_text[0].starts_with('┌') {
            for line in &narrow_text {
                assert!(
                    line.chars().count() <= 60,
                    "line exceeds 60-col budget at narrow width: {line}"
                );
            }
            assert!(
                narrow_text.iter().any(|l| l.contains('…')),
                "columns should be truncated once scaled down to fit width 60: {narrow_text:?}"
            );
        } else {
            assert_eq!(narrow_text, vec![header, separator]);
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
