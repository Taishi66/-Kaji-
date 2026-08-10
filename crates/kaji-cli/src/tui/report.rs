//! Tableaux alignés à largeur fixe pour les blocs système `/cost` et
//! `/docker` — construits par `format!` paddé plutôt que via un widget
//! `Table` ratatui, pour rester une simple liste de `Line` insérable dans le
//! flux du chat.

use crate::tui::theme;
use kaji::session::{UsageAggregate, UsageWindows};
use ratatui::style::Style;
use ratatui::text::{Line, Span};

const GAUGE_WIDTH: usize = 16;
const GAUGE_DANGER_THRESHOLD: f64 = 0.9;

/// Espace fine insécable — séparateur de milliers à la française.
const THIN_SPACE: char = '\u{202f}';

const DOCKER_COL_CAPS: [usize; 4] = [22, 30, 22, 26];

/// Formate un compte de tokens avec séparateur de milliers `THIN_SPACE`
/// (ex. `139 286`).
pub fn fmt_tokens(n: u64) -> String {
    let digits = n.to_string();
    let len = digits.len();
    let mut out = String::with_capacity(len + len / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (len - i).is_multiple_of(3) {
            out.push(THIN_SPACE);
        }
        out.push(c);
    }
    out
}

/// Part entière `part / whole` en pourcentage, arrondie — `0` si `whole` est
/// nul plutôt qu'une division par zéro.
pub fn pct(part: u64, whole: u64) -> u32 {
    if whole == 0 {
        return 0;
    }
    ((part as f64 / whole as f64) * 100.0).round() as u32
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Budget {
    Tokens(u64),
    Dollars(f64),
}

/// Parse une valeur d'env `KAJI_BUDGET_5H` / `KAJI_BUDGET_7J` : entier nu en
/// tokens (`500000`) ou dollars préfixés `$` (`$25`). `None` si vide ou
/// invalide.
pub fn parse_budget(raw: &str) -> Option<Budget> {
    let trimmed = raw.trim();
    if let Some(rest) = trimmed.strip_prefix('$') {
        return rest
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|v| *v >= 0.0)
            .map(Budget::Dollars);
    }
    trimmed.parse::<u64>().ok().map(Budget::Tokens)
}

/// Jauge texte de `width` cases, remplie selon `ratio` (borné à `[0, 1]`).
pub fn gauge(ratio: f64, width: usize) -> String {
    let clamped = ratio.clamp(0.0, 1.0);
    let filled = ((clamped * width as f64).round() as usize).min(width);
    format!("[{}{}]", "█".repeat(filled), "░".repeat(width - filled))
}

fn tok(n: i64) -> u64 {
    n.max(0) as u64
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{head}…")
}

/// Assemble une ligne paddée : la première colonne et la dernière ne sont
/// jamais paddées (label libre à gauche, contenu final libre à droite) ;
/// les colonnes intermédiaires sont alignées à droite ou à gauche selon
/// `right_align`, séparées par deux espaces, précédées d'un espace de marge.
fn format_row(cells: &[String], widths: &[usize], right_align: &[bool]) -> String {
    let mut out = String::from(" ");
    let last = cells.len() - 1;
    for (i, cell) in cells.iter().enumerate() {
        if i > 0 {
            out.push_str("  ");
        }
        if i == last {
            out.push_str(cell);
        } else if right_align[i] {
            out.push_str(&format!("{cell:>width$}", width = widths[i]));
        } else {
            out.push_str(&format!("{cell:<width$}", width = widths[i]));
        }
    }
    out
}

fn column_widths(headers: &[&str], rows: &[Vec<String>]) -> Vec<usize> {
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }
    widths
}

fn cost_cell(agg: &UsageAggregate) -> String {
    match agg.cost {
        Some(c) => format!("${c:.2}"),
        None => "n/a".to_string(),
    }
}

/// Construit le bloc `/cost` : titre, tableau session/5 h/7 j aligné, note
/// de coût indisponible si aucune fenêtre n'a de tarif connu, et lignes de
/// jauge budget si `budget_5h`/`budget_7j` sont fournis.
pub fn cost_table_lines(
    windows: &UsageWindows,
    provider: &str,
    model: &str,
    budget_5h: Option<Budget>,
    budget_7j: Option<Budget>,
) -> Vec<Line<'static>> {
    let session_total = tok(windows.session.total_tokens);
    let last_5h_total = tok(windows.last_5h.total_tokens);
    let last_7d_total = tok(windows.last_7d.total_tokens);

    let rows_data: [(&str, &UsageAggregate, String); 3] = [
        (
            "session",
            &windows.session,
            format!("{} % du 5 h", pct(session_total, last_5h_total)),
        ),
        (
            "5 h",
            &windows.last_5h,
            format!("{} % du 7 j", pct(last_5h_total, last_7d_total)),
        ),
        ("7 j", &windows.last_7d, "—".to_string()),
    ];

    let headers = ["fenêtre", "↑ entrée", "↓ sortie", "total", "coût", "part"];
    let right_align = [false, true, true, true, true, false];

    let rows: Vec<Vec<String>> = rows_data
        .iter()
        .map(|(label, agg, part)| {
            vec![
                (*label).to_string(),
                fmt_tokens(tok(agg.input_tokens)),
                fmt_tokens(tok(agg.output_tokens)),
                fmt_tokens(tok(agg.total_tokens)),
                cost_cell(agg),
                part.clone(),
            ]
        })
        .collect();

    let widths = column_widths(&headers, &rows);
    let header_cells: Vec<String> = headers.iter().map(|h| h.to_string()).collect();
    let header_row = format_row(&header_cells, &widths, &right_align);
    let rule_width = header_row.chars().count().saturating_sub(1);

    let mut lines = vec![
        Line::from(Span::styled(
            format!("/cost — {provider}/{model}"),
            theme::title(),
        )),
        Line::from(""),
        Line::from(Span::styled(header_row, theme::table_header())),
        Line::from(Span::styled(
            format!(" {}", "─".repeat(rule_width)),
            theme::border_inactive(),
        )),
    ];

    for row in &rows {
        lines.push(Line::from(Span::styled(
            format_row(row, &widths, &right_align),
            theme::text(),
        )));
    }

    let cost_unknown_everywhere = windows.session.cost.is_none()
        && windows.last_5h.cost.is_none()
        && windows.last_7d.cost.is_none();
    if cost_unknown_everywhere {
        lines.push(Line::from(Span::styled(
            "coût indisponible : provider sans tarification",
            theme::dim(),
        )));
    }

    for (label, budget, agg) in [
        ("5 h", budget_5h, &windows.last_5h),
        ("7 j", budget_7j, &windows.last_7d),
    ] {
        if let Some(budget) = budget {
            lines.push(budget_gauge_line(label, budget, agg));
        }
    }

    lines
}

fn budget_gauge_line(window_label: &str, budget: Budget, agg: &UsageAggregate) -> Line<'static> {
    let (used_display, total_display, unit, ratio) = match budget {
        Budget::Tokens(total) => {
            let used = tok(agg.total_tokens);
            let ratio = if total == 0 {
                0.0
            } else {
                used as f64 / total as f64
            };
            (fmt_tokens(used), fmt_tokens(total), "tokens", ratio)
        }
        Budget::Dollars(total) => match agg.cost {
            Some(cost) => {
                let ratio = if total <= 0.0 { 0.0 } else { cost / total };
                (format!("${cost:.2}"), format!("${total:.2}"), "", ratio)
            }
            None => {
                return Line::from(Span::styled(
                    format!(" budget {window_label} — coût indisponible pour ce budget"),
                    theme::dim(),
                ));
            }
        },
    };

    let bar = gauge(ratio, GAUGE_WIDTH);
    let bar_style = if ratio >= GAUGE_DANGER_THRESHOLD {
        Style::default().fg(theme::VERMILLON)
    } else {
        Style::default().fg(theme::OR_PATINE)
    };
    let percent = (ratio * 100.0).round() as i64;
    let detail = if unit.is_empty() {
        format!("  {percent} %  ({used_display} / {total_display})")
    } else {
        format!("  {percent} %  ({used_display} / {total_display} {unit})")
    };

    Line::from(vec![
        Span::styled(format!(" budget {window_label}  "), theme::dim()),
        Span::styled(bar, bar_style),
        Span::styled(detail, theme::text()),
    ])
}

/// Parse `docker ps --format "{{.Names}}\t{{.Image}}\t{{.Status}}\t{{.Ports}}"`
/// en tableau aligné NOM/IMAGE/STATUT/PORTS, colonnes tronquées avec `…` au
/// besoin.
pub fn docker_table_lines(raw_output: &str) -> Vec<Line<'static>> {
    let raw_rows: Vec<[String; 4]> = raw_output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| {
            let mut cols = line.split('\t');
            Some([
                cols.next()?.to_string(),
                cols.next()?.to_string(),
                cols.next()?.to_string(),
                cols.next().unwrap_or("").to_string(),
            ])
        })
        .collect();

    if raw_rows.is_empty() {
        return vec![Line::from(Span::styled(
            "docker : aucun conteneur en cours",
            theme::dim(),
        ))];
    }

    let headers = ["nom", "image", "statut", "ports"];
    let right_align = [false, false, false, false];

    let rows: Vec<Vec<String>> = raw_rows
        .iter()
        .map(|row| {
            row.iter()
                .enumerate()
                .map(|(i, cell)| truncate(cell, DOCKER_COL_CAPS[i]))
                .collect()
        })
        .collect();

    let widths = column_widths(&headers, &rows);
    let header_cells: Vec<String> = headers.iter().map(|h| h.to_string()).collect();
    let header_row = format_row(&header_cells, &widths, &right_align);
    let rule_width = header_row.chars().count().saturating_sub(1);

    let mut lines = vec![
        Line::from(Span::styled(header_row, theme::table_header())),
        Line::from(Span::styled(
            format!(" {}", "─".repeat(rule_width)),
            theme::border_inactive(),
        )),
    ];

    for row in &rows {
        lines.push(Line::from(Span::styled(
            format_row(row, &widths, &right_align),
            theme::text(),
        )));
    }

    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain_text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn plain_lines(lines: &[Line]) -> Vec<String> {
        lines.iter().map(plain_text).collect()
    }

    #[test]
    fn fmt_tokens_inserts_thin_space_every_three_digits() {
        assert_eq!(fmt_tokens(0), "0");
        assert_eq!(fmt_tokens(20), "20");
        assert_eq!(fmt_tokens(999), "999");
        assert_eq!(fmt_tokens(1_314), "1\u{202f}314");
        assert_eq!(fmt_tokens(34_242), "34\u{202f}242");
        assert_eq!(fmt_tokens(139_286), "139\u{202f}286");
        assert_eq!(fmt_tokens(9_904), "9\u{202f}904");
        assert_eq!(fmt_tokens(1_000_000), "1\u{202f}000\u{202f}000");
    }

    #[test]
    fn pct_rounds_and_treats_zero_denominator_as_zero() {
        assert_eq!(pct(0, 34_242), 0);
        assert_eq!(pct(34_242, 149_190), 23);
        assert_eq!(pct(100, 0), 0);
        assert_eq!(pct(1, 3), 33);
        assert_eq!(pct(2, 3), 67);
    }

    #[test]
    fn parse_budget_reads_bare_integer_as_tokens() {
        assert_eq!(parse_budget("500000"), Some(Budget::Tokens(500_000)));
        assert_eq!(parse_budget("  80000  "), Some(Budget::Tokens(80_000)));
    }

    #[test]
    fn parse_budget_reads_dollar_prefixed_value_as_dollars() {
        assert_eq!(parse_budget("$25"), Some(Budget::Dollars(25.0)));
        assert_eq!(parse_budget("$12.50"), Some(Budget::Dollars(12.5)));
    }

    #[test]
    fn parse_budget_rejects_garbage_and_negative_values() {
        assert_eq!(parse_budget(""), None);
        assert_eq!(parse_budget("abc"), None);
        assert_eq!(parse_budget("-5"), None);
        assert_eq!(parse_budget("$-5"), None);
    }

    #[test]
    fn gauge_fills_proportionally_to_ratio() {
        assert_eq!(gauge(0.0, 16), "[░░░░░░░░░░░░░░░░]");
        assert_eq!(gauge(1.0, 16), "[████████████████]");
        assert_eq!(gauge(0.5, 16), "[████████░░░░░░░░]");
        assert_eq!(gauge(0.42, 16), "[███████░░░░░░░░░]");
    }

    #[test]
    fn gauge_clamps_out_of_range_ratios() {
        assert_eq!(gauge(-1.0, 4), "[░░░░]");
        assert_eq!(gauge(5.0, 4), "[████]");
    }

    fn aggregate(input: i64, output: i64, cost: Option<f64>) -> UsageAggregate {
        UsageAggregate {
            input_tokens: input,
            output_tokens: output,
            total_tokens: input + output,
            cost,
        }
    }

    fn fixture_windows() -> UsageWindows {
        UsageWindows {
            session: aggregate(0, 0, None),
            last_5h: aggregate(32_928, 1_314, None),
            last_7d: aggregate(139_286, 9_904, None),
        }
    }

    #[test]
    fn cost_table_lines_renders_exact_aligned_table_on_fixed_data() {
        let windows = fixture_windows();
        let lines = cost_table_lines(
            &windows,
            "ollama_cloud",
            "deepseek-v4-flash:0731",
            None,
            None,
        );
        let text = plain_lines(&lines);

        assert_eq!(text[0], "/cost — ollama_cloud/deepseek-v4-flash:0731");
        assert_eq!(text[1], "");
        assert_eq!(text[2], " fenêtre  ↑ entrée  ↓ sortie    total  coût  part");
        assert_eq!(text[3], " ────────────────────────────────────────────────");
        assert_eq!(
            text[4],
            " session         0         0        0   n/a  0 % du 5 h"
        );
        assert_eq!(
            text[5],
            " 5 h        32\u{202f}928     1\u{202f}314   34\u{202f}242   n/a  23 % du 7 j"
        );
        assert_eq!(
            text[6],
            " 7 j       139\u{202f}286     9\u{202f}904  149\u{202f}190   n/a  —"
        );
        assert_eq!(text[7], "coût indisponible : provider sans tarification");
        assert_eq!(text.len(), 8);
    }

    #[test]
    fn cost_table_lines_shows_dollar_costs_and_no_na_footer_when_known() {
        let windows = UsageWindows {
            session: aggregate(100, 20, Some(0.10)),
            last_5h: aggregate(1_000, 200, Some(1.50)),
            last_7d: aggregate(9_000, 2_000, Some(12.00)),
        };
        let lines = cost_table_lines(&windows, "anthropic", "claude-sonnet", None, None);
        let text = plain_lines(&lines);
        assert!(text.iter().any(|l| l.contains("$0.10")));
        assert!(text.iter().any(|l| l.contains("$1.50")));
        assert!(text.iter().any(|l| l.contains("$12.00")));
        assert!(!text.iter().any(|l| l.contains("indisponible")));
    }

    #[test]
    fn cost_table_lines_appends_token_budget_gauge_line() {
        let windows = fixture_windows();
        let lines = cost_table_lines(
            &windows,
            "ollama_cloud",
            "deepseek-v4-flash:0731",
            Some(Budget::Tokens(80_000)),
            None,
        );
        let text = plain_lines(&lines);
        let gauge_line = text
            .iter()
            .find(|l| l.contains("budget 5 h"))
            .expect("budget line present");
        assert!(gauge_line.contains("43 %"));
        assert!(gauge_line.contains("34\u{202f}242 / 80\u{202f}000 tokens"));
    }

    #[test]
    fn cost_table_lines_dollar_budget_reports_unavailable_when_cost_unknown() {
        let windows = fixture_windows();
        let lines = cost_table_lines(
            &windows,
            "ollama_cloud",
            "deepseek-v4-flash:0731",
            None,
            Some(Budget::Dollars(25.0)),
        );
        let text = plain_lines(&lines);
        assert!(text
            .iter()
            .any(|l| l.contains("budget 7 j") && l.contains("indisponible")));
    }

    #[test]
    fn docker_table_lines_renders_exact_aligned_table_from_fixture() {
        let fixture =
            "web\tnginx:latest\tUp 3 hours\t0.0.0.0:80->80/tcp\ndb\tpostgres:16\tUp 3 hours\t";
        let lines = docker_table_lines(fixture);
        let text = plain_lines(&lines);
        assert_eq!(text[0], " nom  image         statut      ports");
        assert_eq!(text[1], " ────────────────────────────────────");
        assert_eq!(
            text[2],
            " web  nginx:latest  Up 3 hours  0.0.0.0:80->80/tcp"
        );
        assert_eq!(text[3], " db   postgres:16   Up 3 hours  ");
    }

    #[test]
    fn docker_table_lines_empty_output_reports_no_containers() {
        let lines = docker_table_lines("");
        assert_eq!(
            plain_lines(&lines),
            vec!["docker : aucun conteneur en cours"]
        );
        let lines = docker_table_lines("\n\n");
        assert_eq!(
            plain_lines(&lines),
            vec!["docker : aucun conteneur en cours"]
        );
    }

    #[test]
    fn docker_table_lines_truncates_long_columns_with_ellipsis() {
        let long_name = "a".repeat(40);
        let fixture = format!("{long_name}\timg\tUp\t");
        let lines = docker_table_lines(&fixture);
        let text = plain_lines(&lines);
        let data_row = &text[2];
        let name_cell = data_row.trim_start().split("  ").next().unwrap();
        assert_eq!(name_cell.chars().count(), DOCKER_COL_CAPS[0]);
        assert!(name_cell.ends_with('…'));
    }
}
