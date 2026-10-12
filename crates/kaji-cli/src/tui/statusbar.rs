//! Permission mode and project on the left, labelled usage and activity on the right.
//!
//! Pure like [`crate::tui::gitstatus::render`], which it delegates the place
//! to: the whole line is built and fitted here, so it is unit-testable without
//! a terminal.

use crate::tui::app::{self, App};
use crate::tui::{gitstatus, icons, theme};
use kaji::config::KajiMode;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// Two spaces (間) rather than a bullet: the bar separates by breathing.
const SEPARATOR: &str = "  ";

fn bounded_spans(spans: Vec<Span<'static>>, budget: usize) -> Vec<Span<'static>> {
    let mut remaining = budget;
    let mut output = Vec::new();
    for span in spans {
        if remaining == 0 {
            break;
        }
        let content = gitstatus::truncate_cells(&span.content, remaining);
        remaining = remaining.saturating_sub(gitstatus::display_width(&content));
        output.push(Span::styled(content, span.style));
    }
    output
}

pub fn render_project(app: &App, width: u16) -> Line<'static> {
    let width = usize::from(width);
    let mut spans = bounded_spans(seal_spans(app), width);
    let activity = bounded_spans(
        activity_spans(app),
        width.saturating_sub(total_width(&spans) + 8),
    );
    let remaining = width.saturating_sub(total_width(&spans) + total_width(&activity));
    let place = if width < 80 {
        let name = app
            .working_dir()
            .file_name()
            .unwrap_or_else(|| app.working_dir().as_os_str())
            .to_string_lossy();
        let mut place = vec![Span::styled(name.into_owned(), theme::text())];
        if let Some(git) = &app.git_status {
            place.push(Span::styled(
                format!(" {} {}", theme::PLACE_SEPARATOR, git.branch),
                theme::dim(),
            ));
        }
        bounded_spans(place, remaining)
    } else {
        bounded_spans(
            gitstatus::render(app.git_status.as_ref(), app.working_dir(), remaining),
            remaining,
        )
    };
    spans.extend(place);
    let padding = width.saturating_sub(total_width(&spans) + total_width(&activity));
    spans.push(Span::raw(" ".repeat(padding)));
    spans.extend(activity);
    Line::from(spans)
}

pub fn render_collapsed(app: &App, width: u16) -> Line<'static> {
    let mut spans = bounded_spans(seal_spans(app), usize::from(width));
    let remaining = usize::from(width).saturating_sub(total_width(&spans));
    let model = gitstatus::truncate_cells(
        if app.model.is_empty() {
            "model ?"
        } else {
            &app.model
        },
        remaining,
    );
    spans.push(Span::styled(model, theme::text()));
    Line::from(spans)
}

fn metric_count(value: i64) -> String {
    if value >= 1_000_000_000 {
        format!("{value:.0e}", value = value as f64)
    } else {
        compact_count(value)
    }
}

pub fn format_cost(cost: Option<f64>, partial: bool) -> String {
    match cost.filter(|value| value.is_finite() && *value >= 0.0) {
        None => "cost ?".to_string(),
        Some(value) => {
            let amount = if value == 0.0 || value >= 0.01 {
                format!("${value:.2}")
            } else if value >= 0.0001 {
                format!("${value:.4}")
            } else {
                "$<0.0001".to_string()
            };
            format!("{} {amount}", if partial { "partial" } else { "est." })
        }
    }
}

pub fn render_telemetry(app: &App, width: u16) -> Line<'static> {
    let width = usize::from(width);
    let count = |value: usize| metric_count(i64::try_from(value).unwrap_or(i64::MAX));
    let stale = if app.context_refreshing { "…" } else { "" };
    let context = match (
        app.context_used,
        app.context_limit.filter(|limit| *limit > 0),
    ) {
        (Some(used), Some(limit)) => format!("ctx ≈{}/{} budget{stale}", count(used), count(limit)),
        (Some(used), None) => format!("ctx ≈{}/?{stale}", count(used)),
        _ => format!("ctx {}", if app.context_refreshing { "…" } else { "?" }),
    };
    let context_compact = match (
        app.context_used,
        app.context_limit.filter(|limit| *limit > 0),
    ) {
        (Some(used), Some(limit)) => format!(
            "ctx≈{}%{stale}",
            (((used as f64 / limit as f64) * 100.0).round() as u64).min(999)
        ),
        (Some(used), None) => format!("ctx≈{}/?{stale}", count(used)),
        _ => format!("ctx{}", if app.context_refreshing { "…" } else { "?" }),
    };
    let pending = if app.usage_refreshing { "…" } else { "" };
    let input = metric_count(app.tokens_total_in);
    let output = metric_count(app.tokens_total_out);
    let usage = if app.usage_ready {
        format!("session tokens{pending} {input}↑ {output}↓")
    } else {
        format!("session {}", if app.usage_refreshing { "…" } else { "?" })
    };
    let usage_compact = if app.usage_ready {
        format!("tok{pending}{input}↑{output}↓")
    } else {
        format!("tok{}", if app.usage_refreshing { "…" } else { "?" })
    };
    let usage_total = if app.usage_ready {
        format!(
            "tok{}{pending}",
            metric_count(app.tokens_total_in.saturating_add(app.tokens_total_out))
        )
    } else {
        usage_compact.clone()
    };
    let cost = format_cost(app.cost_total, app.cost_partial);
    let cost_compact = cost
        .replace("cost ?", "$?")
        .replace("est. ", "≈")
        .replace("partial ", "*")
        .replace("$0.", "$.")
        .replace("$<0.", "$<.");
    let cost_short = if app
        .cost_total
        .is_some_and(|value| value >= 10000.0 && value.is_finite())
    {
        format!(
            "{}${:.0e}",
            if app.cost_partial { "*" } else { "≈" },
            app.cost_total.unwrap_or_default()
        )
    } else {
        cost_compact.clone()
    };
    let candidates = [
        ([context, usage, cost], " · "),
        ([context_compact.clone(), usage_compact, cost_compact], " "),
        ([context_compact, usage_total, cost_short], " "),
    ];
    let model_name = if app.model.is_empty() {
        "model ?"
    } else {
        &app.model
    };
    let model_reserve = if width >= 60 {
        gitstatus::display_width(model_name).clamp(8, 24)
    } else {
        8
    };
    let fits = |candidate: &([String; 3], &str), reserve: usize| {
        candidate
            .0
            .iter()
            .map(|group| gitstatus::display_width(group))
            .sum::<usize>()
            + gitstatus::display_width(candidate.1) * 2
            <= width.saturating_sub(width.min(reserve) + 3)
    };
    let (groups, separator) = candidates
        .iter()
        .find(|candidate| fits(candidate, model_reserve))
        .or_else(|| candidates.iter().find(|candidate| fits(candidate, 8)))
        .cloned()
        .unwrap_or_else(|| {
            (
                ["ctx?".to_string(), "tok?".to_string(), "$?".to_string()],
                " ",
            )
        });
    let detail_width = groups
        .iter()
        .map(|group| gitstatus::display_width(group))
        .sum::<usize>()
        + gitstatus::display_width(separator) * 2;
    let model_budget = width.saturating_sub(detail_width + 3);
    let model = gitstatus::truncate_cells(
        if app.model.is_empty() {
            "model ?"
        } else {
            &app.model
        },
        model_budget,
    );
    let mut spans = vec![
        Span::styled(model, theme::text()),
        Span::styled(" · ", theme::dim()),
    ];
    let pressure = app
        .context_used
        .zip(app.context_limit)
        .is_some_and(|(used, limit)| limit > 0 && used as f64 / limit as f64 >= 0.8);
    for (index, group) in groups.into_iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(separator, theme::dim()));
        }
        let style = if (index == 0 && pressure) || (index == 2 && app.cost_total.is_some()) {
            theme::gold()
        } else {
            theme::text()
        };
        spans.push(Span::styled(group, style));
    }
    Line::from(bounded_spans(spans, width))
}

/// The word always explains the seal; bold briefly marks a mode change.
fn seal_spans(app: &App) -> Vec<Span<'static>> {
    let color = mode_color(app.kaji_mode);
    let mut spans = vec![Span::styled(
        format!(" {} ", app::kaji_mode_seal(app.kaji_mode)),
        theme::seal(color),
    )];

    if let Some(icon) = icons::mode_icon(app.icons, app.kaji_mode) {
        spans.push(Span::styled(format!(" {icon}"), Style::default().fg(color)));
    }

    let mut word_style = Style::default().fg(color);
    if app.seal_unfolded() {
        word_style = word_style.add_modifier(Modifier::BOLD);
    }
    spans.push(Span::styled(
        format!(" {}", app::kaji_mode_badge(app.kaji_mode)),
        word_style,
    ));

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

fn activity_spans(app: &App) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let blades = app.forge.running_count();
    if blades > 0 && !app.forge.visible() {
        push_group(
            &mut spans,
            Span::styled(
                format!("{} {blades} agents", theme::SUBAGENT_GLYPH),
                theme::dim(),
            ),
        );
    }

    if app.turn_active {
        let elapsed = app.turn_started.map(|t| t.elapsed()).unwrap_or_default();
        let phase = app
            .current_tool()
            .map(truncate_tool_name)
            .unwrap_or_else(|| format!("{} thinking", theme::THINKING_GLYPH));
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

    if !spans.is_empty() {
        spans.insert(0, Span::raw(SEPARATOR));
        spans.push(Span::raw(" "));
    }
    spans
}

/// Cells a tool name is owed on the fire before it is cut — the place keeps
/// priority through [`fits`], so a long MCP-qualified name never gets to
/// contest it.
const FIRE_PHASE_MAX_WIDTH: usize = 24;

fn truncate_tool_name(name: &str) -> String {
    gitstatus::truncate_cells(name, FIRE_PHASE_MAX_WIDTH)
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

    #[test]
    fn positive_costs_never_round_to_zero_and_unknown_is_explicit() {
        assert_eq!(format_cost(None, false), "cost ?");
        assert_eq!(format_cost(Some(0.0), false), "est. $0.00");
        assert_eq!(format_cost(Some(0.0012), false), "est. $0.0012");
        assert_eq!(format_cost(Some(0.000001), true), "partial $<0.0001");
        assert_eq!(format_cost(Some(f64::NAN), false), "cost ?");
    }

    #[test]
    fn session_telemetry_keeps_model_context_usage_and_cost_at_all_widths() {
        let _theme = theme::test_guard();
        let mut app = App::new(None);
        app.model = "deepseek-v4.1-flash".to_string();
        app.context_used = Some(4096);
        app.context_limit = Some(128000);
        app.context_refreshing = false;
        app.usage_ready = true;
        app.tokens_total_in = 12000;
        app.tokens_total_out = 2400;
        app.cost_total = Some(0.0012);
        for width in [40, 80, 120, 200] {
            let line = render_telemetry(&app, width);
            let rendered = text(&line);
            assert!(line.width() <= usize::from(width), "{width}: {rendered}");
            assert!(rendered.contains("deep"), "{width}: {rendered}");
            assert!(rendered.contains("ctx"), "{width}: {rendered}");
            assert!(
                rendered.contains('↑') && rendered.contains('↓'),
                "{width}: {rendered}"
            );
            assert!(rendered.contains("0012"), "{width}: {rendered}");
        }
    }

    #[test]
    fn unknown_capacity_is_not_replaced_by_a_harness_default() {
        let _theme = theme::test_guard();
        let mut app = App::new(None);
        app.model = "local".to_string();
        app.context_used = Some(1000);
        app.context_refreshing = false;
        assert!(text(&render_telemetry(&app, 120)).contains("ctx ≈1.0k/?"));
    }

    fn app_at(dir: &str) -> App {
        let mut app = App::new(None);
        app.set_working_dir(PathBuf::from(dir));
        app
    }

    /// Vingt-quatre kanji valent quarante-huit cellules : une coupe en
    /// `chars()` les laissait tous passer et le feu poussait le lieu hors de
    /// la barre.
    #[test]
    fn the_fire_cuts_a_tool_name_on_cells_not_chars() {
        let name = "工".repeat(24);

        let cut = truncate_tool_name(&name);

        assert!(
            gitstatus::display_width(&cut) <= FIRE_PHASE_MAX_WIDTH,
            "{cut:?} fait {} cellules",
            gitstatus::display_width(&cut)
        );
        assert!(cut.ends_with('…'), "la coupe se dit : {cut:?}");
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
                seq: 0,
            },
        );
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
    fn the_seal_keeps_the_mode_word_visible_at_rest(mode: KajiMode, kanji: &str) {
        let _theme = theme::test_guard();
        let mut app = app_at("/tmp/project");
        app.kaji_mode = mode;

        let line = render_project(&app, 100);
        let rendered = text(&line);

        assert!(rendered.starts_with(&format!(" {kanji} ")), "{rendered:?}");
        assert!(
            rendered.contains(app::kaji_mode_badge(mode)),
            "{rendered:?}"
        );
        assert!(!rendered
            .chars()
            .any(|c| ('\u{e000}'..='\u{f8ff}').contains(&c)));
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
        app.icons = IconSet::Nerd;
        let expected = match mode {
            KajiMode::Approve => theme::user_color(),
            KajiMode::SmartApprove => theme::gold_color(),
            KajiMode::Auto => theme::accent_color(),
            KajiMode::Chat => theme::muted_color(),
        };

        let line = render_project(&app, 100);

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
        app.icons = IconSet::Nerd;
        let icon = icons::mode_icon(IconSet::Nerd, mode).expect("une icône par mode");

        let with_icon = text(&render_project(&app, 100));
        app.icons = IconSet::Text;
        let without = text(&render_project(&app, 100));

        assert!(
            with_icon.contains(icon),
            "{mode:?} : icône absente de {with_icon:?}"
        );
        assert!(!without.contains(icon), "{mode:?} : {without:?}");
        assert!(
            without.starts_with(&format!(
                " {}  {}  {} ",
                app::kaji_mode_seal(mode),
                app::kaji_mode_badge(mode),
                theme::DIR_GLYPH
            )),
            "the text mode keeps its label: {without:?}"
        );
    }

    #[test]
    fn a_mode_change_emphasizes_the_word_without_hiding_it_at_rest() {
        let _theme = theme::test_guard();
        let mut app = app_at("/tmp/project");
        app.kaji_mode = KajiMode::SmartApprove;

        let resting = render_project(&app, 100);
        let word = resting
            .spans
            .iter()
            .find(|span| span.content.trim() == "smart")
            .expect("mode word");
        assert!(!word.style.add_modifier.contains(Modifier::BOLD));

        app.unfold_seal();
        let changed = render_project(&app, 100);
        let rendered = text(&changed);
        let word = changed
            .spans
            .iter()
            .find(|span| span.content.trim() == "smart")
            .expect("mode word");
        assert!(word.style.add_modifier.contains(Modifier::BOLD));

        assert!(rendered.starts_with(" 智 "), "{rendered:?}");
        assert!(rendered.contains("smart"), "{rendered:?}");
        assert!(
            rendered.contains(&format!("smart  {}", theme::DIR_GLYPH)),
            "the mode stays before the project: {rendered:?}"
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

        let thinking = text(&render_project(&app, 130));
        assert!(
            thinking.contains(&format!("{} ", theme::FIRE_GLYPH)),
            "{thinking:?}"
        );
        assert!(thinking.contains(theme::THINKING_GLYPH), "{thinking:?}");
        assert!(!thinking.contains("12s"), "{thinking:?}");

        running_tool(&mut app, "shell");
        let running = text(&render_project(&app, 130));

        assert!(running.contains("shell"), "{running:?}");
        assert!(!running.contains(theme::THINKING_GLYPH), "{running:?}");
        assert!(!running.contains("12s"), "{running:?}");
    }

    /// Un nom d'outil MCP à rallonge ne doit pas manger le lieu : le feu le
    /// coupe avant que `la mise en page ait à arbitrer.
    #[test]
    fn a_long_tool_name_is_cut_by_the_fire_itself() {
        let _theme = theme::test_guard();
        let mut app = app_at("/tmp/project");
        app.turn_active = true;
        running_tool(&mut app, "developer__shell_with_a_very_long_qualified_name");

        let rendered = text(&render_project(&app, 130));

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
            !text(&render_project(&app, 130)).contains(theme::SUBAGENT_GLYPH),
            "aucune lame déléguée"
        );

        running_blade(&mut app, "t1");
        running_blade(&mut app, "t2");
        assert!(app.forge.visible(), "deux lames vives ouvrent le volet");
        assert!(
            !text(&render_project(&app, 130)).contains(theme::SUBAGENT_GLYPH),
            "le volet dit déjà tout"
        );

        app.forge.view = ForgeView::ForcedClosed;
        let rendered = text(&render_project(&app, 130));
        assert!(
            rendered.contains(&format!("{} 2", theme::SUBAGENT_GLYPH)),
            "{rendered:?}"
        );

        app.forge.tasks.clear();
        assert!(
            !text(&render_project(&app, 130)).contains(theme::SUBAGENT_GLYPH),
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
    fn session_scope_does_not_switch_to_turn_tokens_while_active() {
        let _theme = theme::test_guard();
        let mut app = app_at("/tmp/project");
        app.turn_active = true;
        app.usage_ready = true;
        app.usage_refreshing = false;
        app.tokens_turn_in = 120;
        app.tokens_total_in = 99000;
        let line = text(&render_telemetry(&app, 120));
        assert!(line.contains("session tokens 99k↑"), "{line}");
        assert!(!line.contains("120↑"), "{line}");
    }

    #[test]
    fn project_and_telemetry_are_bounded_with_extreme_values() {
        let _theme = theme::test_guard();
        let mut app = app_at("/tmp/非常に長いプロジェクト名");
        app.model = "actual-model-非常に長いモデル".repeat(4);
        app.git_status = Some(GitStatus {
            branch: "long-branch".repeat(20),
            modified: usize::MAX,
            insertions: usize::MAX,
            ..GitStatus::default()
        });
        app.turn_active = true;
        app.usage_ready = true;
        app.tokens_total_in = i64::MAX;
        app.tokens_total_out = i64::MAX;
        app.context_used = Some(usize::MAX);
        app.cost_total = Some(0.000001);
        app.cost_partial = true;
        for limit in [None, Some(1), Some(usize::MAX)] {
            app.context_limit = limit;
            for width in [40, 80, 120, 200] {
                let line = render_telemetry(&app, width);
                assert!(
                    line.width() <= usize::from(width),
                    "{width}: {}",
                    text(&line)
                );
                assert!(
                    text(&line).starts_with("actual-"),
                    "{width}: {}",
                    text(&line)
                );
                assert!(text(&line).contains('$'), "{width}: {}", text(&line));
                assert!(render_project(&app, width).width() <= usize::from(width));
            }
        }
    }
}
