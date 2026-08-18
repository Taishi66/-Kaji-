use anyhow::{Result, bail};
use ratatui::style::{Color, Modifier, Style};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Rôles sémantiques d'un thème — les fonctions de style ci-dessous ne
/// connaissent que ces rôles, jamais une couleur littérale.
#[derive(Debug)]
pub struct Palette {
    pub name: &'static str,
    /// Texte courant.
    pub text: Color,
    /// Lignes système, raisonnement, ambiance de démarrage.
    pub muted: Color,
    /// Préfixe utilisateur.
    pub user: Color,
    /// Titres, agent, en-têtes de tableau.
    pub gold: Color,
    /// Accent actif : étage en cours, spinner, curseur, alerte.
    pub accent: Color,
    pub border_inactive: Color,
    /// Fond des puces de code inline.
    pub code_bg: Color,
    /// 4ᵉ teinte des graphiques, distincte des trois accents ci-dessus.
    pub chart_alt: Color,
}

/// Ordre du cycle `/theme` ; `zen` (index 0) est le thème par défaut et
/// reprend à l'identique les couleurs historiques (encre sumi 墨, indigo ai
/// 藍, vermillon shu 朱, or patiné, sumi profond, wakakusa 若草).
pub static THEMES: [Palette; 6] = [
    Palette {
        name: "zen",
        text: Color::Rgb(200, 200, 195),
        muted: Color::DarkGray,
        user: Color::Rgb(84, 110, 140),
        gold: Color::Rgb(196, 164, 106),
        accent: Color::Rgb(203, 88, 65),
        border_inactive: Color::Rgb(84, 110, 140),
        code_bg: Color::Rgb(40, 40, 38),
        chart_alt: Color::Rgb(139, 166, 108),
    },
    Palette {
        name: "light",
        text: Color::Rgb(56, 56, 56),
        muted: Color::Rgb(120, 118, 111),
        user: Color::Rgb(59, 91, 130),
        gold: Color::Rgb(138, 109, 42),
        accent: Color::Rgb(180, 50, 31),
        border_inactive: Color::Rgb(140, 158, 182),
        code_bg: Color::Rgb(236, 234, 228),
        chart_alt: Color::Rgb(94, 127, 58),
    },
    Palette {
        name: "nord",
        text: Color::Rgb(216, 222, 233),
        muted: Color::Rgb(76, 86, 106),
        user: Color::Rgb(136, 192, 208),
        gold: Color::Rgb(235, 203, 139),
        accent: Color::Rgb(191, 97, 106),
        border_inactive: Color::Rgb(94, 129, 172),
        code_bg: Color::Rgb(59, 66, 82),
        chart_alt: Color::Rgb(163, 190, 140),
    },
    Palette {
        name: "gruvbox",
        text: Color::Rgb(235, 219, 178),
        muted: Color::Rgb(146, 131, 116),
        user: Color::Rgb(131, 165, 152),
        gold: Color::Rgb(250, 189, 47),
        accent: Color::Rgb(251, 73, 52),
        border_inactive: Color::Rgb(102, 92, 84),
        code_bg: Color::Rgb(60, 56, 52),
        chart_alt: Color::Rgb(184, 187, 38),
    },
    Palette {
        name: "solarized",
        text: Color::Rgb(131, 148, 150),
        muted: Color::Rgb(101, 123, 131),
        user: Color::Rgb(38, 139, 210),
        gold: Color::Rgb(181, 137, 0),
        accent: Color::Rgb(220, 50, 47),
        border_inactive: Color::Rgb(88, 110, 117),
        code_bg: Color::Rgb(7, 54, 66),
        chart_alt: Color::Rgb(133, 153, 0),
    },
    Palette {
        name: "mono",
        text: Color::Reset,
        muted: Color::DarkGray,
        user: Color::Gray,
        gold: Color::White,
        accent: Color::White,
        border_inactive: Color::DarkGray,
        code_bg: Color::DarkGray,
        chart_alt: Color::Gray,
    },
];

static ACTIVE: AtomicUsize = AtomicUsize::new(0);

pub fn active() -> &'static Palette {
    &THEMES[active_index()]
}

pub fn active_index() -> usize {
    ACTIVE.load(Ordering::Relaxed)
}

/// Pendant infaillible de [`set_active`] pour les appelants qui tiennent déjà
/// un rang dans [`THEMES`] — le sélecteur `/theme`, dont l'aperçu ne peut pas
/// échouer. L'index boucle, comme [`next_name`].
pub fn set_active_index(index: usize) {
    ACTIVE.store(index % THEMES.len(), Ordering::Relaxed);
}

fn index_of(name: &str) -> Option<usize> {
    let name = name.trim();
    THEMES
        .iter()
        .position(|p| p.name.eq_ignore_ascii_case(name))
}

pub fn set_active(name: &str) -> Result<()> {
    let Some(idx) = index_of(name) else {
        let available: Vec<&str> = THEMES.iter().map(|p| p.name).collect();
        bail!(
            "thème inconnu « {} » — disponibles : {}",
            name.trim(),
            available.join(", ")
        );
    };
    ACTIVE.store(idx, Ordering::Relaxed);
    Ok(())
}

/// Thème suivant dans l'ordre de [`THEMES`], en boucle. Un nom inconnu
/// repart du premier thème.
pub fn next_name(current: &str) -> &'static str {
    let idx = index_of(current).map_or(0, |i| (i + 1) % THEMES.len());
    THEMES[idx].name
}

/// Résolution au démarrage : env `KAJI_THEME` > config `KAJI_THEME` > `zen`.
/// Une valeur inconnue retombe sur `zen` plutôt que d'échouer.
pub fn resolve_theme(env: Option<&str>, config: Option<&str>) -> &'static str {
    env.or(config)
        .and_then(index_of)
        .map_or(THEMES[0].name, |idx| THEMES[idx].name)
}

pub fn text_color() -> Color {
    active().text
}

pub fn user_color() -> Color {
    active().user
}

pub fn gold_color() -> Color {
    active().gold
}

pub fn accent_color() -> Color {
    active().accent
}

pub fn chart_alt_color() -> Color {
    active().chart_alt
}

/// La palette active est un état de processus : tout test qui la change (ou
/// qui affirme une couleur exacte) prend ce verrou, restauré à la sortie.
#[cfg(test)]
pub struct ThemeGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
    previous: usize,
}

#[cfg(test)]
impl Drop for ThemeGuard {
    fn drop(&mut self) {
        ACTIVE.store(self.previous, Ordering::Relaxed);
    }
}

#[cfg(test)]
pub fn test_guard() -> ThemeGuard {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let lock = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    ThemeGuard {
        _lock: lock,
        previous: ACTIVE.load(Ordering::Relaxed),
    }
}

pub const KAJI_GLYPH: &str = "鍛冶";
pub const USER_PREFIX: &str = "vous ▸ ";
pub const AGENT_PREFIX: &str = "鍛冶 ▸ ";
pub const SYSTEM_PREFIX: &str = "· ";
pub const THINKING_PREFIX: &str = "思 ";
pub const STEP_SYMBOL: &str = "◦";
pub const SCROLL_INDICATOR: &str = "▼";

/// Glyphes de volet et d'état, en kanji plutôt qu'en emoji. Chacun occupe
/// deux cellules comme l'emoji qu'il remplace, donc les budgets de largeur
/// des titres et de la barre d'état restent justes.
pub const EXPLORER_GLYPH: &str = "樹";
pub const VIEWER_GLYPH: &str = "巻";
pub const DIR_GLYPH: &str = "在";
pub const STEER_GLYPH: &str = "列";
pub const TOOL_GLYPH: &str = "工";
pub const ELAPSED_GLYPH: &str = "刻";
pub const GATE_GLYPH: &str = "門";

pub const SPINNER_FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
/// Loader zen (`思考中`) while a turn is in flight with nothing visible yet.
pub const ENSO_FRAMES: [char; 4] = ['◐', '◓', '◑', '◒'];

/// Ninja cursor (T4) — pulses at the tail of the agent's in-flight text line
/// while it streams. Accent-coloured, distinct from the dim ensō loader.
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
    Style::default().fg(active().text)
}

pub fn user() -> Style {
    Style::default().fg(active().user)
}

pub fn agent() -> Style {
    Style::default()
        .fg(active().gold)
        .add_modifier(Modifier::BOLD)
}

pub fn system() -> Style {
    Style::default()
        .fg(active().muted)
        .add_modifier(Modifier::ITALIC)
}

/// Dim italic, distinct name from [`system`] so the two registers (system
/// notices vs. streamed model reasoning) can diverge visually later even
/// though they currently share the same style.
pub fn thinking() -> Style {
    Style::default()
        .fg(active().muted)
        .add_modifier(Modifier::ITALIC)
}

pub fn accent() -> Style {
    Style::default().fg(active().accent)
}

/// Erreurs provider/LLM — le rôle `accent` porte aussi l'alerte ; nom
/// distinct de [`accent`] pour que le registre d'erreur puisse diverger
/// sans toucher aux appelants « accent actif ».
pub fn error() -> Style {
    Style::default().fg(active().accent)
}

pub fn title() -> Style {
    Style::default()
        .fg(active().gold)
        .add_modifier(Modifier::BOLD)
}

pub fn dim() -> Style {
    Style::default().fg(active().muted)
}

pub fn border_inactive() -> Style {
    Style::default().fg(active().border_inactive)
}

pub fn border_active() -> Style {
    Style::default().fg(active().accent)
}

pub fn code_inline() -> Style {
    let palette = active();
    Style::default().fg(palette.accent).bg(palette.code_bg)
}

pub fn code_block() -> Style {
    Style::default().fg(active().accent)
}

pub fn heading() -> Style {
    Style::default()
        .fg(active().gold)
        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
}

/// En-tête de tableau aligné (`/cost`, `/docker`) — teinte or estompée.
pub fn table_header() -> Style {
    Style::default()
        .fg(active().gold)
        .add_modifier(Modifier::DIM)
}

/// Or nu, sans le gras de [`title`] ni l'atténuation de [`table_header`] —
/// jauges budget et contexte sous leur seuil d'alerte.
pub fn gold() -> Style {
    Style::default().fg(active().gold)
}

/// Rôle sémantique d'un span de bloc pré-rendu (`/cost`, `/context`,
/// `/docker`, `/checkpoints`, bannière, erreurs) : ce que la ligne de chat
/// stocke à la place d'un `Style`, pour que la couleur soit résolue au draw et
/// qu'un `/theme` en session re-colore les blocs déjà poussés.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SpanRole {
    /// Aucun style — espaces de marge et lignes vides.
    Plain,
    Text,
    Dim,
    System,
    Accent,
    Error,
    Title,
    TableHeader,
    BorderInactive,
    Gold,
}

pub fn style(role: SpanRole) -> Style {
    match role {
        SpanRole::Plain => Style::default(),
        SpanRole::Text => text(),
        SpanRole::Dim => dim(),
        SpanRole::System => system(),
        SpanRole::Accent => accent(),
        SpanRole::Error => error(),
        SpanRole::Title => title(),
        SpanRole::TableHeader => table_header(),
        SpanRole::BorderInactive => border_inactive(),
        SpanRole::Gold => gold(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Les volets et la barre d'état mesurent leur budget en cellules : un
    /// glyphe d'une autre largeur que les deux colonnes de l'emoji remplacé
    /// décalerait chaque troncature calculée autour de lui.
    #[test]
    fn every_pane_glyph_takes_two_cells() {
        for glyph in [
            EXPLORER_GLYPH,
            VIEWER_GLYPH,
            DIR_GLYPH,
            STEER_GLYPH,
            TOOL_GLYPH,
            ELAPSED_GLYPH,
            GATE_GLYPH,
        ] {
            assert_eq!(ratatui::text::Span::raw(glyph).width(), 2, "{glyph}");
        }
    }

    #[test]
    fn themes_are_six_uniquely_named_palettes_in_cycle_order() {
        let names: Vec<&str> = THEMES.iter().map(|p| p.name).collect();
        assert_eq!(
            names,
            ["zen", "light", "nord", "gruvbox", "solarized", "mono"]
        );
    }

    #[test]
    fn set_active_matches_names_case_insensitively() {
        let _guard = test_guard();

        set_active("NORD").expect("nord is a built-in theme");

        assert_eq!(active().name, "nord");
    }

    #[test]
    fn set_active_rejects_an_unknown_name_and_lists_the_available_ones() {
        let _guard = test_guard();

        let err = set_active("xyz").expect_err("xyz is not a theme");

        let message = err.to_string();
        assert!(message.contains("xyz"), "{message}");
        for palette in &THEMES {
            assert!(message.contains(palette.name), "{message}");
        }
        assert_eq!(active().name, "zen", "a rejected name changes nothing");
    }

    #[test]
    fn next_name_walks_the_cycle_and_wraps_to_the_first_theme() {
        assert_eq!(next_name("zen"), "light");
        assert_eq!(next_name("solarized"), "mono");
        assert_eq!(next_name("mono"), "zen");
    }

    #[test]
    fn mono_palette_avoids_rgb_so_it_survives_a_16_color_terminal() {
        let mono = THEMES
            .iter()
            .find(|p| p.name == "mono")
            .expect("mono is a built-in theme");

        for color in [
            mono.text,
            mono.muted,
            mono.user,
            mono.gold,
            mono.accent,
            mono.border_inactive,
            mono.code_bg,
            mono.chart_alt,
        ] {
            assert!(
                !matches!(color, Color::Rgb(..)),
                "mono must stay palette-free, got {color:?}"
            );
        }
    }

    #[test]
    fn zen_palette_keeps_the_historical_colors() {
        let zen = &THEMES[0];
        assert_eq!(zen.name, "zen");
        assert_eq!(zen.text, Color::Rgb(200, 200, 195));
        assert_eq!(zen.user, Color::Rgb(84, 110, 140));
        assert_eq!(zen.border_inactive, Color::Rgb(84, 110, 140));
        assert_eq!(zen.accent, Color::Rgb(203, 88, 65));
        assert_eq!(zen.gold, Color::Rgb(196, 164, 106));
        assert_eq!(zen.code_bg, Color::Rgb(40, 40, 38));
        assert_eq!(zen.chart_alt, Color::Rgb(139, 166, 108));
        assert_eq!(zen.muted, Color::DarkGray);
    }

    #[test]
    fn resolve_theme_prefers_env_over_config_over_the_default() {
        assert_eq!(resolve_theme(Some("nord"), Some("gruvbox")), "nord");
        assert_eq!(resolve_theme(None, Some("gruvbox")), "gruvbox");
        assert_eq!(resolve_theme(None, None), "zen");
        assert_eq!(resolve_theme(Some("MONO"), None), "mono");
    }

    #[test]
    fn resolve_theme_falls_back_to_the_default_on_an_unknown_value() {
        assert_eq!(resolve_theme(Some("xyz"), Some("nord")), "zen");
        assert_eq!(resolve_theme(None, Some("")), "zen");
    }

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
