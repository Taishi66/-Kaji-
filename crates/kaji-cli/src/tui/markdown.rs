use crate::tui::theme;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

/// Renders a maison markdown subset (headings, bold, italic, inline code,
/// fenced code blocks, lists, blockquotes) into styled ratatui lines. No
/// external dependency: kept minimal on purpose, tuned for LLM chat output
/// rather than full CommonMark compliance.
pub fn render_markdown(input: &str) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut in_code_block = false;
    let mut table_buf: Vec<&str> = Vec::new();

    for raw_line in input.lines() {
        if raw_line.trim_start().starts_with("```") {
            flush_table_buffer(&mut table_buf, &mut lines);
            in_code_block = !in_code_block;
            continue;
        }
        if in_code_block {
            lines.push(render_code_line(raw_line));
            continue;
        }
        if raw_line.trim_start().starts_with('|') {
            table_buf.push(raw_line);
            continue;
        }
        flush_table_buffer(&mut table_buf, &mut lines);
        lines.push(render_line(raw_line));
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
