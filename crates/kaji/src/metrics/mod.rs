//! Télémétrie tokens/coûts native — agrégats du `usage_ledger` par modèle,
//! provider, session ou projet, sur fenêtres glissantes (5 h / 7 j) et
//! calendaires (jour / semaine / mois locaux), économie de cache, burn rate,
//! projection de fin de mois et budgets.
//!
//! Lecture pure : rien ici n'écrit dans le ledger et rien n'entre dans le
//! prompt d'un tour, donc aucun kind d'event replay n'est requis — `/cost` et
//! `kaji metrics` vivent hors de la boucle agent.

pub mod budget;
pub mod calendar;
pub mod projection;

use anyhow::Result;
use chrono::{DateTime, Local, TimeZone};
use serde::Serialize;

use crate::config::paths::find_git_root;
use crate::session::SessionManager;
use budget::BudgetStatus;
use calendar::Span;
use projection::Projection;

const UNKNOWN_KEY: &str = "(inconnu)";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum MetricsWindow {
    Day,
    Week,
    Month,
    Last5h,
    Last7d,
}

impl MetricsWindow {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_lowercase().as_str() {
            "day" | "jour" | "j" => Some(MetricsWindow::Day),
            "week" | "semaine" | "s" => Some(MetricsWindow::Week),
            "month" | "mois" | "m" => Some(MetricsWindow::Month),
            "5h" => Some(MetricsWindow::Last5h),
            "7d" | "7j" => Some(MetricsWindow::Last7d),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            MetricsWindow::Day => "jour",
            MetricsWindow::Week => "semaine",
            MetricsWindow::Month => "mois",
            MetricsWindow::Last5h => "5 h",
            MetricsWindow::Last7d => "7 j",
        }
    }

    pub fn span(self, now: DateTime<Local>) -> Span {
        match self {
            MetricsWindow::Day => calendar::day_span(now),
            MetricsWindow::Week => calendar::week_span(now),
            MetricsWindow::Month => calendar::month_span(now),
            MetricsWindow::Last5h => Span::sliding(now, 5 * 3600),
            MetricsWindow::Last7d => Span::sliding(now, 7 * 24 * 3600),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum MetricsDimension {
    Model,
    Provider,
    Session,
    Project,
}

impl MetricsDimension {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_lowercase().as_str() {
            "model" | "modele" | "modèle" | "models" | "modeles" | "modèles" => {
                Some(MetricsDimension::Model)
            }
            "provider" | "providers" => Some(MetricsDimension::Provider),
            "session" | "sessions" => Some(MetricsDimension::Session),
            "project" | "projet" | "projects" | "projets" => Some(MetricsDimension::Project),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            MetricsDimension::Model => "modèle",
            MetricsDimension::Provider => "provider",
            MetricsDimension::Session => "session",
            MetricsDimension::Project => "projet",
        }
    }
}

/// Un groupe brut du ledger : la clé de dimension demandée, plus le couple
/// (provider, modèle) que le calcul de cache a besoin de garder pour aller
/// chercher le bon tarif.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LedgerBucket {
    pub key: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub total_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub cost: Option<f64>,
    pub entries: i64,
}

/// Tarifs USD par million de tokens, servis au calcul d'économie de cache.
pub trait PriceBook {
    /// `(entrée, lecture cache)`. `None` quand le modèle n'a pas de
    /// tarification connue — le provider local, typiquement.
    fn input_and_cache_read(
        &self,
        provider: Option<&str>,
        model: Option<&str>,
    ) -> Option<(f64, f64)>;
}

/// Tarifs du catalogue canonique embarqué — la même source que
/// `Pricing::estimate_cost`, donc les économies affichées sont cohérentes
/// avec les coûts estimés déjà écrits au ledger.
pub struct CanonicalPrices;

impl PriceBook for CanonicalPrices {
    fn input_and_cache_read(
        &self,
        provider: Option<&str>,
        model: Option<&str>,
    ) -> Option<(f64, f64)> {
        let canonical = crate::providers::canonical::maybe_get_canonical_model(provider?, model?)?;
        let input = canonical.cost.input?;
        Some((input, canonical.cost.cache_read.unwrap_or(input)))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MetricsRow {
    pub key: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub total_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub entries: i64,
    pub cost: Option<f64>,
    /// Ce que les lectures de cache ont évité de payer, au tarif d'entrée
    /// plein. Plancher, pas exact : les groupes sans tarif connu comptent
    /// pour zéro ; `None` quand aucun groupe n'avait de tarif.
    pub cache_savings: Option<f64>,
}

impl MetricsRow {
    fn add_bucket(&mut self, bucket: &LedgerBucket, prices: &impl PriceBook) {
        self.input_tokens += bucket.input_tokens;
        self.output_tokens += bucket.output_tokens;
        self.total_tokens += bucket.total_tokens;
        self.cache_read_tokens += bucket.cache_read_tokens;
        self.cache_write_tokens += bucket.cache_write_tokens;
        self.entries += bucket.entries;
        if let Some(cost) = bucket.cost {
            self.cost = Some(self.cost.unwrap_or(0.0) + cost);
        }
        if let Some((input_price, cache_read_price)) =
            prices.input_and_cache_read(bucket.provider.as_deref(), bucket.model.as_deref())
        {
            let saved = bucket.cache_read_tokens.max(0) as f64
                * (input_price - cache_read_price).max(0.0)
                / 1_000_000.0;
            self.cache_savings = Some(self.cache_savings.unwrap_or(0.0) + saved);
        }
    }

    /// Part des tokens d'entrée servie depuis le cache. `input_tokens` inclut
    /// déjà les tokens lus en cache (même convention que
    /// `Pricing::estimate_cost`), donc le rapport reste dans `[0, 1]`.
    pub fn cache_hit_rate(&self) -> f64 {
        if self.input_tokens <= 0 {
            return 0.0;
        }
        (self.cache_read_tokens as f64 / self.input_tokens as f64).clamp(0.0, 1.0)
    }

    /// Ce que la fenêtre aurait coûté sans cache.
    pub fn cost_uncached(&self) -> Option<f64> {
        Some(self.cost? + self.cache_savings?)
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MetricsReport {
    pub window: MetricsWindow,
    pub dimension: MetricsDimension,
    pub start: i64,
    pub end: i64,
    pub rows: Vec<MetricsRow>,
    pub totals: MetricsRow,
}

/// Plie les groupes bruts en lignes par clé, coût décroissant puis tokens
/// décroissants (les lignes sans coût connu finissent derrière celles qui en
/// ont un, à tokens égaux).
pub fn fold_buckets(
    buckets: &[LedgerBucket],
    prices: &impl PriceBook,
) -> (Vec<MetricsRow>, MetricsRow) {
    let mut rows: Vec<MetricsRow> = Vec::new();
    let mut totals = MetricsRow {
        key: "total".to_string(),
        ..MetricsRow::default()
    };

    for bucket in buckets {
        let index = match rows.iter().position(|row| row.key == bucket.key) {
            Some(index) => index,
            None => {
                rows.push(MetricsRow {
                    key: bucket.key.clone(),
                    ..MetricsRow::default()
                });
                rows.len() - 1
            }
        };
        rows[index].add_bucket(bucket, prices);
        totals.add_bucket(bucket, prices);
    }

    rows.sort_by(|a, b| {
        b.cost
            .unwrap_or(-1.0)
            .partial_cmp(&a.cost.unwrap_or(-1.0))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.total_tokens.cmp(&a.total_tokens))
            .then(a.key.cmp(&b.key))
    });

    (rows, totals)
}

/// Racine de projet d'un `working_dir` : la racine git si le chemin existe
/// encore, sinon le chemin lui-même — un projet déplacé garde une ligne, il
/// ne disparaît pas dans `(inconnu)`.
pub fn project_key(working_dir: Option<&str>) -> String {
    let Some(dir) = working_dir.filter(|d| !d.is_empty()) else {
        return UNKNOWN_KEY.to_string();
    };
    let path = std::path::Path::new(dir);
    find_git_root(path)
        .map(|root| root.to_string_lossy().into_owned())
        .unwrap_or_else(|| dir.to_string())
}

/// Agrégat d'une fenêtre selon une dimension. `now` vient de l'appelant :
/// hors boucle agent, pas de seam replay à traverser.
pub async fn report(
    session_manager: &SessionManager,
    window: MetricsWindow,
    dimension: MetricsDimension,
    now: DateTime<Local>,
) -> Result<MetricsReport> {
    let span = window.span(now);
    let buckets = session_manager
        .metrics_buckets(dimension, span.start, span.end)
        .await?;
    let (rows, totals) = fold_buckets(&buckets, &CanonicalPrices);
    Ok(MetricsReport {
        window,
        dimension,
        start: span.start,
        end: span.end,
        rows,
        totals,
    })
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BurnReport {
    pub today: f64,
    pub week: f64,
    pub month: f64,
    /// Coût de chaque jour écoulé du mois, du 1er à aujourd'hui inclus.
    pub daily: Vec<f64>,
    pub projection: Projection,
    pub budgets: Vec<BudgetStatus>,
}

/// Répartit des lignes `(timestamp, coût)` sur les jours locaux écoulés du
/// mois de `now`. Les jours sans dépense valent `0.0` — la régression a
/// besoin d'une série continue, pas d'un nuage de points épars.
pub fn daily_costs(rows: &[(i64, Option<f64>)], now: DateTime<Local>) -> Vec<f64> {
    let elapsed = calendar::day_of_month(now) as usize;
    let mut daily = vec![0.0; elapsed];
    for (timestamp, cost) in rows {
        let Some(cost) = cost else { continue };
        let Some(moment) = Local.timestamp_opt(*timestamp, 0).single() else {
            continue;
        };
        let index = calendar::day_of_month(moment) as usize;
        if index >= 1 && index <= elapsed {
            daily[index - 1] += cost;
        }
    }
    daily
}

pub async fn burn(session_manager: &SessionManager, now: DateTime<Local>) -> Result<BurnReport> {
    let day = calendar::day_span(now);
    let week = calendar::week_span(now);
    let month = calendar::month_span(now);

    let today = session_manager
        .usage_cost_between(day.start, day.end)
        .await?;
    let week_cost = session_manager
        .usage_cost_between(week.start, week.end)
        .await?;

    let month_rows = session_manager
        .usage_ledger_costs_between(month.start, month.end)
        .await?;
    let daily = daily_costs(&month_rows, now);
    let month_cost = daily.iter().sum::<f64>();
    let projection = projection::project_month_end(&daily, calendar::days_in_month(now));

    let by_provider = report(
        session_manager,
        MetricsWindow::Month,
        MetricsDimension::Provider,
        now,
    )
    .await?;
    let per_provider: Vec<(String, f64)> = by_provider
        .rows
        .iter()
        .map(|row| (row.key.clone(), row.cost.unwrap_or(0.0)))
        .collect();
    let budgets =
        budget::monthly_statuses(crate::config::Config::global(), month_cost, &per_provider);

    Ok(BurnReport {
        today,
        week: week_cost,
        month: month_cost,
        daily,
        projection,
        budgets,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tarifs figés : `pricey` cache 10× moins cher que l'entrée, `free` sans
    /// tarif connu.
    struct FixedPrices;

    impl PriceBook for FixedPrices {
        fn input_and_cache_read(
            &self,
            _provider: Option<&str>,
            model: Option<&str>,
        ) -> Option<(f64, f64)> {
            match model {
                Some("pricey") => Some((10.0, 1.0)),
                _ => None,
            }
        }
    }

    /// `cache_savings` se calcule sur le seul tarif d'entrée, alors que le
    /// `cost` du ledger vient de `Pricing::estimate_cost`, qui exige entrée
    /// **et** sortie. Un modèle tarifé en entrée seulement afficherait donc une
    /// économie chiffrée à côté d'un coût « n/a ». Aucune garde à l'exécution
    /// pour ça : c'est le catalogue qui porte l'invariant, et c'est ici qu'il
    /// se vérifie — le jour où une entrée le casse, ce test le dit.
    #[test]
    fn the_bundled_catalog_never_prices_input_without_output() {
        let registry = crate::providers::canonical::CanonicalModelRegistry::bundled().unwrap();
        assert!(
            !registry.all_models().is_empty(),
            "un catalogue vide passerait l'invariant sans rien vérifier"
        );
        let asymmetric: Vec<&str> = registry
            .all_models()
            .into_iter()
            .filter(|model| model.cost.input.is_some() && model.cost.output.is_none())
            .map(|model| model.name.as_str())
            .collect();
        assert!(
            asymmetric.is_empty(),
            "modèles tarifés en entrée sans sortie : {asymmetric:?}"
        );
    }

    fn bucket(key: &str, model: &str, cost: Option<f64>, cache_read: i64) -> LedgerBucket {
        LedgerBucket {
            key: key.to_string(),
            provider: Some("anthropic".to_string()),
            model: Some(model.to_string()),
            input_tokens: 1_000,
            output_tokens: 100,
            total_tokens: 1_100,
            cache_read_tokens: cache_read,
            cache_write_tokens: 0,
            cost,
            entries: 1,
        }
    }

    #[test]
    fn folding_sums_buckets_sharing_a_key() {
        let buckets = vec![
            bucket("sonnet", "pricey", Some(1.0), 400),
            bucket("sonnet", "pricey", Some(2.0), 600),
            bucket("local", "free", None, 0),
        ];
        let (rows, totals) = fold_buckets(&buckets, &FixedPrices);

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].key, "sonnet", "la ligne la plus chère en tête");
        assert_eq!(rows[0].entries, 2);
        assert_eq!(rows[0].input_tokens, 2_000);
        assert_eq!(rows[0].cache_read_tokens, 1_000);
        assert!((rows[0].cost.unwrap() - 3.0).abs() < 1e-9);

        assert_eq!(rows[1].key, "local");
        assert_eq!(rows[1].cost, None, "aucune ligne du groupe n'a de coût");
        assert_eq!(rows[1].cache_savings, None, "ni de tarif connu");

        assert_eq!(totals.entries, 3);
        assert_eq!(totals.total_tokens, 3_300);
        assert!((totals.cost.unwrap() - 3.0).abs() < 1e-9);
    }

    #[test]
    fn cache_savings_price_reads_at_the_input_rate_difference() {
        // 1 000 tokens lus au cache, écart 10 - 1 = 9 $/M ⇒ 0,009 $.
        let (rows, _) = fold_buckets(
            &[bucket("sonnet", "pricey", Some(0.5), 1_000)],
            &FixedPrices,
        );
        assert!((rows[0].cache_savings.unwrap() - 0.009).abs() < 1e-12);
        assert!((rows[0].cost_uncached().unwrap() - 0.509).abs() < 1e-12);
    }

    #[test]
    fn cache_hit_rate_is_reads_over_input_and_stays_bounded() {
        let (rows, _) = fold_buckets(&[bucket("sonnet", "pricey", Some(0.5), 250)], &FixedPrices);
        assert!((rows[0].cache_hit_rate() - 0.25).abs() < 1e-9);

        let mut empty = MetricsRow::default();
        assert_eq!(empty.cache_hit_rate(), 0.0, "pas de division par zéro");
        empty.input_tokens = 100;
        empty.cache_read_tokens = 400;
        assert_eq!(empty.cache_hit_rate(), 1.0, "borné à 1");
    }

    #[test]
    fn window_and_dimension_parse_french_and_english_spellings() {
        assert_eq!(MetricsWindow::parse("day"), Some(MetricsWindow::Day));
        assert_eq!(MetricsWindow::parse("Mois"), Some(MetricsWindow::Month));
        assert_eq!(MetricsWindow::parse("5h"), Some(MetricsWindow::Last5h));
        assert_eq!(MetricsWindow::parse("7j"), Some(MetricsWindow::Last7d));
        assert_eq!(MetricsWindow::parse("decade"), None);

        assert_eq!(
            MetricsDimension::parse("modèles"),
            Some(MetricsDimension::Model)
        );
        assert_eq!(
            MetricsDimension::parse("PROJECT"),
            Some(MetricsDimension::Project)
        );
        assert_eq!(MetricsDimension::parse("planet"), None);
    }

    #[test]
    fn empty_working_dir_falls_back_to_the_unknown_key() {
        assert_eq!(project_key(None), UNKNOWN_KEY);
        assert_eq!(project_key(Some("")), UNKNOWN_KEY);
        assert_eq!(
            project_key(Some("/nowhere/at/all")),
            "/nowhere/at/all",
            "un chemin sans dépôt git reste sa propre clé"
        );
    }

    /// Bout-en-bout sur un vrai ledger SQLite : le pool est `pub(crate)`,
    /// donc ces cas vivent ici plutôt que dans `tests/`.
    mod ledger {
        use super::*;
        use crate::config::KajiMode;
        use crate::session::session_manager::SessionType;
        use chrono::{Datelike, NaiveDate};
        use sqlx::{Pool, Sqlite};
        use std::path::PathBuf;
        use tempfile::TempDir;

        async fn session(sm: &SessionManager, working_dir: &str, provider: &str) -> String {
            let id = sm
                .create_session(
                    PathBuf::from(working_dir),
                    "s".to_string(),
                    SessionType::User,
                    KajiMode::default(),
                )
                .await
                .unwrap()
                .id;
            sm.update(&id)
                .provider_name(provider)
                .apply()
                .await
                .unwrap();
            id
        }

        #[allow(clippy::too_many_arguments)]
        async fn seed(
            pool: &Pool<Sqlite>,
            session_id: &str,
            at: i64,
            model: &str,
            provider: &str,
            input: i64,
            cache_read: i64,
            cost: f64,
        ) {
            sqlx::query(
                r#"
                INSERT INTO usage_ledger (
                    session_id, created_timestamp, model, provider,
                    input_tokens, output_tokens, total_tokens,
                    cache_read_tokens, cache_write_tokens,
                    cost, cost_source, is_compaction
                )
                VALUES (?, ?, ?, ?, ?, 50, ?, ?, 0, ?, 'estimated', 0)
                "#,
            )
            .bind(session_id)
            .bind(at)
            .bind(model)
            .bind(provider)
            .bind(input)
            .bind(input + 50)
            .bind(cache_read)
            .bind(cost)
            .execute(pool)
            .await
            .unwrap();
        }

        fn at(y: i32, m: u32, d: u32, h: u32) -> DateTime<Local> {
            Local
                .from_local_datetime(
                    &NaiveDate::from_ymd_opt(y, m, d)
                        .unwrap()
                        .and_hms_opt(h, 0, 0)
                        .unwrap(),
                )
                .earliest()
                .unwrap()
        }

        /// Mercredi 9 septembre 2026 : le 1er est un mardi, la semaine ISO du
        /// 9 commence donc au lundi 7, et le mois compte 30 jours.
        fn reference_now() -> DateTime<Local> {
            at(2026, 9, 9, 15)
        }

        #[tokio::test]
        async fn calendar_windows_slice_the_same_ledger_into_day_week_and_month() {
            let dir = TempDir::new().unwrap();
            let sm = SessionManager::new(dir.path().to_path_buf());
            let id = session(&sm, "/tmp/projet-a", "anthropic").await;
            let pool = sm.storage().pool().await.unwrap();
            let now = reference_now();

            // Hier 23 h et aujourd'hui 00 h : deux jours, une seule semaine.
            seed(
                pool,
                &id,
                at(2026, 9, 8, 23).timestamp(),
                "m",
                "a",
                100,
                0,
                1.0,
            )
            .await;
            seed(
                pool,
                &id,
                at(2026, 9, 9, 0).timestamp(),
                "m",
                "a",
                200,
                0,
                2.0,
            )
            .await;
            seed(
                pool,
                &id,
                at(2026, 9, 9, 14).timestamp(),
                "m",
                "a",
                300,
                0,
                3.0,
            )
            .await;
            // Dimanche 6 : dans le mois, hors de la semaine du lundi 7.
            seed(
                pool,
                &id,
                at(2026, 9, 6, 12).timestamp(),
                "m",
                "a",
                400,
                0,
                4.0,
            )
            .await;
            // 31 août : hors du mois de septembre.
            seed(
                pool,
                &id,
                at(2026, 8, 31, 12).timestamp(),
                "m",
                "a",
                999,
                0,
                9.0,
            )
            .await;

            let day = report(&sm, MetricsWindow::Day, MetricsDimension::Model, now)
                .await
                .unwrap();
            assert_eq!(day.totals.input_tokens, 500, "seulement le 9 septembre");
            assert!((day.totals.cost.unwrap() - 5.0).abs() < 1e-9);

            let week = report(&sm, MetricsWindow::Week, MetricsDimension::Model, now)
                .await
                .unwrap();
            assert_eq!(week.totals.input_tokens, 600, "du lundi 7 au 9 inclus");

            let month = report(&sm, MetricsWindow::Month, MetricsDimension::Model, now)
                .await
                .unwrap();
            assert_eq!(month.totals.input_tokens, 1_000, "le 31 août est exclu");
        }

        #[tokio::test]
        async fn dimensions_split_the_same_window_four_ways() {
            let dir = TempDir::new().unwrap();
            let sm = SessionManager::new(dir.path().to_path_buf());
            let a = session(&sm, "/tmp/projet-a", "anthropic").await;
            let b = session(&sm, "/tmp/projet-b", "openai").await;
            let pool = sm.storage().pool().await.unwrap();
            let now = reference_now();
            let stamp = at(2026, 9, 9, 10).timestamp();

            seed(pool, &a, stamp, "claude-sonnet", "anthropic", 1_000, 0, 3.0).await;
            seed(pool, &a, stamp, "claude-haiku", "anthropic", 500, 0, 0.5).await;
            seed(pool, &b, stamp, "gpt-5", "openai", 800, 0, 2.0).await;

            let by_model = report(&sm, MetricsWindow::Day, MetricsDimension::Model, now)
                .await
                .unwrap();
            assert_eq!(by_model.rows.len(), 3);
            assert_eq!(
                by_model.rows[0].key, "claude-sonnet",
                "coût décroissant : {:?}",
                by_model.rows
            );

            let by_provider = report(&sm, MetricsWindow::Day, MetricsDimension::Provider, now)
                .await
                .unwrap();
            assert_eq!(by_provider.rows.len(), 2);
            let anthropic = by_provider
                .rows
                .iter()
                .find(|row| row.key == "anthropic")
                .unwrap();
            assert_eq!(anthropic.input_tokens, 1_500);
            assert_eq!(anthropic.entries, 2);

            let by_session = report(&sm, MetricsWindow::Day, MetricsDimension::Session, now)
                .await
                .unwrap();
            assert_eq!(by_session.rows.len(), 2);

            let by_project = report(&sm, MetricsWindow::Day, MetricsDimension::Project, now)
                .await
                .unwrap();
            let keys: Vec<&str> = by_project.rows.iter().map(|row| row.key.as_str()).collect();
            assert!(
                keys.contains(&"/tmp/projet-a") && keys.contains(&"/tmp/projet-b"),
                "un working_dir hors dépôt git reste sa propre clé : {keys:?}"
            );
        }

        #[tokio::test]
        async fn cache_hit_rate_is_read_from_the_ledger_columns() {
            let dir = TempDir::new().unwrap();
            let sm = SessionManager::new(dir.path().to_path_buf());
            let id = session(&sm, "/tmp/projet-a", "sans-tarif").await;
            let pool = sm.storage().pool().await.unwrap();
            let now = reference_now();

            seed(
                pool,
                &id,
                at(2026, 9, 9, 10).timestamp(),
                "m",
                "sans-tarif",
                1_000,
                750,
                1.0,
            )
            .await;

            let report = report(&sm, MetricsWindow::Day, MetricsDimension::Model, now)
                .await
                .unwrap();
            assert!((report.totals.cache_hit_rate() - 0.75).abs() < 1e-9);
            assert_eq!(
                report.totals.cache_savings, None,
                "sans tarif connu, aucune économie chiffrable"
            );
            assert_eq!(report.totals.cost_uncached(), None);
        }

        #[tokio::test]
        async fn burn_projects_the_month_from_the_elapsed_days() {
            let dir = TempDir::new().unwrap();
            let sm = SessionManager::new(dir.path().to_path_buf());
            let id = session(&sm, "/tmp/projet-a", "anthropic").await;
            let pool = sm.storage().pool().await.unwrap();
            let now = reference_now();
            assert_eq!(now.month(), 9, "la fixture suppose un mois de 30 jours");

            for day in 1..=9u32 {
                seed(
                    pool,
                    &id,
                    at(2026, 9, day, 10).timestamp(),
                    "m",
                    "anthropic",
                    100,
                    0,
                    2.0,
                )
                .await;
            }

            let burn = burn(&sm, now).await.unwrap();
            assert!((burn.today - 2.0).abs() < 1e-9);
            assert!(
                (burn.week - 6.0).abs() < 1e-9,
                "lundi 7 → 9 : {}",
                burn.week
            );
            assert!((burn.month - 18.0).abs() < 1e-9);
            assert_eq!(burn.daily.len(), 9);
            assert_eq!(burn.projection.elapsed_days, 9);
            assert_eq!(burn.projection.days_in_month, 30);
            assert!(
                (burn.projection.month_end - 60.0).abs() < 1e-6,
                "2 $/jour sur 30 jours : {}",
                burn.projection.month_end
            );
        }

        #[tokio::test]
        async fn an_empty_ledger_reports_zeroes_rather_than_failing() {
            let dir = TempDir::new().unwrap();
            let sm = SessionManager::new(dir.path().to_path_buf());
            let now = reference_now();

            let report = report(&sm, MetricsWindow::Month, MetricsDimension::Model, now)
                .await
                .unwrap();
            assert!(report.rows.is_empty());
            assert_eq!(report.totals.cost, None);

            let burn = burn(&sm, now).await.unwrap();
            assert_eq!(burn.month, 0.0);
            assert_eq!(burn.projection.month_end, 0.0);
            assert_eq!(burn.daily.len(), 9, "les 9 jours écoulés valent zéro");
        }
    }

    #[test]
    fn daily_costs_bucket_rows_into_elapsed_local_days() {
        let now = Local
            .from_local_datetime(
                &chrono::NaiveDate::from_ymd_opt(2026, 9, 5)
                    .unwrap()
                    .and_hms_opt(18, 0, 0)
                    .unwrap(),
            )
            .earliest()
            .unwrap();
        let day = |d: u32, h: u32| {
            Local
                .from_local_datetime(
                    &chrono::NaiveDate::from_ymd_opt(2026, 9, d)
                        .unwrap()
                        .and_hms_opt(h, 0, 0)
                        .unwrap(),
                )
                .earliest()
                .unwrap()
                .timestamp()
        };

        let daily = daily_costs(
            &[
                (day(1, 9), Some(1.0)),
                (day(1, 23), Some(0.5)),
                (day(3, 12), Some(2.0)),
                (day(5, 17), Some(4.0)),
                (day(5, 12), None),
            ],
            now,
        );

        assert_eq!(daily.len(), 5, "du 1er au 5 inclus");
        assert!((daily[0] - 1.5).abs() < 1e-9);
        assert_eq!(daily[1], 0.0, "un jour sans dépense vaut zéro, pas un trou");
        assert!((daily[2] - 2.0).abs() < 1e-9);
        assert_eq!(daily[3], 0.0);
        assert!((daily[4] - 4.0).abs() < 1e-9);
    }
}
