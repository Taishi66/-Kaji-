//! Le provider du rejeu : il ne parle à aucun modèle, il resert les chunks
//! enregistrés pour l'appel courant.
//!
//! L'appel courant est désigné par `(turn_seq, call_idx)`, la clé sous laquelle
//! `stream_response_from_provider` a journalisé la requête. La signature
//! `Provider::stream(&self, ...)` ne la transporte pas : elle vit dans une
//! `ReplayPosition` partagée avec l'appelant, qui ouvre chaque tour et laisse le
//! provider incrémenter le compteur d'appels exactement comme `next_llm_call` le
//! fait à l'enregistrement
//! (`docs/superpowers/specs/2026-08-27-event-log-v2-replay-exact-design.md`, S3).

use std::sync::atomic::{AtomicI64, AtomicU32, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use kaji_providers::conversation::token_usage::ProviderUsage;
use kaji_providers::errors::ProviderError;
use kaji_providers::model::ModelConfig;
use rmcp::model::Tool;
use tracing::warn;

use crate::conversation::message::Message;
use crate::providers::base::{MessageStream, Provider};
use crate::replay::cursor::EventCursor;
use crate::replay::hashing::request_hash;

/// Où en est le rejeu dans le journal. Miroir exact du compteur de capture
/// (`TurnRecorder::next_call_idx`) : un tour ouvert remet le compteur d'appels à
/// zéro, chaque appel LLM en consomme un.
#[derive(Debug, Default)]
pub struct ReplayPosition {
    turn_seq: AtomicI64,
    next_call_idx: AtomicU32,
}

impl ReplayPosition {
    pub fn begin_turn(&self, turn_seq: i64) {
        self.turn_seq.store(turn_seq, Ordering::SeqCst);
        self.next_call_idx.store(0, Ordering::SeqCst);
    }

    /// Le tour ouvert. Les intercepteurs du rejeu (bloc mémoire, horloge,
    /// compaction, approbations) s'adressent au journal par ce tour-là.
    pub fn turn(&self) -> i64 {
        self.turn_seq.load(Ordering::SeqCst)
    }

    fn next_call(&self) -> (i64, u32) {
        (
            self.turn_seq.load(Ordering::SeqCst),
            self.next_call_idx.fetch_add(1, Ordering::SeqCst),
        )
    }
}

pub struct ReplayProvider {
    cursor: Arc<EventCursor>,
    position: Arc<ReplayPosition>,
    lenient: bool,
}

impl ReplayProvider {
    pub fn new(cursor: Arc<EventCursor>, lenient: bool) -> Self {
        Self {
            cursor,
            position: Arc::new(ReplayPosition::default()),
            lenient,
        }
    }

    /// Le curseur de position à ouvrir sur chaque tour rejoué.
    pub fn position(&self) -> Arc<ReplayPosition> {
        Arc::clone(&self.position)
    }
}

#[async_trait]
impl Provider for ReplayProvider {
    fn get_name(&self) -> &str {
        "replay"
    }

    async fn stream(
        &self,
        _model_config: &ModelConfig,
        system: &str,
        messages: &[Message],
        tools: &[Tool],
    ) -> Result<MessageStream, ProviderError> {
        let (turn_seq, call_idx) = self.position.next_call();
        let exchange = self
            .cursor
            .llm_responses
            .get(&(turn_seq, call_idx))
            .ok_or_else(|| {
                ProviderError::ExecutionError(format!(
                    "replay: journal tronqué ou divergent — aucune réponse enregistrée au tour {turn_seq}, appel {call_idx}"
                ))
            })?;

        let replayed_hash = request_hash(system, messages, tools);
        if replayed_hash != exchange.request_hash {
            if !self.lenient {
                return Err(ProviderError::ExecutionError(format!(
                    "replay: requête divergente au tour {turn_seq}, appel {call_idx} — \
                     enregistré {}, rejoué {replayed_hash}",
                    exchange.request_hash
                )));
            }
            warn!(
                turn_seq,
                call_idx,
                recorded = %exchange.request_hash,
                replayed = %replayed_hash,
                "replay lenient: requête divergente, réponse enregistrée servie quand même"
            );
        }

        if exchange.finish != "stop" {
            return Err(ProviderError::ExecutionError(format!(
                "replay: l'appel du tour {turn_seq}, appel {call_idx} s'est terminé sur \
                 « {} » à l'enregistrement",
                exchange.finish
            )));
        }

        let chunks = exchange
            .chunks
            .iter()
            .map(|chunk| {
                serde_json::from_value::<(Option<Message>, Option<ProviderUsage>)>(chunk.clone())
                    .map_err(|error| {
                        ProviderError::ExecutionError(format!(
                            "replay: chunk illisible au tour {turn_seq}, appel {call_idx}: {error}"
                        ))
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Box::pin(futures::stream::iter(chunks.into_iter().map(Ok))))
    }
}
