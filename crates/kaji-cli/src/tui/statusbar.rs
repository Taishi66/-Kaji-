//! The bottom bar (« hanko & forge ») — the mode's vermilion seal, then the
//! place (directory ⟩ branch and repository state), an empty middle, and the
//! forge's telemetry pinned right: model, 炭 tokens, cost, and 火 while a turn
//! burns.
//!
//! Pure like [`crate::tui::gitstatus::render`], which it delegates the place
//! to: the whole line is built and fitted here, so it is unit-testable without
//! a terminal.

use crate::tui::app::{self, App};
use crate::tui::{gitstatus, theme};
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

/// A hanko is vermilion whatever it seals, so the mode rides on the kanji
/// alone. `REVERSED` fills the seal with the accent and hands the kanji the
/// terminal's own background — legible on a light theme as on a dark one.
fn seal_spans(app: &App) -> Vec<Span<'static>> {
    vec![
        Span::styled(
            format!(" {} ", app::kaji_mode_seal(app.kaji_mode)),
            theme::seal(),
        ),
        Span::raw(" "),
    ]
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

    if app.turn_active {
        let elapsed = app.turn_started.map(|t| t.elapsed()).unwrap_or_default();
        push_group(
            &mut spans,
            Span::styled(
                format!(
                    "{} {} {}s",
                    theme::FIRE_GLYPH,
                    theme::blade_frame(elapsed),
                    elapsed.as_secs()
                ),
                theme::accent(),
            ),
        );
    }

    spans.push(Span::raw(" "));
    spans
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
    use crate::tui::gitstatus::GitStatus;
    use kaji::config::KajiMode;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};
    use test_case::test_case;

    fn app_at(dir: &str) -> App {
        let mut app = App::new(None);
        app.set_working_dir(PathBuf::from(dir));
        app
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
    fn the_seal_carries_the_mode_as_a_kanji_and_stays_vermilion(mode: KajiMode, kanji: &str) {
        let _theme = theme::test_guard();
        let mut app = app_at("/tmp/project");
        app.kaji_mode = mode;

        let line = render(&app, 100);
        let rendered = text(&line);

        assert!(rendered.starts_with(&format!(" {kanji} ")), "{rendered:?}");
        assert_eq!(line.spans[0].style, theme::seal(), "{mode:?}");
        for word in ["auto", "approve", "smart", "chat"] {
            assert!(
                !rendered.contains(word),
                "le mot du mode reste hors de la barre : {rendered:?}"
            );
        }
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
        assert!(rendered.contains("12s"), "{rendered:?}");
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
            "12s",
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

        assert!(
            rendered.starts_with(&format!(" 自  {} /tmp/project ", theme::DIR_GLYPH)),
            "{rendered:?}"
        );
        assert!(!rendered.contains(theme::PLACE_SEPARATOR), "{rendered:?}");
    }
}
