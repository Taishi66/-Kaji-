use ratatui::style::{Color, Modifier, Style};

/// Encre sumi 墨 — texte courant.
pub const ENCRE: Color = Color::Rgb(200, 200, 195);
/// Indigo ai 藍 — préfixe utilisateur, bordures inactives.
pub const INDIGO: Color = Color::Rgb(84, 110, 140);
/// Vermillon shu 朱 — accent actif : étage en cours, spinner, alerte.
pub const VERMILLON: Color = Color::Rgb(203, 88, 65);
/// Or patiné — titres, glyphe 鍛冶, coches de complétion.
pub const OR_PATINE: Color = Color::Rgb(196, 164, 106);

pub const KAJI_GLYPH: &str = "鍛冶";
pub const USER_PREFIX: &str = "vous ▸ ";
pub const AGENT_PREFIX: &str = "鍛冶 ▸ ";
pub const SYSTEM_PREFIX: &str = "· ";
pub const STEP_SYMBOL: &str = "◦";
pub const GATE_SYMBOL: &str = "⚔";
pub const SCROLL_INDICATOR: &str = "▼";

pub const SPINNER_FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

pub fn spinner_frame(elapsed: std::time::Duration) -> char {
    let idx = (elapsed.as_millis() / 100) as usize % SPINNER_FRAMES.len();
    SPINNER_FRAMES[idx]
}

pub fn text() -> Style {
    Style::default().fg(ENCRE)
}

pub fn user() -> Style {
    Style::default().fg(INDIGO)
}

pub fn agent() -> Style {
    Style::default().fg(OR_PATINE).add_modifier(Modifier::BOLD)
}

pub fn system() -> Style {
    Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::ITALIC)
}

pub fn accent() -> Style {
    Style::default().fg(VERMILLON)
}

pub fn title() -> Style {
    Style::default().fg(OR_PATINE).add_modifier(Modifier::BOLD)
}

pub fn dim() -> Style {
    Style::default().fg(Color::DarkGray)
}

pub fn border_inactive() -> Style {
    Style::default().fg(INDIGO)
}

pub fn border_active() -> Style {
    Style::default().fg(VERMILLON)
}

pub fn code_inline() -> Style {
    Style::default().fg(ENCRE).add_modifier(Modifier::REVERSED)
}

pub fn code_block() -> Style {
    Style::default().fg(VERMILLON)
}

pub fn heading() -> Style {
    Style::default()
        .fg(OR_PATINE)
        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
}
