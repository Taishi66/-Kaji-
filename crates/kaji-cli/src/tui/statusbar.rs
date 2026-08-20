//! The bottom bar (« hanko & forge ») — the mode's vermilion seal, then the
//! place (directory ⟩ branch and repository state), an empty middle, and the
//! forge's telemetry pinned right: model, 炭 tokens, cost, 遣 the blades running
//! behind a folded 炉 panel, and 火 while a turn burns.
//!
//! Pure like [`crate::tui::gitstatus::render`], which it delegates the place
//! to: the whole line is built and fitted here, so it is unit-testable without
//! a terminal.

use crate::tui::app::{self, App};
use crate::tui::{gitstatus, icons, theme};
use kaji::config::KajiMode;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

/// Two spaces (間) rather than a bullet: the bar separates by breathing.
const SEPARATOR: &str = "  ";

/// Cells the place is owed before the telemetry may claim any. Below it the
/// telemetry drops the model, then itself — the bar always says where it
/// stands.
const MIN_PLACE_WIDTH: usize = 24;

pub fn render(app: &App, width: u16) -> Line<'static> {
    let width = width as usize;
    let seal = seal_spans(app);
    let seal_width = total_width(&seal);

    let mut telemetry = telemetry_spans(app, true);
    let mut place = place_spans(app, width, seal_width, &telemetry);
    if !fits(width, seal_width, &place, &telemetry) {
        telemetry = telemetry_spans(app, false);
        place = place_spans(app, width, seal_width, &telemetry);
    }
    if !fits(width, seal_width, &place, &telemetry) {
        telemetry = Vec::new();
        place = place_spans(app, width, seal_width, &telemetry);
    }

    let ma = width.saturating_sub(seal_width + total_width(&place) + total_width(&telemetry));
    let mut spans = seal;
    spans.extend(place);
    spans.push(Span::raw(" ".repeat(ma)));
    spans.extend(telemetry);
    Line::from(spans)
}

/// The telemetry gives way — its model first, then itself — when the place is
/// left under `MIN_PLACE_WIDTH` cells, or when the line would not fit at all
/// because the repository state is too wide to shrink.
fn fits(
    width: usize,
    seal_width: usize,
    place: &[Span<'static>],
    telemetry: &[Span<'static>],
) -> bool {
    let telemetry_width = total_width(telemetry);
    width >= seal_width + MIN_PLACE_WIDTH + telemetry_width
        && width >= seal_width + total_width(place) + telemetry_width
}

fn place_spans(
    app: &App,
    width: usize,
    seal_width: usize,
    telemetry: &[Span<'static>],
) -> Vec<Span<'static>> {
    gitstatus::render(
        app.git_status.as_ref(),
        app.working_dir(),
        width.saturating_sub(seal_width + total_width(telemetry)),
    )
}

/// The seal names the mode with its kanji, `REVERSED` in the mode's colour so
/// it stays legible on a light theme as on a dark one; the icon (unless
/// `KAJI_ICONS=text`) and, while unfolded, the mode's word ride beside it in
/// that same colour, not reversed.
fn seal_spans(app: &App) -> Vec<Span<'static>> {
    let color = mode_color(app.kaji_mode);
    let mut spans = vec![Span::styled(
        format!(" {} ", app::kaji_mode_seal(app.kaji_mode)),
        theme::seal(color),
    )];

    if let Some(icon) = icons::mode_icon(app.icons, app.kaji_mode) {
        spans.push(Span::styled(format!(" {icon}"), Style::default().fg(color)));
    }

    if app.seal_unfolded() {
        spans.push(Span::styled(
            format!(" {}", app::kaji_mode_badge(app.kaji_mode)),
            Style::default().fg(color),
        ));
    }

    spans.push(Span::raw("  "));
    spans
}

/// Ce que le sceau promet, en couleur : l'humain décide (indigo, celui de
/// `vous ▸`), kaji juge (or, celui de 鍛冶), personne n'est consulté
/// (vermillon), aucun outil ne tourne (gris).
pub(crate) fn mode_color(mode: KajiMode) -> Color {
    match mode {
        KajiMode::Approve => theme::user_color(),
        KajiMode::SmartApprove => theme::gold_color(),
        KajiMode::Auto => theme::accent_color(),
        KajiMode::Chat => theme::muted_color(),
    }
}

/// The forge's numbers, right to left of the trailing margin. The fire burns
/// for the current turn only: at rest the silence is the information.
fn telemetry_spans(app: &App, with_model: bool) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    if with_model && !app.model.is_empty() {
        push_group(&mut spans, Span::styled(app.model.clone(), theme::dim()));
    }

    let (input, output) = if app.turn_active {
        (app.tokens_turn_in, app.tokens_turn_out)
    } else {
        (app.tokens_total_in, app.tokens_total_out)
    };
    push_group(
        &mut spans,
        Span::styled(
            format!(
                "{} {}↑ {}↓",
                theme::TOKENS_GLYPH,
                compact_count(input),
                compact_count(output)
            ),
            theme::dim(),
        ),
    );

    if let Some(cost) = app.cost_total {
        push_group(
            &mut spans,
            Span::styled(format!("${cost:.2}"), theme::gold()),
        );
    }

    let blades = app.forge.running_count();
    if blades > 0 && !app.forge.visible() {
        push_group(
            &mut spans,
            Span::styled(format!("{} {blades}", theme::SUBAGENT_GLYPH), theme::dim()),
        );
    }

    if app.turn_active {
        let elapsed = app.turn_started.map(|t| t.elapsed()).unwrap_or_default();
        let phase = app
            .current_tool()
            .map(truncate_tool_name)
            .unwrap_or_else(|| theme::THINKING_GLYPH.to_string());
        push_group(
            &mut spans,
            Span::styled(
                format!(
                    "{} {} {phase}",
                    theme::FIRE_GLYPH,
                    theme::blade_frame(elapsed)
                ),
                theme::accent(),
            ),
        );
    }

    spans.push(Span::raw(" "));
    spans
}

/// Cells a tool name is owed on the fire before it is cut — the place keeps
/// priority through [`fits`], so a long MCP-qualified name never gets to
/// contest it.
const FIRE_PHASE_MAX_WIDTH: usize = 24;

fn truncate_tool_name(name: &str) -> String {
    if name.chars().count() <= FIRE_PHASE_MAX_WIDTH {
        return name.to_string();
    }
    let head: String = name.chars().take(FIRE_PHASE_MAX_WIDTH - 1).collect();
    format!("{head}…")
}

fn push_group(spans: &mut Vec<Span<'static>>, group: Span<'static>) {
    if !spans.is_empty() {
        spans.push(Span::raw(SEPARATOR));
    }
    spans.push(group);
}

/// Four cells at most, truncated rather than rounded: a counter that grows must
/// never widen the telemetry and push the place around.
pub fn compact_count(n: i64) -> String {
    match n.max(0) {
        n @ 0..=999 => n.to_string(),
        n @ 1_000..=9_999 => format!("{}.{}k", n / 1_000, n % 1_000 / 100),
        n @ 10_000..=999_999 => format!("{}k", n / 1_000),
        n @ 1_000_000..=9_999_999 => format!("{}.{}M", n / 1_000_000, n % 1_000_000 / 100_000),
        n => format!("{}M", n / 1_000_000),
    }
}

fn total_width(spans: &[Span<'static>]) -> usize {
    spans.iter().map(Span::width).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::forge::{ForgeStatus, ForgeTask, ForgeView};
    use crate::tui::gitstatus::GitStatus;
    use crate::tui::icons::IconSet;
    use kaji::agents::AgentEvent;
    use kaji::conversation::message::Message;
    use rmcp::model::CallToolRequestParams;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};
    use test_case::test_case;

    fn app_at(dir: &str) -> App {
        let mut app = App::new(None);
        app.set_working_dir(PathBuf::from(dir));
        app
    }

    fn running_tool(app: &mut App, name: &str) {
        app.apply_agent_event(&AgentEvent::Message(
            Message::assistant()
                .with_tool_request("t1", Ok(CallToolRequestParams::new(name.to_string()))),
        ));
    }

    fn running_blade(app: &mut App, id: &str) {
        app.forge.tasks.insert(
            id.to_string(),
            ForgeTask {
                id: id.to_string(),
                description: id.to_string(),
                status: ForgeStatus::Running,
                current_tool: None,
                elapsed_secs: 0,
                turns: 0,
                result: None,
                error: None,
            },
        );
    }

    fn on_a_branch() -> GitStatus {
        GitStatus {
            branch: "feat/kaji-init".to_string(),
            modified: 3,
            untracked: 2,
            insertions: 40,
            deletions: 12,
            ..GitStatus::default()
        }
    }

    fn text(line: &Line<'static>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test_case(KajiMode::Auto, "自"; "auto")]
    #[test_case(KajiMode::Approve, "承"; "approve")]
    #[test_case(KajiMode::SmartApprove, "智"; "smart")]
    #[test_case(KajiMode::Chat, "話"; "chat")]
    fn the_seal_carries_the_mode_as_a_kanji_folded_over_its_word(mode: KajiMode, kanji: &str) {
        let _theme = theme::test_guard();
        let mut app = app_at("/tmp/project");
        app.kaji_mode = mode;

        let line = render(&app, 100);
        let rendered = text(&line);

        assert!(rendered.starts_with(&format!(" {kanji} ")), "{rendered:?}");
        for word in ["auto", "approve", "smart", "chat"] {
            assert!(
                !rendered.contains(word),
                "le mot du mode reste replié : {rendered:?}"
            );
        }
    }

    /// La couleur est la seconde traduction du kanji, et elle dit qui décide :
    /// l'humain, kaji, ou personne.
    #[test_case(KajiMode::Approve; "approve_est_indigo")]
    #[test_case(KajiMode::SmartApprove; "smart_est_or")]
    #[test_case(KajiMode::Auto; "auto_est_vermillon")]
    #[test_case(KajiMode::Chat; "chat_est_inerte")]
    fn the_seal_and_its_icon_take_the_modes_colour(mode: KajiMode) {
        let _theme = theme::test_guard();
        let mut app = app_at("/tmp/project");
        app.kaji_mode = mode;
        let expected = match mode {
            KajiMode::Approve => theme::user_color(),
            KajiMode::SmartApprove => theme::gold_color(),
            KajiMode::Auto => theme::accent_color(),
            KajiMode::Chat => theme::muted_color(),
        };

        let line = render(&app, 100);

        assert_eq!(line.spans[0].style, theme::seal(expected), "{mode:?}");
        assert_eq!(
            line.spans[1].style,
            Style::default().fg(expected),
            "l'icône porte la couleur du sceau sans l'inverser : {mode:?}"
        );
    }

    #[test_case(KajiMode::Approve; "approve")]
    #[test_case(KajiMode::SmartApprove; "smart")]
    #[test_case(KajiMode::Auto; "auto")]
    #[test_case(KajiMode::Chat; "chat")]
    fn the_icon_follows_the_seal_and_the_text_fallback_drops_it(mode: KajiMode) {
        let _theme = theme::test_guard();
        let mut app = app_at("/tmp/project");
        app.kaji_mode = mode;
        let icon = icons::mode_icon(IconSet::Nerd, mode).expect("une icône par mode");

        let with_icon = text(&render(&app, 100));
        app.icons = IconSet::Text;
        let without = text(&render(&app, 100));

        assert!(
            with_icon.contains(icon),
            "{mode:?} : icône absente de {with_icon:?}"
        );
        assert!(!without.contains(icon), "{mode:?} : {without:?}");
        assert!(
            without.starts_with(&format!(
                " {}   {} ",
                app::kaji_mode_seal(mode),
                theme::DIR_GLYPH
            )),
            "le repli texte rend la barre d'avant l'icône : {without:?}"
        );
    }

    /// Le kanji ne se traduit pas tout seul : le mot se déplie à côté du sceau
    /// au démarrage et à chaque changement de mode, puis s'efface.
    #[test]
    fn an_unfolded_seal_spells_its_mode_out_next_to_the_kanji() {
        let _theme = theme::test_guard();
        let mut app = app_at("/tmp/project");
        app.kaji_mode = KajiMode::SmartApprove;

        assert!(!text(&render(&app, 100)).contains("smart"));

        app.unfold_seal();
        let rendered = text(&render(&app, 100));

        assert!(rendered.starts_with(" 智 "), "{rendered:?}");
        assert!(rendered.contains("smart"), "{rendered:?}");
        assert!(
            rendered.contains(&format!("smart  {}", theme::DIR_GLYPH)),
            "le mot se déplie hors du sceau, avant le lieu : {rendered:?}"
        );
    }

    /// Le composer garde le chrono (`刻 12s`) : la barre dit ce qui brûle, pas
    /// depuis combien de temps.
    #[test]
    fn the_fire_names_the_running_tool_and_leaves_the_chrono_to_the_composer() {
        let _theme = theme::test_guard();
        let mut app = app_at("/tmp/project");
        app.turn_active = true;
        app.turn_started = Some(Instant::now() - Duration::from_secs(12));

        let thinking = text(&render(&app, 130));
        assert!(
            thinking.contains(&format!("{} ", theme::FIRE_GLYPH)),
            "{thinking:?}"
        );
        assert!(thinking.contains(theme::THINKING_GLYPH), "{thinking:?}");
        assert!(!thinking.contains("12s"), "{thinking:?}");

        running_tool(&mut app, "shell");
        let running = text(&render(&app, 130));

        assert!(running.contains("shell"), "{running:?}");
        assert!(!running.contains(theme::THINKING_GLYPH), "{running:?}");
        assert!(!running.contains("12s"), "{running:?}");
    }

    /// Un nom d'outil MCP à rallonge ne doit pas manger le lieu : le feu le
    /// coupe avant que `fits()` ait à arbitrer.
    #[test]
    fn a_long_tool_name_is_cut_by_the_fire_itself() {
        let _theme = theme::test_guard();
        let mut app = app_at("/tmp/project");
        app.turn_active = true;
        running_tool(&mut app, "developer__shell_with_a_very_long_qualified_name");

        let rendered = text(&render(&app, 130));

        assert!(
            rendered.contains("developer__shell_with_a…"),
            "{rendered:?}"
        );
    }

    /// Le compte des lames n'a de valeur que quand le volet 炉 est replié :
    /// ouvert, il les nomme une par une et la barre se tairait pour rien.
    #[test]
    fn the_bar_counts_the_running_blades_only_while_the_forge_panel_is_folded() {
        let _theme = theme::test_guard();
        let mut app = app_at("/tmp/project");

        assert!(
            !text(&render(&app, 130)).contains(theme::SUBAGENT_GLYPH),
            "aucune lame déléguée"
        );

        running_blade(&mut app, "t1");
        running_blade(&mut app, "t2");
        assert!(app.forge.visible(), "deux lames vives ouvrent le volet");
        assert!(
            !text(&render(&app, 130)).contains(theme::SUBAGENT_GLYPH),
            "le volet dit déjà tout"
        );

        app.forge.view = ForgeView::ForcedClosed;
        let rendered = text(&render(&app, 130));
        assert!(
            rendered.contains(&format!("{} 2", theme::SUBAGENT_GLYPH)),
            "{rendered:?}"
        );

        app.forge.tasks.clear();
        assert!(
            !text(&render(&app, 130)).contains(theme::SUBAGENT_GLYPH),
            "plus rien ne tourne"
        );
    }

    #[test_case(0, "0"; "zero")]
    #[test_case(987, "987"; "hundreds_stay_exact")]
    #[test_case(1_249, "1.2k"; "thousands_keep_one_truncated_decimal")]
    #[test_case(1_250, "1.2k"; "thousands_never_round_up")]
    #[test_case(9_999, "9.9k"; "thousands_top")]
    #[test_case(12_345, "12k"; "ten_thousands_drop_the_decimal")]
    #[test_case(123_456, "123k"; "hundred_thousands")]
    #[test_case(1_234_567, "1.2M"; "millions_keep_one_truncated_decimal")]
    #[test_case(12_345_678, "12M"; "ten_millions_drop_the_decimal")]
    fn compact_count_keeps_every_magnitude_within_four_cells(n: i64, expected: &str) {
        assert_eq!(compact_count(n), expected);
    }

    #[test]
    fn an_active_turn_burns_and_counts_the_turns_own_tokens() {
        let _theme = theme::test_guard();
        let mut app = app_at("/tmp/project");
        app.turn_active = true;
        app.turn_started = Some(Instant::now() - Duration::from_secs(12));
        app.tokens_turn_in = 120;
        app.tokens_turn_out = 340;
        app.tokens_total_in = 99_999;

        let rendered = text(&render(&app, 130));

        assert!(
            rendered.contains(&format!("{} 120↑ 340↓", theme::TOKENS_GLYPH)),
            "{rendered:?}"
        );
        assert!(
            !rendered.contains("99k"),
            "les totaux attendent : {rendered:?}"
        );
        assert!(rendered.contains(theme::FIRE_GLYPH), "{rendered:?}");
    }

    #[test]
    fn an_idle_bar_goes_silent_and_shows_the_session_totals() {
        let _theme = theme::test_guard();
        let mut app = app_at("/tmp/project");
        app.tokens_total_in = 12_000;
        app.tokens_total_out = 4_100;

        let rendered = text(&render(&app, 130));

        assert!(
            rendered.contains(&format!("{} 12k↑ 4.1k↓", theme::TOKENS_GLYPH)),
            "{rendered:?}"
        );
        assert!(
            !rendered.contains(theme::FIRE_GLYPH),
            "le feu et son chrono s'éteignent : {rendered:?}"
        );
    }

    #[test]
    fn the_cost_shows_in_gold_only_once_there_is_one() {
        let _theme = theme::test_guard();
        let mut app = app_at("/tmp/project");

        assert!(!text(&render(&app, 130)).contains('$'), "coût inconnu");

        app.cost_total = Some(0.42);
        let line = render(&app, 130);

        assert!(text(&line).contains("$0.42"), "{:?}", text(&line));
        let cost = line
            .spans
            .iter()
            .find(|span| span.content.contains('$'))
            .expect("le coût est rendu");
        assert_eq!(cost.style, theme::gold());
    }

    #[test]
    fn the_model_shows_when_the_session_knows_it() {
        let _theme = theme::test_guard();
        let mut app = app_at("/tmp/project");

        assert!(
            !text(&render(&app, 130)).contains("claude"),
            "modèle inconnu"
        );

        app.model = "claude-fable-5".to_string();

        assert!(text(&render(&app, 130)).contains("claude-fable-5"));
    }

    /// A narrow terminal gives the place its `MIN_PLACE_WIDTH` back by dropping
    /// the model first — the numbers are what the user watches.
    #[test]
    fn a_narrow_bar_drops_the_model_before_anything_else() {
        let _theme = theme::test_guard();
        let mut app = app_at("/tmp/project");
        app.git_status = Some(GitStatus {
            branch: "feat/kaji-init".to_string(),
            modified: 3,
            ..GitStatus::default()
        });
        app.model = "claude-fable-5".to_string();
        app.tokens_total_in = 12_000;
        app.tokens_total_out = 4_100;
        app.cost_total = Some(1.30);

        let line = render(&app, 60);
        let rendered = text(&line);

        assert!(
            line.width() <= 60,
            "{} cellules : {rendered:?}",
            line.width()
        );
        assert!(rendered.starts_with(" 自 "), "{rendered:?}");
        assert!(
            rendered.contains(
                icons::mode_icon(IconSet::Nerd, KajiMode::Auto).expect("une icône par mode")
            ),
            "l'icône ne se coupe jamais : {rendered:?}"
        );
        assert!(rendered.contains(theme::DIR_GLYPH), "{rendered:?}");
        assert!(!rendered.contains("claude-fable-5"), "{rendered:?}");
        assert!(rendered.contains("$1.30"), "{rendered:?}");
        assert!(
            rendered.contains(&format!("{} 12k↑ 4.1k↓", theme::TOKENS_GLYPH)),
            "{rendered:?}"
        );
    }

    /// Last rung: a repository state too wide to shrink takes the whole line
    /// rather than pushing the telemetry off the right edge.
    #[test]
    fn a_bar_too_narrow_for_both_gives_the_line_to_the_place() {
        let _theme = theme::test_guard();
        let mut app = app_at("/tmp/project");
        app.git_status = Some(on_a_branch());
        app.model = "claude-fable-5".to_string();
        app.cost_total = Some(1.30);

        let line = render(&app, 50);
        let rendered = text(&line);

        assert!(
            line.width() <= 50,
            "{} cellules : {rendered:?}",
            line.width()
        );
        assert!(rendered.starts_with(" 自 "), "{rendered:?}");
        assert!(rendered.contains("feat/kaji-init"), "{rendered:?}");
        assert!(!rendered.contains(theme::TOKENS_GLYPH), "{rendered:?}");
        assert!(!rendered.contains('$'), "{rendered:?}");
    }

    #[test]
    fn a_wide_bar_carries_the_place_the_branch_and_the_whole_telemetry() {
        let _theme = theme::test_guard();
        let mut app = app_at("/tmp/workspace/kaji");
        app.git_status = Some(on_a_branch());
        app.model = "claude-fable-5".to_string();
        app.turn_active = true;
        app.turn_started = Some(Instant::now() - Duration::from_secs(12));
        app.tokens_turn_in = 120;
        app.tokens_turn_out = 340;
        app.cost_total = Some(0.42);

        let line = render(&app, 130);
        let rendered = text(&line);

        assert_eq!(
            line.width(),
            130,
            "la barre remplit sa ligne : {rendered:?}"
        );
        for expected in [
            " 自 ",
            "在 /tmp/workspace/kaji",
            theme::PLACE_SEPARATOR,
            "feat/kaji-init",
            "✚3",
            "…2",
            "+40",
            "−12",
            "claude-fable-5",
            "炭 120↑ 340↓",
            "$0.42",
            "火",
            theme::THINKING_GLYPH,
        ] {
            assert!(
                rendered.contains(expected),
                "{expected} manquant : {rendered:?}"
            );
        }
    }

    /// Outside a repository the place is the directory alone — the bar keeps
    /// its seal and its telemetry either way.
    #[test]
    fn outside_a_repository_only_the_directory_holds_the_left_side() {
        let _theme = theme::test_guard();
        let app = app_at("/tmp/project");

        let rendered = text(&render(&app, 130));

        let icon = icons::mode_icon(IconSet::Nerd, KajiMode::Auto).expect("une icône par mode");
        assert!(
            rendered.starts_with(&format!(" 自  {icon}  {} /tmp/project ", theme::DIR_GLYPH)),
            "{rendered:?}"
        );
        assert!(!rendered.contains(theme::PLACE_SEPARATOR), "{rendered:?}");
    }
}
