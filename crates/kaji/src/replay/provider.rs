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
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use kaji_providers::conversation::token_usage::ProviderUsage;
use kaji_providers::errors::ProviderError;
use kaji_providers::model::ModelConfig;
use rmcp::model::Tool;
use tracing::warn;

use crate::conversation::message::Message;
use crate::providers::base::{MessageStream, Provider};
use crate::replay::cursor::{EventCursor, LlmExchange};
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

    /// L'appel LLM sur le point d'être servi, sans le consommer : ce que la
    /// boucle assemble avant `stream` appartient à cet appel-là. Miroir de
    /// `TurnRecorder::current_call_idx` côté enregistrement.
    pub fn call(&self) -> u32 {
        self.next_call_idx.load(Ordering::SeqCst)
    }

    fn next_call(&self) -> (i64, u32) {
        (
            self.turn_seq.load(Ordering::SeqCst),
            self.next_call_idx.fetch_add(1, Ordering::SeqCst),
        )
    }
}

/// Un appel dont la requête rejouée ne correspond pas à celle enregistrée, et
/// que le mode lenient a servi quand même.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Divergence {
    pub turn_seq: i64,
    pub call_idx: u32,
    pub recorded_hash: String,
    pub replayed_hash: String,
}

/// Les divergences tolérées par le mode lenient, partagées avec l'appelant :
/// sans elles, `--lenient` servirait une réponse enregistrée sur une requête
/// différente sans que rien ne le signale hors du log fichier.
#[derive(Debug, Default)]
pub struct DivergenceLog {
    entries: Mutex<Vec<Divergence>>,
}

impl DivergenceLog {
    fn record(&self, divergence: Divergence) {
        self.entries.lock().unwrap().push(divergence);
    }

    /// Les divergences accumulées depuis le dernier appel — l'appelant les
    /// rend au fil des tours plutôt qu'en un bloc final.
    pub fn drain(&self) -> Vec<Divergence> {
        std::mem::take(&mut *self.entries.lock().unwrap())
    }
}

/// L'erreur à rendre pour un appel que l'enregistrement a terminé autrement que
/// sur `stop`. La variante enregistrée est rendue telle quelle : les deux
/// boucles branchent dessus — compaction de secours sur
/// `ContextLengthExceeded`, notification sur `CreditsExhausted`, message
/// d'erreur sinon — et un rejeu qui rendrait toujours `ExecutionError` prendrait
/// un autre bras sans que le hash soit jamais atteint, donc sans divergence
/// signalée.
fn recorded_failure(exchange: &LlmExchange, turn_seq: i64, call_idx: u32) -> ProviderError {
    exchange.error.clone().unwrap_or_else(|| {
        ProviderError::ExecutionError(format!(
            "replay: l'appel du tour {turn_seq}, appel {call_idx} s'est terminé sur \
             « {} » à l'enregistrement",
            exchange.finish
        ))
    })
}

pub struct ReplayProvider {
    cursor: Arc<EventCursor>,
    position: Arc<ReplayPosition>,
    divergences: Arc<DivergenceLog>,
    lenient: bool,
}

impl ReplayProvider {
    pub fn new(cursor: Arc<EventCursor>, lenient: bool) -> Self {
        Self {
            cursor,
            position: Arc::new(ReplayPosition::default()),
            divergences: Arc::new(DivergenceLog::default()),
            lenient,
        }
    }

    /// Le curseur de position à ouvrir sur chaque tour rejoué.
    pub fn position(&self) -> Arc<ReplayPosition> {
        Arc::clone(&self.position)
    }

    /// Les divergences tolérées, à rendre visibles par l'appelant.
    pub fn divergences(&self) -> Arc<DivergenceLog> {
        Arc::clone(&self.divergences)
    }
}

#[async_trait]
impl Provider for ReplayProvider {
    fn get_name(&self) -> &str {
        "replay"
    }

    /// `Provider::complete` délègue à `stream` par défaut, donc tout appel qui
    /// ne vient pas de la boucle — nommage de session (spawné, en course),
    /// résumé de compaction, résumé de paires d'outils — consommerait un
    /// `call_idx` du flux principal et décalerait tout le tour. Le rejeu n'a
    /// qu'un canal, celui que `next_llm_call` annonce : les autres sont
    /// refusés, jamais servis en silence.
    async fn complete(
        &self,
        _model_config: &ModelConfig,
        _system: &str,
        _messages: &[Message],
        _tools: &[Tool],
    ) -> Result<(Message, ProviderUsage), ProviderError> {
        Err(ProviderError::ExecutionError(format!(
            "replay: appel LLM hors boucle au tour {} — le rejeu ne sert que les appels \
             annoncés par la boucle",
            self.position.turn()
        )))
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
            self.divergences.record(Divergence {
                turn_seq,
                call_idx,
                recorded_hash: exchange.request_hash.clone(),
                replayed_hash,
            });
        }

        if exchange.finish != "stop" {
            return Err(recorded_failure(exchange, turn_seq, call_idx));
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

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use serde_json::json;

    use super::*;
    use crate::replay::cursor::{LlmExchange, LogMeta};

    fn cursor_recording(request_hash: &str) -> Arc<EventCursor> {
        Arc::new(EventCursor {
            log_meta: LogMeta {
                kaji_version: "test".to_string(),
                schema_version: 2,
                idgen_seed: "seed".to_string(),
            },
            llm_responses: HashMap::from([(
                (1, 0),
                LlmExchange {
                    request_hash: request_hash.to_string(),
                    chunks: vec![json!([null, null])],
                    finish: "stop".to_string(),
                    error: None,
                },
            )]),
            tool_results: HashMap::new(),
            memory_blocks: HashMap::new(),
            turn_contexts: HashMap::new(),
            tool_manifests: HashMap::new(),
            toolshim_messages: HashMap::new(),
            clock_reads: HashMap::new(),
            condense_turns: HashSet::new(),
            condense_summaries: HashMap::new(),
            tool_pair_summaries: HashMap::new(),
            approvals: HashMap::new(),
            gate_decisions: HashMap::new(),
            workflow_artifacts: HashMap::new(),
        })
    }

    async fn replay_first_call(provider: &ReplayProvider) {
        provider.position().begin_turn(1);
        let _stream = provider
            .stream(&ModelConfig::new("kaji-replay"), "system", &[], &[])
            .await
            .expect("le rejeu sert la réponse enregistrée");
    }

    #[tokio::test]
    async fn lenient_exposes_the_divergence_it_tolerates() {
        let provider = ReplayProvider::new(cursor_recording("hash-enregistré"), true);
        let divergences = provider.divergences();

        replay_first_call(&provider).await;

        let tolerated = divergences.drain();
        assert_eq!(tolerated.len(), 1, "{tolerated:?}");
        assert_eq!(tolerated[0].turn_seq, 1);
        assert_eq!(tolerated[0].call_idx, 0);
        assert_eq!(tolerated[0].recorded_hash, "hash-enregistré");
        assert_eq!(tolerated[0].replayed_hash, request_hash("system", &[], &[]));
        assert!(
            divergences.drain().is_empty(),
            "drain rend ce qui s'est accumulé depuis le dernier appel"
        );
    }

    /// Le nommage de session, le résumé de compaction et le résumé de paires
    /// d'outils appellent `Provider::complete`, dont l'implémentation par
    /// défaut délègue à `stream` : ils consommeraient un `call_idx` du flux
    /// principal — le nommage depuis un `tokio::spawn`, donc en course avec la
    /// boucle. Le rejeu les refuse au lieu de décaler le journal.
    #[tokio::test]
    async fn an_off_loop_completion_is_refused_without_moving_the_position() {
        let provider =
            ReplayProvider::new(cursor_recording(&request_hash("system", &[], &[])), false);
        provider.position().begin_turn(1);

        let refused = provider
            .complete(&ModelConfig::new("kaji-replay"), "system", &[], &[])
            .await;
        let error = refused.expect_err("un appel hors boucle ne peut pas être rejoué");
        assert!(
            error.to_string().contains("hors boucle"),
            "l'erreur nomme la cause : {error}"
        );

        let _served = provider
            .stream(&ModelConfig::new("kaji-replay"), "system", &[], &[])
            .await
            .expect("le premier appel de la boucle reste l'appel 0");
    }

    #[tokio::test]
    async fn a_faithful_request_records_no_divergence() {
        let provider =
            ReplayProvider::new(cursor_recording(&request_hash("system", &[], &[])), true);
        let divergences = provider.divergences();

        replay_first_call(&provider).await;

        assert!(divergences.drain().is_empty());
    }
}
