use anyhow::Result;
use kaji_providers::conversation::token_usage::{ProviderUsage, Usage as TokenUsage};

use crate::agents::state_machine::operation::StateEffect;
use crate::conversation::message::MessageUsage;
use crate::conversation::Conversation;
use crate::session::{Session, SessionManager};

fn attach_to_last_assistant(effects: &mut [StateEffect], usage: &ProviderUsage) {
    let Some(message) = effects.iter_mut().rev().find_map(|effect| match effect {
        StateEffect::AppendMessage(message)
            if message.role == rmcp::model::Role::Assistant && message.error_kind().is_none() =>
        {
            Some(message)
        }
        _ => None,
    }) else {
        return;
    };
    message.metadata.usage = Some(Box::new(MessageUsage::from_provider_usage(usage, false)));
}

pub(super) fn enrich(session: &Session, effects: &mut [StateEffect]) {
    if !effects.iter().any(|effect| {
        matches!(
            effect,
            StateEffect::RecordUsage(_) | StateEffect::ReplaceConversation { usage: Some(_), .. }
        )
    }) {
        return;
    }
    enrich_with_prices(
        session,
        effects,
        &crate::metrics::pricing::ConfiguredPrices::load(crate::config::Config::global()),
    );
}

fn enrich_with_prices(
    session: &Session,
    effects: &mut [StateEffect],
    prices: &dyn crate::metrics::PriceBook,
) {
    for index in 0..effects.len() {
        let (usage, replaces_conversation) = match &effects[index] {
            StateEffect::RecordUsage(usage) => (usage.clone(), false),
            StateEffect::ReplaceConversation {
                usage: Some(usage), ..
            } => (usage.clone(), true),
            _ => continue,
        };
        let (cost, cost_source) = crate::metrics::pricing::resolve_usage_cost(
            &usage,
            session.provider_name.as_deref(),
            prices,
        );

        let mut enriched = usage.clone();
        enriched.cost = cost;
        enriched.cost_source = cost_source;

        if !replaces_conversation {
            attach_to_last_assistant(effects, &enriched);
        }
        match &mut effects[index] {
            StateEffect::RecordUsage(usage) => *usage = enriched,
            StateEffect::ReplaceConversation { usage, .. } => *usage = Some(enriched),
            _ => {}
        }
    }
}

/// `replay` vrai ⇒ le tour rejoue une trace : rien n'est facturé, l'usage
/// ledger de la session source reste tel qu'il a été enregistré (spec
/// `docs/superpowers/specs/2026-08-27-event-log-v2-replay-exact-design.md`, S3).
/// Pendant boucle SM du gate de `Agent::update_session_metrics`.
pub(super) async fn record(
    session_manager: &SessionManager,
    session: &Session,
    usage: &ProviderUsage,
    replaces_conversation: bool,
    replay: bool,
) -> Result<()> {
    if replay {
        return Ok(());
    }
    let ledger = MessageUsage::from_provider_usage(usage, replaces_conversation);
    session_manager
        .record_usage_metrics(
            &session.id,
            session.schedule_id.clone(),
            usage.usage,
            &usage.model,
            &ledger,
        )
        .await?;
    Ok(())
}

pub(super) async fn estimate_context(conversation: &Conversation) -> Result<TokenUsage> {
    let tokens = crate::context_mgmt::count_context_tokens(conversation).await?;
    Ok(TokenUsage::new(Some(tokens), None, Some(tokens)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::base::CostSource;

    fn custom_prices() -> crate::metrics::pricing::ConfiguredPrices {
        crate::metrics::pricing::ConfiguredPrices::new(
            crate::metrics::pricing::ModelPricingOverrides::new(std::collections::HashMap::from([
                (
                    "custom-priced".to_owned(),
                    crate::providers::canonical::Pricing {
                        input: Some(2.0),
                        output: Some(4.0),
                        cache_read: Some(0.5),
                        cache_write: Some(3.0),
                    },
                ),
            ])),
        )
    }

    #[test]
    fn state_usage_prices_custom_model_and_preserves_provider_cost() {
        let session = Session {
            provider_name: Some("custom".into()),
            ..Session::default()
        };
        let usage = ProviderUsage::new(
            "custom-priced".into(),
            TokenUsage::new(Some(1000), Some(100), None).with_cache_tokens(Some(400), Some(100)),
        );
        let mut effects = vec![StateEffect::RecordUsage(usage.clone())];
        enrich_with_prices(&session, &mut effects, &custom_prices());
        let StateEffect::RecordUsage(enriched) = &effects[0] else {
            panic!("usage effect retained")
        };
        assert!((enriched.cost.unwrap() - 0.0019).abs() < 1e-12);
        assert_eq!(enriched.cost_source, Some(CostSource::Estimated));
        let mut effects = vec![StateEffect::RecordUsage(
            usage.with_cost(0.25, CostSource::ProviderReported),
        )];
        enrich_with_prices(&session, &mut effects, &custom_prices());
        let StateEffect::RecordUsage(enriched) = &effects[0] else {
            panic!("usage effect retained")
        };
        assert_eq!(enriched.cost, Some(0.25));
        assert_eq!(enriched.cost_source, Some(CostSource::ProviderReported));
    }
}
