//! Tableaux alignés à largeur fixe pour les blocs système `/cost` et
//! `/docker` — construits par `format!` paddé plutôt que via un widget
//! `Table` ratatui, pour rester une simple liste de lignes insérable dans le
//! flux du chat.

use crate::tui::app::{RoledLine, RoledSpan};
use kaji::agents::ContextBreakdown;
use kaji::context_mgmt::condense::CondenseTotals;
use kaji::metrics::budget::{BudgetLevel, BudgetStatus};
use kaji::metrics::{BurnReport, MetricsReport, MetricsRow, MetricsWindow};
use kaji::session::{UsageAggregate, UsageWindows};

const GAUGE_WIDTH: usize = 16;
const GAUGE_DANGER_THRESHOLD: f64 = 0.9;
const CONTEXT_GAUGE_WIDTH: usize = 30;
const CONTEXT_CATEGORY_GAUGE_WIDTH: usize = 10;
const CONTEXT_LABEL_WIDTH: usize = 10;

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
) -> Vec<RoledLine> {
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
        vec![RoledSpan::title(format!("/cost — {provider}/{model}"))],
        vec![RoledSpan::plain("")],
        vec![RoledSpan::table_header(header_row)],
        vec![RoledSpan::border_inactive(format!(
            " {}",
            "─".repeat(rule_width)
        ))],
    ];

    for row in &rows {
        lines.push(vec![RoledSpan::text(format_row(
            row,
            &widths,
            &right_align,
        ))]);
    }

    let cost_unknown_everywhere = windows.session.cost.is_none()
        && windows.last_5h.cost.is_none()
        && windows.last_7d.cost.is_none();
    if cost_unknown_everywhere {
        lines.push(vec![RoledSpan::dim(
            "coût indisponible : provider sans tarification",
        )]);
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

fn budget_gauge_line(window_label: &str, budget: Budget, agg: &UsageAggregate) -> RoledLine {
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
                return vec![RoledSpan::dim(format!(
                    " budget {window_label} — coût indisponible pour ce budget"
                ))];
            }
        },
    };

    let bar = gauge(ratio, GAUGE_WIDTH);
    let bar_span = if ratio >= GAUGE_DANGER_THRESHOLD {
        RoledSpan::accent(bar)
    } else {
        RoledSpan::gold(bar)
    };
    let percent = (ratio * 100.0).round() as i64;
    let detail = if unit.is_empty() {
        format!("  {percent} %  ({used_display} / {total_display})")
    } else {
        format!("  {percent} %  ({used_display} / {total_display} {unit})")
    };

    vec![
        RoledSpan::dim(format!(" budget {window_label}  ")),
        bar_span,
        RoledSpan::text(detail),
    ]
}

/// Vues de `/cost`. `Windows` est la vue historique (session / 5 h / 7 j) ;
/// les autres tapent dans le ledger via `kaji::metrics`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CostView {
    Windows,
    Models,
    Day,
    Week,
    Month,
    Cache,
    Projection,
}

impl CostView {
    /// `None` quand l'argument ne nomme aucune vue — l'appelant affiche
    /// l'usage plutôt que de retomber silencieusement sur la vue par défaut.
    pub fn parse(arg: &str) -> Option<Self> {
        let trimmed = arg.trim();
        if trimmed.is_empty() {
            return Some(CostView::Windows);
        }
        match trimmed.to_lowercase().as_str() {
            "modèles" | "modeles" | "models" | "modèle" | "modele" | "model" => {
                Some(CostView::Models)
            }
            "jour" | "day" => Some(CostView::Day),
            "semaine" | "week" => Some(CostView::Week),
            "mois" | "month" => Some(CostView::Month),
            "cache" => Some(CostView::Cache),
            "projection" | "proj" => Some(CostView::Projection),
            _ => None,
        }
    }

    /// Fenêtre du ledger interrogée par la vue. `Projection` n'en a pas : le
    /// burn report porte ses propres bornes.
    pub fn window(self) -> Option<MetricsWindow> {
        match self {
            CostView::Windows | CostView::Projection => None,
            CostView::Models => Some(MetricsWindow::Last7d),
            CostView::Day => Some(MetricsWindow::Day),
            CostView::Week => Some(MetricsWindow::Week),
            CostView::Month | CostView::Cache => Some(MetricsWindow::Month),
        }
    }

    pub fn usage() -> &'static str {
        "usage : /cost [modèles|jour|semaine|mois|cache|projection]"
    }
}

fn cost_str(cost: Option<f64>) -> String {
    match cost {
        Some(c) => format!("${c:.2}"),
        None => "n/a".to_string(),
    }
}

/// Rend un tableau en lignes de texte nu : en-tête, filet, corps. Partagé par
/// le bloc TUI (qui restyle chaque ligne) et `kaji metrics --format table`
/// (qui les imprime telles quelles), pour que les deux surfaces ne divergent
/// jamais sur les colonnes.
pub fn render_table(headers: &[&str], rows: &[Vec<String>], right_align: &[bool]) -> Vec<String> {
    let widths = column_widths(headers, rows);
    let header_cells: Vec<String> = headers.iter().map(|h| (*h).to_string()).collect();
    let header_row = format_row(&header_cells, &widths, right_align);
    let rule_width = header_row.chars().count().saturating_sub(1);

    let mut lines = vec![header_row, format!(" {}", "─".repeat(rule_width))];
    for row in rows {
        lines.push(format_row(row, &widths, right_align));
    }
    lines
}

const METRICS_HEADERS: [&str; 6] = ["clé", "↑ entrée", "↓ sortie", "total", "coût", "appels"];
const METRICS_ALIGN: [bool; 6] = [false, true, true, true, true, true];

const CACHE_HEADERS: [&str; 6] = ["clé", "↑ entrée", "cache lu", "hit", "coût", "économisé"];
const CACHE_ALIGN: [bool; 6] = [false, true, true, true, true, true];

/// Lignes du tableau d'agrégats — clé, tokens, coût, appels — totaux inclus
/// en dernière ligne dès qu'il y a plus d'un groupe.
pub fn metrics_rows(report: &MetricsReport) -> Vec<Vec<String>> {
    let row_cells = |row: &MetricsRow| {
        vec![
            truncate(&row.key, 40),
            fmt_tokens(tok(row.input_tokens)),
            fmt_tokens(tok(row.output_tokens)),
            fmt_tokens(tok(row.total_tokens)),
            cost_str(row.cost),
            row.entries.to_string(),
        ]
    };
    let mut rows: Vec<Vec<String>> = report.rows.iter().map(row_cells).collect();
    if report.rows.len() > 1 {
        rows.push(row_cells(&report.totals));
    }
    rows
}

/// Lignes du tableau cache — la colonne `économisé` est un plancher : les
/// groupes sans tarif connu comptent pour zéro.
pub fn cache_rows(report: &MetricsReport) -> Vec<Vec<String>> {
    let row_cells = |row: &MetricsRow| {
        vec![
            truncate(&row.key, 40),
            fmt_tokens(tok(row.input_tokens)),
            fmt_tokens(tok(row.cache_read_tokens)),
            format!("{} %", (row.cache_hit_rate() * 100.0).round() as i64),
            cost_str(row.cost),
            cost_str(row.cache_savings),
        ]
    };
    let mut rows: Vec<Vec<String>> = report.rows.iter().map(row_cells).collect();
    if report.rows.len() > 1 {
        rows.push(row_cells(&report.totals));
    }
    rows
}

pub fn metrics_headers(cache: bool) -> (&'static [&'static str], &'static [bool]) {
    if cache {
        (&CACHE_HEADERS, &CACHE_ALIGN)
    } else {
        (&METRICS_HEADERS, &METRICS_ALIGN)
    }
}

fn styled_table(headers: &[&str], rows: &[Vec<String>], right_align: &[bool]) -> Vec<RoledLine> {
    let mut lines = Vec::new();
    for (index, text) in render_table(headers, rows, right_align)
        .into_iter()
        .enumerate()
    {
        lines.push(match index {
            0 => vec![RoledSpan::table_header(text)],
            1 => vec![RoledSpan::border_inactive(text)],
            _ => vec![RoledSpan::text(text)],
        });
    }
    lines
}

/// Bloc `/cost <vue>` pour toute vue tabulaire (modèles, jour, semaine, mois,
/// cache).
pub fn metrics_table_lines(view: CostView, report: &MetricsReport) -> Vec<RoledLine> {
    let cache = view == CostView::Cache;
    let (headers, align) = metrics_headers(cache);
    let rows = if cache {
        cache_rows(report)
    } else {
        metrics_rows(report)
    };

    let title = if cache {
        format!(
            "/cost cache — {} (par {})",
            report.window.label(),
            report.dimension.label()
        )
    } else {
        format!(
            "/cost {} — par {}",
            report.window.label(),
            report.dimension.label()
        )
    };

    let mut lines = vec![vec![RoledSpan::title(title)], vec![RoledSpan::plain("")]];
    if report.rows.is_empty() {
        lines.push(vec![RoledSpan::dim("aucune consommation sur la fenêtre")]);
        return lines;
    }
    lines.extend(styled_table(headers, &rows, align));

    if cache {
        if let Some(full) = report.totals.cost_uncached() {
            lines.push(vec![RoledSpan::dim(format!(
                "sans cache : ${full:.2} — le cache a épargné {}",
                cost_str(report.totals.cache_savings)
            ))]);
        } else {
            lines.push(vec![RoledSpan::dim(
                "économie chiffrable seulement pour les modèles tarifés",
            )]);
        }
    }
    lines
}

/// Bloc `/cost projection` : burn du jour et de la semaine, dépense du mois,
/// projection de fin de mois et jauges de budget.
pub fn projection_lines(burn: &BurnReport) -> Vec<RoledLine> {
    let projection = &burn.projection;
    let mut lines = vec![
        vec![RoledSpan::title("/cost projection — mois en cours")],
        vec![RoledSpan::plain("")],
    ];

    let rows = vec![
        vec!["aujourd'hui".to_string(), format!("${:.2}", burn.today)],
        vec!["semaine".to_string(), format!("${:.2}", burn.week)],
        vec![
            format!(
                "mois (J{}/{})",
                projection.elapsed_days, projection.days_in_month
            ),
            format!("${:.2}", burn.month),
        ],
        vec![
            "rythme".to_string(),
            format!("${:.2} / jour", projection.daily_rate),
        ],
        vec![
            "projection fin de mois".to_string(),
            format!("${:.2}", projection.month_end),
        ],
    ];
    lines.extend(styled_table(&["", "USD"], &rows, &[false, true]));

    if projection.elapsed_days < 2 {
        lines.push(vec![RoledSpan::dim(
            "projection extrapolée d'un seul jour — indicative",
        )]);
    }

    for status in &burn.budgets {
        lines.push(budget_status_line(status));
    }
    lines
}

fn budget_span(level: BudgetLevel, bar: String) -> RoledSpan {
    match level {
        BudgetLevel::Over | BudgetLevel::High => RoledSpan::accent(bar),
        _ => RoledSpan::gold(bar),
    }
}

/// Jauge d'un budget mensuel, quel que soit son niveau.
pub fn budget_status_line(status: &BudgetStatus) -> RoledLine {
    let percent = (status.ratio * 100.0).round() as i64;
    vec![
        RoledSpan::dim(format!(" budget {} ", status.scope)),
        budget_span(status.level, gauge(status.ratio, GAUGE_WIDTH)),
        RoledSpan::text(format!(
            "  {percent} %  (${:.2} / ${:.2} ce mois)",
            status.spent, status.limit
        )),
    ]
}

/// Avertissements de budget : une ligne par seuil franchi, aucune sinon.
/// Jamais un arrêt — le user garde la main.
pub fn budget_warning_lines(statuses: &[BudgetStatus]) -> Vec<RoledLine> {
    statuses
        .iter()
        .filter(|status| status.breached())
        .map(|status| {
            let threshold = status.level.threshold().unwrap_or(100);
            let percent = (status.ratio * 100.0).round() as i64;
            vec![
                budget_span(status.level, format!("budget {} ", status.scope)),
                RoledSpan::text(format!(
                    "— {threshold} % franchi : ${:.2} / ${:.2} ce mois ({percent} %)",
                    status.spent, status.limit
                )),
            ]
        })
        .collect()
}

/// Ligne de synthèse condense — `None` si aucun résultat d'outil n'a jamais
/// été condensé. Le compte de tokens est une estimation cumulative : les
/// mêmes résultats condensés sont recomptés à chaque appel provider tant
/// qu'ils restent hors de la fenêtre de fraîcheur (voir
/// `condense::totals`), donc ce nombre reflète l'économie réalisée sur
/// l'ensemble de la session, pas un total de résultats uniques.
pub fn condense_line(totals: &CondenseTotals) -> Option<RoledLine> {
    if totals.results_touched == 0 {
        return None;
    }
    let saved_tokens = totals.bytes_before.saturating_sub(totals.bytes_after) / 4;
    Some(vec![RoledSpan::dim(format!(
        "condensé : {} résultats · ~{} tok d'historique non envoyés (cumul, est.)",
        totals.results_touched,
        fmt_tokens(saved_tokens)
    ))])
}

/// Construit le bloc `/context` : jauge d'occupation globale, une ligne par
/// catégorie (tokens, part de la limite, mini-jauge), le reste libre et le
/// dernier total rapporté par le provider s'il existe.
pub fn context_table_lines(
    breakdown: &ContextBreakdown,
    provider: &str,
    model: &str,
) -> Vec<RoledLine> {
    let limit = breakdown.limit as u64;
    let used = breakdown.used as u64;
    let ratio = if limit == 0 {
        0.0
    } else {
        used as f64 / limit as f64
    };
    // `compaction_threshold_pct == 0` is the "auto-compact off" sentinel: no
    // target to announce, and no threshold to colour the bar against.
    let auto_compact = (breakdown.compaction_threshold_pct > 0).then(|| {
        format!(
            " · auto-compact à {} ({} %)",
            fmt_tokens(breakdown.compact_at() as u64),
            breakdown.compaction_threshold_pct
        )
    });
    let bar = gauge(ratio, CONTEXT_GAUGE_WIDTH);
    let bar_span = match &auto_compact {
        Some(_) if breakdown.used_pct() >= breakdown.compaction_threshold_pct => {
            RoledSpan::accent(bar)
        }
        _ => RoledSpan::gold(bar),
    };

    let mut lines = vec![
        vec![RoledSpan::title(format!("/context — {provider}/{model}"))],
        vec![RoledSpan::plain("")],
        vec![
            RoledSpan::plain(" "),
            bar_span,
            RoledSpan::text(format!(
                " {} / {} ({} %){}",
                fmt_tokens(used),
                fmt_tokens(limit),
                breakdown.used_pct(),
                auto_compact
                    .as_deref()
                    .unwrap_or(" · auto-compact désactivé")
            )),
        ],
        vec![RoledSpan::plain("")],
    ];

    for (label, tokens) in [
        ("système", breakdown.system),
        ("hints", breakdown.hints),
        ("skills", breakdown.skills),
        ("outils", breakdown.tools),
        ("mcp", breakdown.mcp),
        ("mémoire", breakdown.memory),
        ("messages", breakdown.messages),
    ] {
        lines.push(context_category_line(label, tokens as u64, limit));
    }

    lines.push(vec![RoledSpan::text(format!(
        " {:<CONTEXT_LABEL_WIDTH$} {:>8}",
        "libre",
        fmt_tokens(breakdown.free() as u64)
    ))]);

    if let Some(last_reported) = breakdown.last_reported {
        lines.push(vec![RoledSpan::dim(format!(
            "dernier total rapporté par le provider : {}",
            fmt_tokens(last_reported as u64)
        ))]);
    }

    lines.push(vec![RoledSpan::dim(
        "estimation tokenizer o200k — les chiffres exacts sont ceux du provider",
    )]);

    lines
}

/// Une catégorie vide n'a ni part ni jauge à montrer : `—` dit « rien ici »
/// sans imposer une barre à zéro parmi celles qui portent du sens.
fn context_category_line(label: &str, tokens: u64, limit: u64) -> RoledLine {
    if tokens == 0 {
        return vec![RoledSpan::dim(format!(
            " {label:<CONTEXT_LABEL_WIDTH$} {:>8}",
            "—"
        ))];
    }
    let ratio = if limit == 0 {
        0.0
    } else {
        tokens as f64 / limit as f64
    };
    vec![RoledSpan::text(format!(
        " {label:<CONTEXT_LABEL_WIDTH$} {:>8} {:>3} %  {}",
        fmt_tokens(tokens),
        pct(tokens, limit),
        gauge(ratio, CONTEXT_CATEGORY_GAUGE_WIDTH)
    ))]
}

/// Parse `docker ps --format "{{.Names}}\t{{.Image}}\t{{.Status}}\t{{.Ports}}"`
/// en tableau aligné NOM/IMAGE/STATUT/PORTS, colonnes tronquées avec `…` au
/// besoin.
pub fn docker_table_lines(raw_output: &str) -> Vec<RoledLine> {
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
        return vec![vec![RoledSpan::dim("docker : aucun conteneur en cours")]];
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
        vec![RoledSpan::table_header(header_row)],
        vec![RoledSpan::border_inactive(format!(
            " {}",
            "─".repeat(rule_width)
        ))],
    ];

    for row in &rows {
        lines.push(vec![RoledSpan::text(format_row(
            row,
            &widths,
            &right_align,
        ))]);
    }

    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain_text(line: &RoledLine) -> String {
        line.iter().map(|span| span.text.as_str()).collect()
    }

    fn plain_lines(lines: &[RoledLine]) -> Vec<String> {
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
    fn condense_line_is_none_when_nothing_touched() {
        let totals = CondenseTotals {
            results_touched: 0,
            bytes_before: 1_000,
            bytes_after: 100,
        };
        assert_eq!(condense_line(&totals), None);
    }

    #[test]
    fn condense_line_reports_count_and_estimated_tokens_saved() {
        let totals = CondenseTotals {
            results_touched: 3,
            bytes_before: 4_400,
            bytes_after: 400,
        };
        let line = condense_line(&totals).expect("line present");
        assert_eq!(
            plain_text(&line),
            "condensé : 3 résultats · ~1\u{202f}000 tok d'historique non envoyés (cumul, est.)"
        );
    }

    fn fixture_breakdown() -> ContextBreakdown {
        ContextBreakdown {
            system: 4_200,
            hints: 1_100,
            skills: 0,
            mcp: 2_500,
            tools: 3_300,
            memory: 0,
            messages: 9_000,
            used: 20_100,
            limit: 200_000,
            last_reported: Some(18_742),
            compaction_threshold_pct: 60,
        }
    }

    #[test]
    fn context_table_lines_renders_title_gauge_and_every_category() {
        let lines = context_table_lines(&fixture_breakdown(), "anthropic", "claude-sonnet");
        let text = plain_lines(&lines);

        assert_eq!(text[0], "/context — anthropic/claude-sonnet");
        assert_eq!(text[1], "");
        assert_eq!(
            text[2],
            " [███░░░░░░░░░░░░░░░░░░░░░░░░░░░] 20\u{202f}100 / 200\u{202f}000 (10 %) · auto-compact à 120\u{202f}000 (60 %)"
        );
        assert_eq!(text[3], "");
        assert_eq!(text[4], " système       4\u{202f}200   2 %  [░░░░░░░░░░]");
        assert_eq!(text[5], " hints         1\u{202f}100   1 %  [░░░░░░░░░░]");
        assert_eq!(text[6], " skills            —");
        assert_eq!(text[7], " outils        3\u{202f}300   2 %  [░░░░░░░░░░]");
        assert_eq!(text[8], " mcp           2\u{202f}500   1 %  [░░░░░░░░░░]");
        assert_eq!(text[9], " mémoire           —");
        assert_eq!(text[10], " messages      9\u{202f}000   5 %  [░░░░░░░░░░]");
        assert_eq!(text[11], " libre       179\u{202f}900");
        assert_eq!(
            text[12],
            "dernier total rapporté par le provider : 18\u{202f}742"
        );
        assert_eq!(
            text[13],
            "estimation tokenizer o200k — les chiffres exacts sont ceux du provider"
        );
        assert_eq!(text.len(), 14);
    }

    #[test]
    fn context_table_lines_announces_no_target_when_auto_compact_is_disabled() {
        let mut breakdown = fixture_breakdown();
        breakdown.compaction_threshold_pct = 0;
        let text = plain_lines(&context_table_lines(
            &breakdown,
            "anthropic",
            "claude-sonnet",
        ));

        assert_eq!(
            text[2],
            " [███░░░░░░░░░░░░░░░░░░░░░░░░░░░] 20\u{202f}100 / 200\u{202f}000 (10 %) · auto-compact désactivé"
        );
    }

    #[test]
    fn context_table_lines_omits_the_provider_total_when_never_reported() {
        let mut breakdown = fixture_breakdown();
        breakdown.last_reported = None;
        let text = plain_lines(&context_table_lines(&breakdown, "ollama", "qwen"));
        assert!(!text.iter().any(|l| l.contains("dernier total")));
        assert!(text.last().unwrap().starts_with("estimation tokenizer"));
    }

    #[test]
    fn context_table_lines_keep_gauges_at_their_declared_width_when_saturated() {
        let mut breakdown = fixture_breakdown();
        breakdown.messages = 500_000;
        breakdown.used = 512_100;
        let text = plain_lines(&context_table_lines(&breakdown, "ollama", "qwen"));

        assert!(text[2].contains(&"█".repeat(CONTEXT_GAUGE_WIDTH)));
        assert!(text[2].contains("(100 %)"));
        let messages_row = &text[10];
        assert!(messages_row.contains(&format!("[{}]", "█".repeat(CONTEXT_CATEGORY_GAUGE_WIDTH))));
        assert!(messages_row.contains("250 %"));
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

    fn metrics_row(key: &str, input: i64, cache_read: i64, cost: Option<f64>) -> MetricsRow {
        MetricsRow {
            key: key.to_string(),
            input_tokens: input,
            output_tokens: 100,
            total_tokens: input + 100,
            cache_read_tokens: cache_read,
            cache_write_tokens: 0,
            entries: 2,
            cost,
            cache_savings: cost.map(|_| 0.25),
        }
    }

    fn fixture_report(window: MetricsWindow) -> MetricsReport {
        let rows = vec![
            metrics_row("claude-sonnet", 10_000, 4_000, Some(1.50)),
            metrics_row("claude-haiku", 2_000, 0, Some(0.10)),
        ];
        let totals = MetricsRow {
            key: "total".to_string(),
            input_tokens: 12_000,
            output_tokens: 200,
            total_tokens: 12_200,
            cache_read_tokens: 4_000,
            cache_write_tokens: 0,
            entries: 4,
            cost: Some(1.60),
            cache_savings: Some(0.50),
        };
        MetricsReport {
            window,
            dimension: kaji::metrics::MetricsDimension::Model,
            start: 0,
            end: 1,
            rows,
            totals,
        }
    }

    #[test]
    fn cost_view_parses_the_six_named_views_and_rejects_the_rest() {
        assert_eq!(CostView::parse(""), Some(CostView::Windows));
        assert_eq!(CostView::parse("modèles"), Some(CostView::Models));
        assert_eq!(CostView::parse("models"), Some(CostView::Models));
        assert_eq!(CostView::parse("jour"), Some(CostView::Day));
        assert_eq!(CostView::parse("Semaine"), Some(CostView::Week));
        assert_eq!(CostView::parse("mois"), Some(CostView::Month));
        assert_eq!(CostView::parse("cache"), Some(CostView::Cache));
        assert_eq!(CostView::parse("proj"), Some(CostView::Projection));
        assert_eq!(CostView::parse("bidule"), None);
    }

    #[test]
    fn cost_view_windows_match_their_calendar_span() {
        assert_eq!(CostView::Windows.window(), None);
        assert_eq!(CostView::Projection.window(), None);
        assert_eq!(CostView::Models.window(), Some(MetricsWindow::Last7d));
        assert_eq!(CostView::Day.window(), Some(MetricsWindow::Day));
        assert_eq!(CostView::Week.window(), Some(MetricsWindow::Week));
        assert_eq!(CostView::Month.window(), Some(MetricsWindow::Month));
        assert_eq!(CostView::Cache.window(), Some(MetricsWindow::Month));
    }

    #[test]
    fn metrics_table_lines_render_one_row_per_key_plus_totals() {
        let lines = metrics_table_lines(CostView::Month, &fixture_report(MetricsWindow::Month));
        let text = plain_lines(&lines);
        assert_eq!(text[0], "/cost mois — par modèle");
        assert!(text[2].contains("↑ entrée"), "en-tête : {}", text[2]);
        assert!(text[4].contains("claude-sonnet") && text[4].contains("$1.50"));
        assert!(text[5].contains("claude-haiku"));
        assert!(
            text[6].contains("total") && text[6].contains("$1.60"),
            "ligne de totaux : {}",
            text[6]
        );
    }

    #[test]
    fn metrics_table_lines_report_an_empty_window_instead_of_an_empty_table() {
        let mut report = fixture_report(MetricsWindow::Day);
        report.rows.clear();
        let text = plain_lines(&metrics_table_lines(CostView::Day, &report));
        assert_eq!(text[2], "aucune consommation sur la fenêtre");
    }

    #[test]
    fn cache_view_shows_hit_rate_and_the_full_price_footer() {
        let text = plain_lines(&metrics_table_lines(
            CostView::Cache,
            &fixture_report(MetricsWindow::Month),
        ));
        assert!(text[0].starts_with("/cost cache — mois"));
        assert!(text[2].contains("hit"), "colonnes cache : {}", text[2]);
        // 4 000 lus sur 10 000 d'entrée.
        assert!(text[4].contains("40 %"), "taux de hit : {}", text[4]);
        assert!(
            text.last().unwrap().contains("sans cache : $2.10"),
            "pied de page : {:?}",
            text.last()
        );
    }

    #[test]
    fn cache_view_says_so_when_no_model_is_priced() {
        let mut report = fixture_report(MetricsWindow::Month);
        report.totals.cache_savings = None;
        for row in &mut report.rows {
            row.cache_savings = None;
        }
        let text = plain_lines(&metrics_table_lines(CostView::Cache, &report));
        assert_eq!(
            text.last().unwrap(),
            "économie chiffrable seulement pour les modèles tarifés"
        );
    }

    fn fixture_burn(daily: Vec<f64>, budgets: Vec<BudgetStatus>) -> BurnReport {
        let projection = kaji::metrics::projection::project_month_end(&daily, 30);
        BurnReport {
            today: *daily.last().unwrap_or(&0.0),
            week: daily.iter().sum(),
            month: daily.iter().sum(),
            daily,
            projection,
            budgets,
        }
    }

    #[test]
    fn projection_view_lists_burn_then_the_month_end_estimate() {
        let text = plain_lines(&projection_lines(&fixture_burn(vec![2.0; 10], Vec::new())));
        assert_eq!(text[0], "/cost projection — mois en cours");
        assert!(text.iter().any(|l| l.contains("mois (J10/30)")));
        assert!(
            text.iter().any(|l| l.contains("$60.00")),
            "projection : {text:?}"
        );
        assert!(
            !text.iter().any(|l| l.contains("indicative")),
            "10 jours écoulés : pas d'avertissement"
        );
    }

    #[test]
    fn projection_view_flags_a_single_day_extrapolation() {
        let text = plain_lines(&projection_lines(&fixture_burn(vec![3.0], Vec::new())));
        assert!(text.iter().any(|l| l.contains("indicative")));
    }

    #[test]
    fn projection_view_appends_a_gauge_per_declared_budget() {
        let burn = fixture_burn(
            vec![10.0; 5],
            vec![
                BudgetStatus::new("global", 100.0, 50.0),
                BudgetStatus::new("anthropic", 40.0, 50.0),
            ],
        );
        let text = plain_lines(&projection_lines(&burn));
        assert!(text.iter().any(|l| l.contains("budget global")));
        assert!(text
            .iter()
            .any(|l| l.contains("budget anthropic") && l.contains("125 %")));
    }

    #[test]
    fn budget_warnings_fire_only_on_breached_thresholds() {
        let statuses = [
            BudgetStatus::new("global", 100.0, 10.0),
            BudgetStatus::new("half", 100.0, 55.0),
            BudgetStatus::new("high", 100.0, 85.0),
            BudgetStatus::new("over", 100.0, 130.0),
        ];
        let text = plain_lines(&budget_warning_lines(&statuses));
        assert_eq!(text.len(), 3, "le budget à 10 % ne dit rien : {text:?}");
        assert!(text[0].contains("half") && text[0].contains("50 % franchi"));
        assert!(text[1].contains("high") && text[1].contains("80 % franchi"));
        assert!(text[2].contains("over") && text[2].contains("100 % franchi"));
    }

    #[test]
    fn render_table_pads_columns_to_the_widest_cell() {
        let rows = vec![
            vec!["a".to_string(), "1".to_string()],
            vec!["longue".to_string(), "22".to_string()],
        ];
        let lines = render_table(&["clé", "n"], &rows, &[false, true]);
        assert_eq!(lines.len(), 4, "en-tête, filet, 2 lignes");
        assert!(lines[1].contains('─'));
        assert!(lines[2].starts_with(" a      "), "padding : {:?}", lines[2]);
    }
}
