//! `kaji metrics` : agrégats tokens/coûts du `usage_ledger`, pour un œil
//! humain (`--format table`) ou pour un script/cron (`--format json`).
//!
//! Lecture seule et hors boucle agent : rien n'entre dans un prompt, donc
//! aucune capture replay n'est requise — l'horloge locale peut être lue
//! directement pour les bornes calendaires.

use anyhow::Result;
use kaji::metrics::{self, MetricsDimension, MetricsWindow};
use kaji::session::SessionManager;
use serde::Serialize;

use crate::tui::report::{metrics_headers, metrics_rows, render_table};

#[derive(clap::ValueEnum, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MetricsWindowArg {
    #[default]
    Day,
    Week,
    Month,
    #[value(name = "5h")]
    FiveHours,
    #[value(name = "7d")]
    SevenDays,
}

impl From<MetricsWindowArg> for MetricsWindow {
    fn from(arg: MetricsWindowArg) -> Self {
        match arg {
            MetricsWindowArg::Day => MetricsWindow::Day,
            MetricsWindowArg::Week => MetricsWindow::Week,
            MetricsWindowArg::Month => MetricsWindow::Month,
            MetricsWindowArg::FiveHours => MetricsWindow::Last5h,
            MetricsWindowArg::SevenDays => MetricsWindow::Last7d,
        }
    }
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MetricsDimensionArg {
    #[default]
    Model,
    Provider,
    Session,
    Project,
}

impl From<MetricsDimensionArg> for MetricsDimension {
    fn from(arg: MetricsDimensionArg) -> Self {
        match arg {
            MetricsDimensionArg::Model => MetricsDimension::Model,
            MetricsDimensionArg::Provider => MetricsDimension::Provider,
            MetricsDimensionArg::Session => MetricsDimension::Session,
            MetricsDimensionArg::Project => MetricsDimension::Project,
        }
    }
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MetricsFormat {
    #[default]
    Table,
    Json,
}

/// Sortie `--format json`. Champs stables : les agrégats, le burn du mois et
/// les budgets déclarés — de quoi alimenter un cron sans reparser un tableau.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct MetricsJson<'a> {
    window: MetricsWindow,
    dimension: MetricsDimension,
    start: i64,
    end: i64,
    rows: &'a [metrics::MetricsRow],
    totals: &'a metrics::MetricsRow,
    cache_hit_rate: f64,
    cache_savings: Option<f64>,
    burn: &'a metrics::BurnReport,
}

/// Hors boucle agent : `kaji metrics` ne fait que relire le ledger, rien
/// n'entre dans un prompt, donc l'horloge locale se lit directement plutôt que
/// via `PromptClock`.
fn now_local() -> chrono::DateTime<chrono::Local> {
    chrono::Local::now()
}

pub async fn handle_metrics_subcommand(
    window: MetricsWindowArg,
    by: MetricsDimensionArg,
    format: MetricsFormat,
) -> Result<()> {
    let session_manager = SessionManager::instance();
    let now = now_local();
    let window: MetricsWindow = window.into();
    let dimension: MetricsDimension = by.into();

    let report = metrics::report(&session_manager, window, dimension, now).await?;
    let burn = metrics::burn(&session_manager, now).await?;

    match format {
        MetricsFormat::Json => {
            let payload = MetricsJson {
                window,
                dimension,
                start: report.start,
                end: report.end,
                rows: &report.rows,
                totals: &report.totals,
                cache_hit_rate: report.totals.cache_hit_rate(),
                cache_savings: report.totals.cache_savings,
                burn: &burn,
            };
            println!("{}", serde_json::to_string_pretty(&payload)?);
        }
        MetricsFormat::Table => {
            println!(
                "kaji metrics — {} · par {}",
                window.label(),
                dimension.label()
            );
            if report.rows.is_empty() {
                println!("aucune consommation sur la fenêtre");
            } else {
                let (headers, align) = metrics_headers(false);
                for line in render_table(headers, &metrics_rows(&report), align) {
                    println!("{line}");
                }
            }
            println!();
            println!(
                " cache : {} % de hit · {} économisés",
                (report.totals.cache_hit_rate() * 100.0).round() as i64,
                match report.totals.cache_savings {
                    Some(saved) => format!("${saved:.2}"),
                    None => "n/a".to_string(),
                }
            );
            println!(
                " burn : ${:.2} aujourd'hui · ${:.2} cette semaine · ${:.2} ce mois (J{}/{})",
                burn.today,
                burn.week,
                burn.month,
                burn.projection.elapsed_days,
                burn.projection.days_in_month
            );
            println!(
                " projection fin de mois : ${:.2} (${:.2}/jour)",
                burn.projection.month_end, burn.projection.daily_rate
            );
            for status in &burn.budgets {
                println!(
                    " budget {} : ${:.2} / ${:.2} ({} %)",
                    status.scope,
                    status.spent,
                    status.limit,
                    (status.ratio * 100.0).round() as i64
                );
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_and_dimension_args_map_onto_the_core_enums() {
        assert_eq!(
            MetricsWindow::from(MetricsWindowArg::Day),
            MetricsWindow::Day
        );
        assert_eq!(
            MetricsWindow::from(MetricsWindowArg::FiveHours),
            MetricsWindow::Last5h
        );
        assert_eq!(
            MetricsWindow::from(MetricsWindowArg::SevenDays),
            MetricsWindow::Last7d
        );
        assert_eq!(
            MetricsDimension::from(MetricsDimensionArg::Project),
            MetricsDimension::Project
        );
        assert_eq!(MetricsWindowArg::default(), MetricsWindowArg::Day);
        assert_eq!(MetricsDimensionArg::default(), MetricsDimensionArg::Model);
        assert_eq!(MetricsFormat::default(), MetricsFormat::Table);
    }

    /// Le contrat `--format json` : les clés que consommerait un cron.
    #[test]
    fn json_payload_keeps_a_stable_camel_case_shape() {
        let row = metrics::MetricsRow {
            key: "claude-sonnet".to_string(),
            input_tokens: 1_000,
            output_tokens: 100,
            total_tokens: 1_100,
            cache_read_tokens: 400,
            cache_write_tokens: 0,
            entries: 2,
            cost: Some(1.25),
            cache_savings: Some(0.25),
        };
        let burn = metrics::BurnReport {
            today: 1.0,
            week: 2.0,
            month: 3.0,
            daily: vec![1.0, 2.0],
            projection: kaji::metrics::projection::project_month_end(&[1.0, 2.0], 30),
            budgets: vec![kaji::metrics::budget::BudgetStatus::new(
                "global", 100.0, 3.0,
            )],
        };
        let rows = [row.clone()];
        let payload = MetricsJson {
            window: MetricsWindow::Month,
            dimension: MetricsDimension::Model,
            start: 100,
            end: 200,
            rows: &rows,
            totals: &row,
            cache_hit_rate: row.cache_hit_rate(),
            cache_savings: row.cache_savings,
            burn: &burn,
        };

        let value: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&payload).unwrap()).unwrap();
        assert_eq!(value["window"], "month");
        assert_eq!(value["dimension"], "model");
        assert_eq!(value["start"], 100);
        assert_eq!(value["rows"][0]["key"], "claude-sonnet");
        assert_eq!(value["rows"][0]["inputTokens"], 1_000);
        assert_eq!(value["rows"][0]["cacheReadTokens"], 400);
        assert_eq!(value["rows"][0]["cacheSavings"], 0.25);
        assert_eq!(value["totals"]["entries"], 2);
        assert_eq!(value["cacheHitRate"], 0.4);
        assert_eq!(value["burn"]["month"], 3.0);
        assert_eq!(value["burn"]["projection"]["daysInMonth"], 30);
        assert_eq!(value["burn"]["budgets"][0]["scope"], "global");
        assert_eq!(value["burn"]["budgets"][0]["level"], "ok");
    }
}
