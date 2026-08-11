use ratatui::style::{Color, Modifier, Style};

/// Encre sumi 墨 — texte courant.
pub const ENCRE: Color = Color::Rgb(200, 200, 195);
/// Indigo ai 藍 — préfixe utilisateur, bordures inactives.
pub const INDIGO: Color = Color::Rgb(84, 110, 140);
/// Vermillon shu 朱 — accent actif : étage en cours, spinner, alerte.
pub const VERMILLON: Color = Color::Rgb(203, 88, 65);
/// Or patiné — titres, glyphe 鍛冶, coches de complétion.
pub const OR_PATINE: Color = Color::Rgb(196, 164, 106);
/// Sumi profond — fond discret des puces de code inline.
pub const SUMI_PROFOND: Color = Color::Rgb(40, 40, 38);
/// Wakakusa 若草 — vert tendre, 4ᵉ teinte des graphiques en camembert (distincte
/// des trois accents déjà pris par vermillon/or/indigo).
pub const WAKAKUSA: Color = Color::Rgb(139, 166, 108);

pub const KAJI_GLYPH: &str = "鍛冶";
pub const USER_PREFIX: &str = "vous ▸ ";
pub const AGENT_PREFIX: &str = "鍛冶 ▸ ";
pub const SYSTEM_PREFIX: &str = "· ";
pub const THINKING_PREFIX: &str = "思 ";
pub const STEP_SYMBOL: &str = "◦";
pub const GATE_SYMBOL: &str = "⚔";
pub const SCROLL_INDICATOR: &str = "▼";

pub const SPINNER_FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
/// Loader zen (`思考中`) while a turn is in flight with nothing visible yet.
pub const ENSO_FRAMES: [char; 4] = ['◐', '◓', '◑', '◒'];

/// Ninja cursor (T4) — pulses at the tail of the agent's in-flight text line
/// while it streams. Vermillon, distinct from the dim ensō loader.
pub const BLADE_FRAMES: [char; 4] = ['▊', '▋', '▌', '▍'];

pub fn spinner_frame(elapsed: std::time::Duration) -> char {
    let idx = (elapsed.as_millis() / 100) as usize % SPINNER_FRAMES.len();
    SPINNER_FRAMES[idx]
}

/// Same shape as [`spinner_frame`] (one full cycle per second) but over the
/// 4-frame ensō set — driven by the event loop's existing 250 ms tick.
pub fn enso_frame(elapsed: std::time::Duration) -> char {
    let idx = (elapsed.as_millis() / 250) as usize % ENSO_FRAMES.len();
    ENSO_FRAMES[idx]
}

/// Same shape as [`enso_frame`] but over the blade set, on a faster ~600 ms
/// full cycle (150 ms/frame) — the pulse reads as more urgent than the
/// loader's slower breathing. Driven by the same redraw tick; a given draw
/// can land mid-frame since 150 ms doesn't divide the 250 ms tick evenly,
/// which is fine for a pulse (no frame is ever skipped over time).
pub fn blade_frame(elapsed: std::time::Duration) -> char {
    let idx = (elapsed.as_millis() / 150) as usize % BLADE_FRAMES.len();
    BLADE_FRAMES[idx]
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

/// Dim italic, distinct name from [`system`] so the two registers (system
/// notices vs. streamed model reasoning) can diverge visually later even
/// though they currently share the same style.
pub fn thinking() -> Style {
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
    Style::default().fg(VERMILLON).bg(SUMI_PROFOND)
}

pub fn code_block() -> Style {
    Style::default().fg(VERMILLON)
}

pub fn heading() -> Style {
    Style::default()
        .fg(OR_PATINE)
        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
}

/// En-tête de tableau aligné (`/cost`, `/docker`) — or patiné estompé.
pub fn table_header() -> Style {
    Style::default().fg(OR_PATINE).add_modifier(Modifier::DIM)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn enso_frame_cycles_through_four_frames_over_a_one_second_period() {
        assert_eq!(enso_frame(Duration::from_millis(0)), '◐');
        assert_eq!(enso_frame(Duration::from_millis(250)), '◓');
        assert_eq!(enso_frame(Duration::from_millis(500)), '◑');
        assert_eq!(enso_frame(Duration::from_millis(750)), '◒');
        assert_eq!(
            enso_frame(Duration::from_millis(1000)),
            '◐',
            "wraps back to the first frame after the 1s period"
        );
    }

    #[test]
    fn blade_frame_cycles_through_four_frames_over_a_600ms_period() {
        assert_eq!(blade_frame(Duration::from_millis(0)), '▊');
        assert_eq!(blade_frame(Duration::from_millis(150)), '▋');
        assert_eq!(blade_frame(Duration::from_millis(300)), '▌');
        assert_eq!(blade_frame(Duration::from_millis(450)), '▍');
        assert_eq!(
            blade_frame(Duration::from_millis(600)),
            '▊',
            "wraps back to the first frame after the ~600ms period"
        );
    }
}
