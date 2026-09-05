//! HTML → texte lisible, sans dépendance.
//!
//! Un balayage unique : les balises de bloc deviennent des sauts de ligne, les
//! titres et les listes gardent leur marqueur markdown, les liens gardent leur
//! cible, et les contenus non textuels (script, style, svg) sont jetés. La
//! sortie est déterministe, ce qui compte pour un journal rejouable.

pub fn html_to_markdown(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let mut state = Renderer::default();
    for token in Tokenizer::new(html) {
        state.push(&mut out, token);
    }
    tidy(&out)
}

enum Token<'a> {
    Text(&'a str),
    Open { name: String, attributes: &'a str },
    Close(String),
}

struct Tokenizer<'a> {
    source: &'a str,
    cursor: usize,
}

impl<'a> Tokenizer<'a> {
    fn new(source: &'a str) -> Self {
        Self { source, cursor: 0 }
    }

    fn rest(&self) -> &'a str {
        self.source.get(self.cursor..).unwrap_or("")
    }
}

/// Les bornes viennent toutes de `find` ou de `char_indices`, donc elles
/// tombent sur des frontières de caractères ; `get` le dit au compilateur au
/// lieu de le supposer.
fn slice(text: &str, range: std::ops::Range<usize>) -> &str {
    text.get(range).unwrap_or("")
}

impl<'a> Iterator for Tokenizer<'a> {
    type Item = Token<'a>;

    fn next(&mut self) -> Option<Token<'a>> {
        loop {
            let rest = self.rest();
            if rest.is_empty() {
                return None;
            }

            if !rest.starts_with('<') {
                let end = rest.find('<').unwrap_or(rest.len());
                self.cursor += end;
                return Some(Token::Text(slice(rest, 0..end)));
            }

            if let Some(after) = rest.strip_prefix("<!--") {
                let end = after.find("-->").map(|i| 4 + i + 3).unwrap_or(rest.len());
                self.cursor += end;
                continue;
            }

            if rest.starts_with("<!") || rest.starts_with("<?") {
                let end = rest.find('>').map(|i| i + 1).unwrap_or(rest.len());
                self.cursor += end;
                continue;
            }

            let Some(end) = tag_end(rest) else {
                // Un `<` isolé : du texte, pas une balise.
                self.cursor += 1;
                return Some(Token::Text("<"));
            };

            let inner = slice(rest, 1..end);
            self.cursor += end + 1;

            let inner = inner.strip_suffix('/').unwrap_or(inner);
            if let Some(name) = inner.strip_prefix('/') {
                return Some(Token::Close(name.trim().to_ascii_lowercase()));
            }

            let split = inner
                .find(|c: char| c.is_ascii_whitespace())
                .unwrap_or(inner.len());
            return Some(Token::Open {
                name: slice(inner, 0..split).to_ascii_lowercase(),
                attributes: inner.get(split..).unwrap_or(""),
            });
        }
    }
}

/// La position du `>` qui ferme la balise, en ignorant ceux qui vivent dans une
/// valeur d'attribut entre guillemets.
fn tag_end(rest: &str) -> Option<usize> {
    let mut quote: Option<char> = None;
    for (index, c) in rest.char_indices().skip(1) {
        match (quote, c) {
            (Some(open), _) if c == open => quote = None,
            (Some(_), _) => {}
            (None, '"') | (None, '\'') => quote = Some(c),
            (None, '>') => return Some(index),
            (None, _) => {}
        }
    }
    None
}

const SKIPPED: &[&str] = &["script", "style", "noscript", "svg", "template", "iframe"];

const BLOCKS: &[&str] = &[
    "address",
    "article",
    "aside",
    "blockquote",
    "div",
    "dd",
    "dl",
    "dt",
    "figcaption",
    "figure",
    "footer",
    "form",
    "header",
    "main",
    "nav",
    "ol",
    "p",
    "pre",
    "section",
    "table",
    "tbody",
    "tfoot",
    "thead",
    "tr",
    "ul",
];

#[derive(Default)]
struct Renderer {
    skip_depth: usize,
    link: Option<String>,
}

impl Renderer {
    fn push(&mut self, out: &mut String, token: Token<'_>) {
        match token {
            Token::Open { name, attributes } => {
                if SKIPPED.contains(&name.as_str()) {
                    self.skip_depth += 1;
                    return;
                }
                if self.skip_depth > 0 {
                    return;
                }
                self.open(out, &name, attributes);
            }
            Token::Close(name) => {
                if SKIPPED.contains(&name.as_str()) {
                    self.skip_depth = self.skip_depth.saturating_sub(1);
                    return;
                }
                if self.skip_depth > 0 {
                    return;
                }
                self.close(out, &name);
            }
            Token::Text(text) => {
                if self.skip_depth == 0 {
                    push_text(out, text);
                }
            }
        }
    }

    fn open(&mut self, out: &mut String, name: &str, attributes: &str) {
        match name {
            "br" => out.push('\n'),
            "hr" => {
                break_line(out);
                out.push_str("---");
                break_line(out);
            }
            "li" => {
                break_line(out);
                out.push_str("- ");
            }
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                blank_line(out);
                let level = name
                    .strip_prefix('h')
                    .and_then(|digit| digit.parse::<usize>().ok())
                    .unwrap_or(1);
                out.push_str(&"#".repeat(level));
                out.push(' ');
            }
            "a" => {
                if let Some(href) = attribute(attributes, "href") {
                    out.push('[');
                    self.link = Some(href);
                }
            }
            "td" | "th" => {
                if !out.ends_with('\n') && !out.is_empty() {
                    out.push_str(" | ");
                }
            }
            _ if BLOCKS.contains(&name) => blank_line(out),
            _ => {}
        }
    }

    fn close(&mut self, out: &mut String, name: &str) {
        match name {
            "li" => break_line(out),
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => blank_line(out),
            "a" => {
                if let Some(href) = self.link.take() {
                    out.push_str("](");
                    out.push_str(&href);
                    out.push(')');
                }
            }
            _ if BLOCKS.contains(&name) => blank_line(out),
            _ => {}
        }
    }
}

fn break_line(out: &mut String) {
    while out.ends_with(' ') {
        out.pop();
    }
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
}

fn blank_line(out: &mut String) {
    break_line(out);
    if !out.is_empty() && !out.ends_with("\n\n") {
        out.push('\n');
    }
}

fn push_text(out: &mut String, text: &str) {
    for c in decoded(text) {
        push_char(out, c);
    }
}

/// Le texte entités résolues, caractère par caractère. Une entité inconnue est
/// rendue telle quelle plutôt que perdue.
fn decoded(text: &str) -> Vec<char> {
    let mut result = Vec::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '&' {
            let mut entity = String::new();
            let mut consumed = 0usize;
            let mut closed = false;
            for next in chars.clone().take(32) {
                consumed += 1;
                if next == ';' {
                    closed = true;
                    break;
                }
                entity.push(next);
            }
            if closed {
                if let Some(decoded) = decode_entity(&entity) {
                    for _ in 0..consumed {
                        chars.next();
                    }
                    result.push(decoded);
                    continue;
                }
            }
        }
        result.push(c);
    }
    result
}

/// La valeur d'un attribut, guillemets simples, doubles ou absents.
fn attribute(attributes: &str, wanted: &str) -> Option<String> {
    let chars: Vec<char> = attributes.chars().collect();
    let mut index = 0usize;

    while index < chars.len() {
        while index < chars.len() && chars[index].is_ascii_whitespace() {
            index += 1;
        }
        let start = index;
        while index < chars.len() && !chars[index].is_ascii_whitespace() && chars[index] != '=' {
            index += 1;
        }
        if start == index {
            return None;
        }
        let name: String = chars[start..index]
            .iter()
            .collect::<String>()
            .to_ascii_lowercase();

        while index < chars.len() && chars[index].is_ascii_whitespace() {
            index += 1;
        }
        let mut value = String::new();
        if index < chars.len() && chars[index] == '=' {
            index += 1;
            while index < chars.len() && chars[index].is_ascii_whitespace() {
                index += 1;
            }
            if index < chars.len() && (chars[index] == '"' || chars[index] == '\'') {
                let quote = chars[index];
                index += 1;
                while index < chars.len() && chars[index] != quote {
                    value.push(chars[index]);
                    index += 1;
                }
                index += 1;
            } else {
                while index < chars.len() && !chars[index].is_ascii_whitespace() {
                    value.push(chars[index]);
                    index += 1;
                }
            }
        }

        if name == wanted {
            return Some(decoded(&value).into_iter().collect());
        }
    }

    None
}

fn push_char(out: &mut String, c: char) {
    if c.is_whitespace() {
        if out.is_empty() || out.ends_with(' ') || out.ends_with('\n') {
            return;
        }
        out.push(' ');
        return;
    }
    out.push(c);
}

fn decode_entity(entity: &str) -> Option<char> {
    if let Some(number) = entity.strip_prefix('#') {
        let code = match number.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok()?,
            None => number.parse::<u32>().ok()?,
        };
        return char::from_u32(code);
    }
    match entity {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        // Ramené à une espace ordinaire : le texte servi au modèle n'a rien à
        // gagner à porter des espaces insécables.
        "nbsp" => Some(' '),
        "hellip" => Some('…'),
        "mdash" => Some('—'),
        "ndash" => Some('–'),
        "laquo" => Some('«'),
        "raquo" => Some('»'),
        "eacute" => Some('é'),
        "egrave" => Some('è'),
        "agrave" => Some('à'),
        "ccedil" => Some('ç'),
        "copy" => Some('©'),
        _ => None,
    }
}

fn tidy(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut blank_run = 0usize;
    for line in raw.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            blank_run += 1;
            if blank_run > 1 || out.is_empty() {
                continue;
            }
        } else {
            blank_run = 0;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attributes_with_angle_brackets_do_not_break_the_tokenizer() {
        let markdown = html_to_markdown(r#"<p title="a > b">texte</p>"#);
        assert_eq!(markdown, "texte");
    }

    #[test]
    fn a_lone_angle_bracket_stays_text() {
        assert_eq!(html_to_markdown("2 < 3"), "2 < 3");
    }

    #[test]
    fn an_unknown_entity_is_left_alone() {
        assert_eq!(html_to_markdown("<p>&zzz; fin</p>"), "&zzz; fin");
    }

    #[test]
    fn nested_headings_keep_their_level() {
        let markdown = html_to_markdown("<h3>Trois</h3><h1>Un</h1>");
        assert!(markdown.contains("### Trois"));
        assert!(markdown.contains("# Un"));
    }

    #[test]
    fn a_link_without_href_keeps_its_text() {
        assert_eq!(html_to_markdown("<a>texte</a>"), "texte");
    }
}
