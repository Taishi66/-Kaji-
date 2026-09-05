//! Budgets mensuels en dollars — global et par provider.
//!
//! `KAJI_BUDGET_MONTHLY_USD` cadre la dépense de tous les providers réunis ;
//! `KAJI_BUDGET_MONTHLY_USD_<PROVIDER>` cadre un provider en particulier.
//! Les deux passent par `Config::get_param`, donc une valeur de config vaut
//! une variable d'environnement.
//!
//! Franchir un seuil n'arrête rien : c'est une ligne d'avertissement, pas un
//! garde-fou — le user garde la main (pattern quota-awareness).

use serde::Serialize;

use crate::config::Config;

/// Seuils d'avertissement, en pourcentage du budget.
pub const BUDGET_THRESHOLDS: [u32; 3] = [50, 80, 100];

pub const GLOBAL_BUDGET_KEY: &str = "kaji_budget_monthly_usd";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetLevel {
    /// Sous 50 % — rien à signaler.
    Ok,
    /// 50 % franchi.
    Half,
    /// 80 % franchi.
    High,
    /// Budget épuisé ou dépassé.
    Over,
}

impl BudgetLevel {
    pub fn threshold(self) -> Option<u32> {
        match self {
            BudgetLevel::Ok => None,
            BudgetLevel::Half => Some(50),
            BudgetLevel::High => Some(80),
            BudgetLevel::Over => Some(100),
        }
    }
}

pub fn budget_level(ratio: f64) -> BudgetLevel {
    if ratio >= 1.0 {
        BudgetLevel::Over
    } else if ratio >= 0.8 {
        BudgetLevel::High
    } else if ratio >= 0.5 {
        BudgetLevel::Half
    } else {
        BudgetLevel::Ok
    }
}

/// `global` pour l'enveloppe tous providers, sinon le nom du provider.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BudgetStatus {
    pub scope: String,
    pub limit: f64,
    pub spent: f64,
    pub ratio: f64,
    pub level: BudgetLevel,
}

impl BudgetStatus {
    pub fn new(scope: impl Into<String>, limit: f64, spent: f64) -> Self {
        let ratio = if limit > 0.0 { spent / limit } else { 0.0 };
        BudgetStatus {
            scope: scope.into(),
            limit,
            spent,
            ratio,
            level: budget_level(ratio),
        }
    }

    pub fn breached(&self) -> bool {
        self.level != BudgetLevel::Ok
    }
}

/// `anthropic` → `KAJI_BUDGET_MONTHLY_USD_ANTHROPIC`. Tout ce qui n'est ni
/// alphanumérique ni `_` devient `_`, pour qu'un provider à tiret (`ollama-
/// cloud`) reste nommable en variable d'environnement.
pub fn provider_budget_key(provider: &str) -> String {
    let suffix: String = provider
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    format!("{GLOBAL_BUDGET_KEY}_{suffix}")
}

fn positive_budget(config: &Config, key: &str) -> Option<f64> {
    config
        .get_param::<f64>(key)
        .ok()
        .filter(|v| v.is_finite() && *v > 0.0)
}

pub fn global_monthly_budget(config: &Config) -> Option<f64> {
    positive_budget(config, GLOBAL_BUDGET_KEY)
}

pub fn provider_monthly_budget(config: &Config, provider: &str) -> Option<f64> {
    positive_budget(config, &provider_budget_key(provider))
}

/// Statuts de budget d'un mois : l'enveloppe globale d'abord, puis chaque
/// provider dépensier qui a un budget déclaré. Les providers sans budget ne
/// produisent aucune ligne.
pub fn monthly_statuses(
    config: &Config,
    month_total: f64,
    per_provider: &[(String, f64)],
) -> Vec<BudgetStatus> {
    let mut statuses = Vec::new();
    if let Some(limit) = global_monthly_budget(config) {
        statuses.push(BudgetStatus::new("global", limit, month_total));
    }
    for (provider, spent) in per_provider {
        if let Some(limit) = provider_monthly_budget(config, provider) {
            statuses.push(BudgetStatus::new(provider.clone(), limit, *spent));
        }
    }
    statuses
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_map_to_the_three_thresholds() {
        assert_eq!(budget_level(0.0), BudgetLevel::Ok);
        assert_eq!(budget_level(0.4999), BudgetLevel::Ok);
        assert_eq!(budget_level(0.5), BudgetLevel::Half);
        assert_eq!(budget_level(0.7999), BudgetLevel::Half);
        assert_eq!(budget_level(0.8), BudgetLevel::High);
        assert_eq!(budget_level(0.9999), BudgetLevel::High);
        assert_eq!(budget_level(1.0), BudgetLevel::Over);
        assert_eq!(budget_level(3.0), BudgetLevel::Over);
    }

    #[test]
    fn status_computes_ratio_and_flags_breach() {
        let status = BudgetStatus::new("global", 100.0, 82.5);
        assert!((status.ratio - 0.825).abs() < 1e-9);
        assert_eq!(status.level, BudgetLevel::High);
        assert!(status.breached());

        let quiet = BudgetStatus::new("global", 100.0, 10.0);
        assert_eq!(quiet.level, BudgetLevel::Ok);
        assert!(!quiet.breached());
    }

    #[test]
    fn zero_limit_never_reports_a_breach() {
        let status = BudgetStatus::new("global", 0.0, 42.0);
        assert_eq!(status.ratio, 0.0);
        assert_eq!(status.level, BudgetLevel::Ok);
    }

    #[test]
    fn provider_key_uppercases_to_an_env_name() {
        assert_eq!(
            provider_budget_key("anthropic"),
            "kaji_budget_monthly_usd_anthropic"
        );
        assert_eq!(
            provider_budget_key("ollama-cloud").to_uppercase(),
            "KAJI_BUDGET_MONTHLY_USD_OLLAMA_CLOUD"
        );
    }

    #[test]
    fn thresholds_are_the_documented_fifty_eighty_hundred() {
        assert_eq!(BUDGET_THRESHOLDS, [50, 80, 100]);
        assert_eq!(BudgetLevel::Half.threshold(), Some(50));
        assert_eq!(BudgetLevel::High.threshold(), Some(80));
        assert_eq!(BudgetLevel::Over.threshold(), Some(100));
        assert_eq!(BudgetLevel::Ok.threshold(), None);
    }
}
