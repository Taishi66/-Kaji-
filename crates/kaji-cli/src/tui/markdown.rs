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

    for raw_line in input.lines() {
        if raw_line.trim_start().starts_with("```") {
            in_code_block = !in_code_block;
            continue;
        }
        if in_code_block {
            lines.push(render_code_line(raw_line));
            continue;
        }
        if let Some(heading) = render_heading(raw_line) {
            lines.push(heading);
            continue;
        }
        if let Some(quote) = render_blockquote(raw_line) {
            lines.push(quote);
            continue;
        }
        if let Some(item) = render_list_item(raw_line) {
            lines.push(item);
            continue;
        }
        lines.push(Line::from(render_inline_spans(raw_line)));
    }

    lines
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
        assert!(code.style.add_modifier.contains(Modifier::REVERSED));
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
}
