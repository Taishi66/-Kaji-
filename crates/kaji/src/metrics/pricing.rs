//! Tarifs déclarés par l'utilisateur, pour les modèles que le catalogue
//! canonique embarqué ne connaît pas — un provider maison, un déploiement
//! privé, un modèle sorti après la dernière mise à jour du catalogue.
//!
//! La section `model_pricing:` de la config indexe un nom de modèle sur les
//! mêmes champs que le catalogue (`input`, `output`, `cache_read`,
//! `cache_write`, en USD par million de tokens). Elle prime le catalogue ; un
//! modèle absent des deux reste sans coût plutôt que chiffré à zéro.

use std::collections::HashMap;

use crate::config::Config;
use crate::providers::canonical::{strip_version_suffix, Pricing};

use super::{CanonicalPrices, LedgerBucket, PriceBook};

pub const MODEL_PRICING_KEY: &str = "model_pricing";

/// Tarifs de la config, indexés sur le nom de modèle.
///
/// Une entrée n'est retenue que si elle porte **à la fois** `input` et
/// `output` : un modèle tarifé à moitié afficherait une économie de cache à
/// côté d'un coût « n/a », l'asymétrie que le catalogue s'interdit déjà.
#[derive(Debug, Clone, Default)]
pub struct ModelPricingOverrides {
    entries: HashMap<String, Pricing>,
}

impl ModelPricingOverrides {
    pub fn new(entries: HashMap<String, Pricing>) -> Self {
        ModelPricingOverrides {
            entries: entries
                .into_iter()
                .filter(|(_, price)| price.input.is_some() && price.output.is_some())
                .collect(),
        }
    }

    /// Section `model_pricing:` de la config. Absente ou illisible : aucun
    /// tarif déclaré, le catalogue reste seul.
    pub fn load(config: &Config) -> Self {
        Self::new(config.get_param(MODEL_PRICING_KEY).unwrap_or_default())
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Nom exact d'abord, puis le nom débarrassé de son suffixe de version
    /// (`-latest`, `-20260101`, `@20260101`, `-2026-01-01`, `-bedrock`) — la
    /// normalisation du catalogue, et sa sensibilité à la casse.
    pub fn pricing(&self, model: &str) -> Option<&Pricing> {
        self.entries
            .get(model)
            .or_else(|| self.entries.get(&strip_version_suffix(model)))
    }
}

/// Tarifs servis aux agrégats : la config d'abord, le catalogue embarqué
/// ensuite.
pub struct ConfiguredPrices {
    overrides: ModelPricingOverrides,
}

impl ConfiguredPrices {
    pub fn new(overrides: ModelPricingOverrides) -> Self {
        ConfiguredPrices { overrides }
    }

    pub fn load(config: &Config) -> Self {
        Self::new(ModelPricingOverrides::load(config))
    }
}

impl PriceBook for ConfiguredPrices {
    fn pricing(&self, provider: Option<&str>, model: Option<&str>) -> Option<Pricing> {
        let model = model?;
        match self.overrides.pricing(model) {
            Some(declared) => Some(declared.clone()),
            None => CanonicalPrices.pricing(provider, Some(model)),
        }
    }
}

/// Coût d'un groupe du ledger, même formule que `Pricing::estimate_cost` mais
/// sur les sommes `i64` d'un agrégat : entrée non cachée au tarif plein,
/// lectures et écritures de cache à leur tarif (à défaut celui de l'entrée),
/// sortie au tarif de sortie.
pub fn estimate_bucket_cost(pricing: &Pricing, bucket: &LedgerBucket) -> Option<f64> {
    let input_price = pricing.input?;
    let output_price = pricing.output?;
    let cache_read_price = pricing.cache_read.unwrap_or(input_price);
    let cache_write_price = pricing.cache_write.unwrap_or(input_price);

    let input = bucket.input_tokens.max(0) as f64;
    let output = bucket.output_tokens.max(0) as f64;
    let cache_read = bucket.cache_read_tokens.max(0) as f64;
    let cache_write = bucket.cache_write_tokens.max(0) as f64;
    let uncached = (input - cache_read - cache_write).max(0.0);

    Some(
        (uncached * input_price
            + cache_read * cache_read_price
            + cache_write * cache_write_price
            + output * output_price)
            / 1_000_000.0,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::metrics::{fold_buckets, ConfiguredPrices, LedgerBucket, PriceBook};
    use std::collections::HashMap;
    use tempfile::NamedTempFile;

    fn pricing(input: Option<f64>, output: Option<f64>, cache_read: Option<f64>) -> Pricing {
        Pricing {
            input,
            output,
            cache_read,
            cache_write: None,
        }
    }

    fn overrides(entries: &[(&str, Pricing)]) -> ModelPricingOverrides {
        ModelPricingOverrides::new(
            entries
                .iter()
                .map(|(name, price)| (name.to_string(), price.clone()))
                .collect::<HashMap<String, Pricing>>(),
        )
    }

    fn bucket(model: &str, provider: &str, cost: Option<f64>, cache_read: i64) -> LedgerBucket {
        LedgerBucket {
            key: model.to_string(),
            provider: Some(provider.to_string()),
            model: Some(model.to_string()),
            input_tokens: 1_000_000,
            output_tokens: 100_000,
            total_tokens: 1_100_000,
            cache_read_tokens: cache_read,
            cache_write_tokens: 0,
            cost,
            entries: 1,
        }
    }

    #[test]
    fn an_overlay_entry_beats_the_bundled_catalog() {
        let catalog = CanonicalPrices
            .pricing(Some("anthropic"), Some("claude-sonnet-4-5"))
            .expect("le modèle est au catalogue");
        assert!(catalog.input.is_some());

        let prices = ConfiguredPrices::new(overrides(&[(
            "claude-sonnet-4-5",
            pricing(Some(1.0), Some(2.0), None),
        )]));
        let resolved = prices
            .pricing(Some("anthropic"), Some("claude-sonnet-4-5"))
            .unwrap();
        assert_eq!(resolved.input, Some(1.0), "la config prime le catalogue");
        assert_eq!(resolved.output, Some(2.0));
    }

    #[test]
    fn a_model_in_neither_the_overlay_nor_the_catalog_stays_unpriced() {
        let prices = ConfiguredPrices::new(overrides(&[(
            "un-autre-modele",
            pricing(Some(1.0), Some(2.0), None),
        )]));
        assert!(prices
            .pricing(Some("provider-maison"), Some("modele-maison"))
            .is_none());

        let (rows, _) = fold_buckets(
            &[bucket("modele-maison", "provider-maison", None, 0)],
            &prices,
        );
        assert_eq!(rows[0].cost, None, "n/a plutôt qu'un zéro inventé");
        assert_eq!(rows[0].cache_savings, None);
    }

    #[test]
    fn an_overlay_prices_a_bucket_the_ledger_left_uncosted() {
        let prices = ConfiguredPrices::new(overrides(&[(
            "modele-maison",
            pricing(Some(3.0), Some(15.0), None),
        )]));
        let (rows, _) = fold_buckets(
            &[bucket("modele-maison", "provider-maison", None, 0)],
            &prices,
        );
        // 1 M d'entrée à 3 $/M + 100 k de sortie à 15 $/M.
        assert!((rows[0].cost.unwrap() - 4.5).abs() < 1e-9);
    }

    #[test]
    fn a_cost_already_recorded_in_the_ledger_is_never_recomputed() {
        let prices = ConfiguredPrices::new(overrides(&[(
            "modele-maison",
            pricing(Some(3.0), Some(15.0), None),
        )]));
        let (rows, _) = fold_buckets(
            &[bucket("modele-maison", "provider-maison", Some(0.25), 0)],
            &prices,
        );
        assert!(
            (rows[0].cost.unwrap() - 0.25).abs() < 1e-9,
            "le coût du ledger fait foi"
        );
    }

    #[test]
    fn overlay_lookup_falls_back_to_the_version_stripped_name() {
        let prices = ConfiguredPrices::new(overrides(&[(
            "modele-maison",
            pricing(Some(3.0), Some(15.0), None),
        )]));
        assert_eq!(
            prices
                .pricing(Some("provider-maison"), Some("modele-maison-20260101"))
                .unwrap()
                .input,
            Some(3.0)
        );
        assert!(
            prices
                .pricing(Some("provider-maison"), Some("Modele-Maison"))
                .is_none(),
            "la casse compte, comme au catalogue"
        );
    }

    #[test]
    fn an_entry_without_both_input_and_output_is_ignored() {
        let half = overrides(&[("modele-maison", pricing(Some(3.0), None, Some(0.3)))]);
        assert!(
            half.pricing("modele-maison").is_none(),
            "un tarif d'entrée seul afficherait une économie de cache à côté d'un coût n/a"
        );
    }

    #[test]
    fn cache_savings_price_reads_at_the_overlay_rate_difference() {
        let prices = ConfiguredPrices::new(overrides(&[(
            "modele-maison",
            pricing(Some(10.0), Some(20.0), Some(1.0)),
        )]));
        let (rows, _) = fold_buckets(
            &[bucket("modele-maison", "provider-maison", None, 1_000_000)],
            &prices,
        );
        // 1 M de lectures, écart 10 - 1 = 9 $/M.
        assert!((rows[0].cache_savings.unwrap() - 9.0).abs() < 1e-9);
        // Entrée non cachée nulle, lectures à 1 $/M, sortie 100 k à 20 $/M.
        assert!((rows[0].cost.unwrap() - 3.0).abs() < 1e-9);
    }

    #[test]
    fn a_catalog_model_without_pricing_still_saves_nothing() {
        let prices = ConfiguredPrices::new(ModelPricingOverrides::default());
        let (rows, _) = fold_buckets(&[bucket("mistral-nemo", "ollama", None, 500_000)], &prices);
        assert_eq!(
            rows[0].cache_savings, None,
            "le catalogue efface les tarifs des providers locaux"
        );
        assert_eq!(rows[0].cost, None);
    }

    #[test]
    fn overrides_load_from_the_model_pricing_config_section() {
        let config_file = NamedTempFile::new().unwrap();
        let secrets_file = NamedTempFile::new().unwrap();
        std::fs::write(
            config_file.path(),
            "model_pricing:\n  \"deepseek-v4-flash:0731\":\n    input: 0.27\n    output: 1.1\n    cache_read: 0.027\n",
        )
        .unwrap();
        let config =
            Config::new_with_file_secrets(config_file.path(), secrets_file.path()).unwrap();

        let loaded = ModelPricingOverrides::load(&config);
        let entry = loaded
            .pricing("deepseek-v4-flash:0731")
            .expect("le nom porte le suffixe tel quel");
        assert_eq!(entry.input, Some(0.27));
        assert_eq!(entry.output, Some(1.1));
        assert_eq!(entry.cache_read, Some(0.027));
    }

    #[test]
    fn a_config_without_the_section_yields_no_override() {
        let config_file = NamedTempFile::new().unwrap();
        let secrets_file = NamedTempFile::new().unwrap();
        std::fs::write(config_file.path(), "kaji_mode: auto\n").unwrap();
        let config =
            Config::new_with_file_secrets(config_file.path(), secrets_file.path()).unwrap();

        assert!(ModelPricingOverrides::load(&config).is_empty());
    }
}
