use crate::tui::theme;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use serde::Deserialize;

/// Renders a maison markdown subset (headings, bold, italic, inline code,
/// fenced code blocks, lists, blockquotes) into styled ratatui lines. No
/// external dependency: kept minimal on purpose, tuned for LLM chat output
/// rather than full CommonMark compliance.
pub fn render_markdown(input: &str) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut in_code_block = false;
    let mut in_chart_block = false;
    let mut chart_buf: Vec<&str> = Vec::new();
    let mut table_buf: Vec<&str> = Vec::new();

    for raw_line in input.lines() {
        let trimmed = raw_line.trim_start();
        if trimmed.starts_with("```") {
            flush_table_buffer(&mut table_buf, &mut lines);
            if in_chart_block {
                lines.extend(render_chart_block(&chart_buf));
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
        flush_table_buffer(&mut table_buf, &mut lines);
        lines.push(render_line(raw_line));
    }
    if in_chart_block {
        lines.extend(chart_buf.iter().map(|line| render_code_line(line)));
    }
    flush_table_buffer(&mut table_buf, &mut lines);

    lines
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

fn flush_table_buffer(table_buf: &mut Vec<&str>, lines: &mut Vec<Line<'static>>) {
    if table_buf.is_empty() {
        return;
    }
    match try_render_table(table_buf) {
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
/// arbitrary LLM output must never panic here.
fn try_render_table(lines: &[&str]) -> Option<Vec<Line<'static>>> {
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
            natural_widths[i] = natural_widths[i].max(cell.chars().count());
        }
    }
    let col_widths = fit_table_to_budget(&natural_widths, TABLE_BUDGET_COLS)?;

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

fn fit_table_cell(content: &str, width: usize) -> String {
    let chars: Vec<char> = content.chars().collect();
    if chars.len() <= width {
        let mut s: String = chars.into_iter().collect();
        s.push_str(&" ".repeat(width - s.chars().count()));
        s
    } else if width == 0 {
        String::new()
    } else {
        let truncated: String = chars[..width - 1].iter().collect();
        format!("{truncated}…")
    }
}

fn table_row_line(cells: &[String], col_widths: &[usize], header: bool) -> Line<'static> {
    let mut spans = vec![Span::styled("│", theme::dim())];
    for (i, &width) in col_widths.iter().enumerate() {
        let content = cells.get(i).map(String::as_str).unwrap_or("");
        let cell = fit_table_cell(content, width);
        let style = if header {
            theme::text().add_modifier(Modifier::BOLD)
        } else {
            theme::text()
        };
        spans.push(Span::styled(format!(" {cell} "), style));
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
const PIE_COLORS: [Color; 4] = [
    theme::VERMILLON,
    theme::OR_PATINE,
    theme::INDIGO,
    theme::ENCRE,
];

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
fn render_chart_block(body: &[&str]) -> Vec<Line<'static>> {
    match parse_chart_spec(body) {
        Some(spec) => render_chart(&spec),
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

fn render_chart(spec: &ChartSpec) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    if let Some(title) = &spec.title {
        out.push(Line::from(Span::styled(title.clone(), theme::title())));
    }

    let label_width = spec
        .items
        .iter()
        .map(|item| truncate_label(&item.label, CHART_LABEL_CAP).chars().count())
        .max()
        .unwrap_or(0)
        .min(CHART_LABEL_CAP);

    match spec.kind {
        ChartKind::Bar => {
            let max = spec
                .items
                .iter()
                .fold(0.0_f64, |acc, item| acc.max(item.value));
            for item in &spec.items {
                out.push(render_bar_line(item, max, label_width));
            }
        }
        ChartKind::Pie => {
            let total: f64 = spec.items.iter().map(|item| item.value).sum();
            for (i, item) in spec.items.iter().enumerate() {
                out.push(render_pie_line(item, total, label_width, i));
            }
        }
    }
    out
}

fn render_bar_line(item: &ChartItem, max: f64, label_width: usize) -> Line<'static> {
    let bar = "█".repeat(chart_bar_width(item.value, max));
    Line::from(vec![
        Span::styled(pad_label(&item.label, label_width), theme::text()),
        Span::raw("  "),
        Span::styled(bar, theme::accent()),
        Span::raw("  "),
        Span::styled(format_chart_value(item.value), theme::text()),
    ])
}

fn render_pie_line(item: &ChartItem, total: f64, label_width: usize, idx: usize) -> Line<'static> {
    let color = Style::default().fg(PIE_COLORS[idx % PIE_COLORS.len()]);
    let bar = "█".repeat(chart_bar_width(item.value, total));
    let pct = (item.value / total * 100.0).round() as i64;
    Line::from(vec![
        Span::styled("●", color),
        Span::raw(" "),
        Span::styled(pad_label(&item.label, label_width), theme::text()),
        Span::raw("  "),
        Span::styled(bar, color),
        Span::raw("  "),
        Span::styled(format!("{pct} %"), theme::text()),
    ])
}

fn chart_bar_width(value: f64, denom: f64) -> usize {
    if denom <= 0.0 || value <= 0.0 {
        return 0;
    }
    let width = ((value / denom) * CHART_BAR_MAX_WIDTH as f64).round() as usize;
    width.clamp(1, CHART_BAR_MAX_WIDTH)
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

fn pad_label(label: &str, width: usize) -> String {
    let truncated = truncate_label(label, CHART_LABEL_CAP);
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
        let lines = render_markdown("hello **world**");
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
        let lines = render_markdown("un mot *souligné* ici");
        let italic = lines[0]
            .spans
            .iter()
            .find(|s| s.content == "souligné")
            .expect("italic span");
        assert!(italic.style.add_modifier.contains(Modifier::ITALIC));
    }

    #[test]
    fn renders_inline_code() {
        let lines = render_markdown("lance `cargo test` maintenant");
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
        let lines = render_markdown("texte\n```\nlet x = 1;\nlet y = 2;\n```\nfin");
        assert_eq!(lines.len(), 4);
        assert_eq!(plain_text(&lines[0]), "texte");
        assert!(plain_text(&lines[1]).contains("let x = 1;"));
        assert!(plain_text(&lines[2]).contains("let y = 2;"));
        assert_eq!(plain_text(&lines[3]), "fin");
        assert!(!lines.iter().any(|l| plain_text(l).contains("```")));
    }

    #[test]
    fn renders_bullet_and_ordered_list_items() {
        let lines = render_markdown("- premier\n* second\n1. troisième");
        assert_eq!(lines.len(), 3);
        assert!(plain_text(&lines[0]).starts_with('•'));
        assert!(plain_text(&lines[0]).contains("premier"));
        assert!(plain_text(&lines[1]).starts_with('•'));
        assert!(plain_text(&lines[2]).starts_with("1."));
        assert!(plain_text(&lines[2]).contains("troisième"));
    }

    #[test]
    fn renders_blockquote_with_dim_marker() {
        let lines = render_markdown("> une citation");
        assert!(plain_text(&lines[0]).contains('▎'));
        assert!(plain_text(&lines[0]).contains("une citation"));
    }

    #[test]
    fn renders_heading_bold_underlined() {
        let lines = render_markdown("# Titre principal");
        assert_eq!(plain_text(&lines[0]), "Titre principal");
        let span = &lines[0].spans[0];
        assert!(span.style.add_modifier.contains(Modifier::BOLD));
        assert!(span.style.add_modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn renders_mixed_text_paragraph_untouched() {
        let lines = render_markdown("texte normal sans formatage");
        assert_eq!(lines.len(), 1);
        assert_eq!(plain_text(&lines[0]), "texte normal sans formatage");
    }

    #[test]
    fn unterminated_delimiters_fall_back_to_literal_text() {
        let lines = render_markdown("texte **incomplet sans fermeture");
        assert_eq!(plain_text(&lines[0]), "texte **incomplet sans fermeture");
    }

    #[test]
    fn renders_pipe_table_as_box_drawing() {
        let lines = render_markdown("| a | bb |\n| - | -- |\n| c | dd |");
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
        let lines = render_markdown(&input);
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
    fn malformed_table_falls_back_to_raw_lines() {
        let input = "| a | b |\n| - | - |\n| c | d | e |";
        let lines = render_markdown(input);
        let rendered: Vec<String> = lines.iter().map(plain_text).collect();
        assert_eq!(rendered, vec!["| a | b |", "| - | - |", "| c | d | e |"]);
    }

    #[test]
    fn table_inside_other_content_renders_between_paragraphs() {
        let lines = render_markdown("avant\n| a | b |\n| - | - |\n| c | d |\napres");
        assert_eq!(lines.len(), 7);
        assert_eq!(plain_text(&lines[0]), "avant");
        assert!(plain_text(&lines[1]).starts_with('┌'));
        assert!(plain_text(&lines[5]).starts_with('└'));
        assert_eq!(plain_text(&lines[6]), "apres");
    }

    #[test]
    fn renders_bar_chart_block() {
        let input = "```kaji-chart\n{\"type\":\"bar\",\"items\":[{\"label\":\"a\",\"value\":10},{\"label\":\"bb\",\"value\":20},{\"label\":\"ccc\",\"value\":5}]}\n```";
        let lines = render_markdown(input);
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
        let input = "```kaji-chart\n{\"type\":\"pie\",\"items\":[{\"label\":\"x\",\"value\":1},{\"label\":\"y\",\"value\":1},{\"label\":\"z\",\"value\":3}]}\n```";
        let lines = render_markdown(input);
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
        assert_eq!(dot0.style.fg, Some(theme::VERMILLON));
        let dot1 = lines[1]
            .spans
            .iter()
            .find(|s| s.content == "●")
            .expect("pastille span");
        assert_eq!(dot1.style.fg, Some(theme::OR_PATINE));
    }

    #[test]
    fn invalid_chart_json_falls_back_to_raw_block() {
        let input = "```kaji-chart\nnot json\n```";
        let lines = render_markdown(input);
        assert_eq!(lines.len(), 1);
        assert_eq!(plain_text(&lines[0]), "│ not json");
    }

    #[test]
    fn empty_items_falls_back() {
        let input = "```kaji-chart\n{\"type\":\"bar\",\"items\":[]}\n```";
        let lines = render_markdown(input);
        assert_eq!(lines.len(), 1);
        assert!(plain_text(&lines[0]).contains("\"items\":[]"));
    }

    #[test]
    fn negative_values_fall_back() {
        let input =
            "```kaji-chart\n{\"type\":\"bar\",\"items\":[{\"label\":\"a\",\"value\":-1}]}\n```";
        let lines = render_markdown(input);
        assert_eq!(lines.len(), 1);
        assert!(plain_text(&lines[0]).contains("\"value\":-1"));
    }

    #[test]
    fn unterminated_chart_fence_falls_back_to_raw_lines() {
        let input = "```kaji-chart\n{\"type\":\"bar\",\n\"items\":[{\"label\":\"a\"";
        let lines = render_markdown(input);
        assert_eq!(lines.len(), 2);
        assert_eq!(plain_text(&lines[0]), "│ {\"type\":\"bar\",");
        assert_eq!(plain_text(&lines[1]), "│ \"items\":[{\"label\":\"a\"");
    }

    #[test]
    fn giant_bar_value_falls_back_to_scientific_notation() {
        let input =
            "```kaji-chart\n{\"type\":\"bar\",\"items\":[{\"label\":\"a\",\"value\":1e308}]}\n```";
        let lines = render_markdown(input);
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
        let input = "```kaji-chart\n{\"type\":\"pie\",\"items\":[{\"label\":\"a\",\"value\":1},{\"label\":\"b\",\"value\":1},{\"label\":\"c\",\"value\":1},{\"label\":\"d\",\"value\":1},{\"label\":\"e\",\"value\":1}]}\n```";
        let lines = render_markdown(input);
        assert_eq!(lines.len(), 5);
        let dot4 = lines[4]
            .spans
            .iter()
            .find(|s| s.content == "●")
            .expect("pastille span");
        assert_eq!(dot4.style.fg, Some(theme::VERMILLON));
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

        let lines = render_markdown(&input);

        let rendered: Vec<String> = lines.iter().map(plain_text).collect();
        assert_eq!(rendered, vec![header, separator]);
        assert!(!rendered.iter().any(|l| l.starts_with('┌')));
    }
}
