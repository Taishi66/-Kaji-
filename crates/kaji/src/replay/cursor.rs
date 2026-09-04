//! Le journal v2 d'une session, indexé par clé pour le rejeu.
//!
//! Chaque kind est rangé sous la clé par laquelle la boucle le redemandera —
//! `(turn_seq, call_idx)` pour un appel LLM, `tool_call_id` pour un résultat
//! d'outil, `turn_seq` pour le bloc mémoire et les lectures d'horloge. Jamais
//! d'adressage positionnel : une entrée manquante doit sauter aux yeux, pas
//! décaler silencieusement tout ce qui suit
//! (`docs/superpowers/specs/2026-08-27-event-log-v2-replay-exact-design.md`, S3).

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use serde::Deserialize;
use serde_json::Value;
use thiserror::Error;

use crate::conversation::message::Message;
use crate::permission::Permission;
use crate::replay::manifest::ToolManifest;
use crate::session::session_manager::SessionEvent;
use crate::session::SessionManager;

/// Pourquoi une session ne peut pas être rejouée. Chaque cas a sa réponse
/// utilisateur propre — le CLI de rejeu (Task 11) les traduit ; aucune n'est
/// une erreur brute.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ReplayUnavailable {
    #[error("session enregistrée avant le replay v2 (aucun log_meta dans le journal)")]
    PreV2,

    #[error("journal purgé ou incomplet : la session est marquée non rejouable")]
    Purged,

    #[error("journal tronqué au tour {0} : le tour n'a pas de turn_end")]
    TruncatedAt(i64),
}

/// En-tête du journal : ce qui identifie l'enregistrement et la graine dont
/// l'`IdGen` du rejeu redérive les mêmes ids.
#[derive(Debug, Clone, Deserialize)]
pub struct LogMeta {
    pub kaji_version: String,
    pub schema_version: i32,
    pub idgen_seed: String,
}

/// Un appel LLM enregistré : la requête (par son hash) et la réponse qui lui a
/// été servie. Les deux vivent dans deux events distincts du journal —
/// `llm_request` et `llm_response` — réunis ici sous leur clé commune.
#[derive(Debug, Clone)]
pub struct LlmExchange {
    pub request_hash: String,
    pub chunks: Vec<Value>,
    pub finish: String,
}

/// Le journal v2 d'une session, prêt à être interrogé par clé.
pub struct EventCursor {
    pub log_meta: LogMeta,
    pub llm_responses: HashMap<(i64, u32), LlmExchange>,
    pub tool_results: HashMap<String, String>,
    pub memory_blocks: HashMap<i64, String>,
    /// L'environnement d'extensions de chaque tour : outils et fragments de
    /// prompt système qui en dérivent (`replay::manifest`).
    pub tool_manifests: HashMap<i64, ToolManifest>,
    pub clock_reads: HashMap<i64, Vec<String>>,
    pub condense_turns: HashSet<i64>,
    /// Le résumé que l'appel LLM de compaction a rendu, par tour. Cet appel
    /// passe par `Provider::complete`, hors du canal `(turn_seq, call_idx)` :
    /// il a donc sa propre clé.
    pub condense_summaries: HashMap<i64, Message>,
    /// Les décisions d'approbation v1 du journal, par `(turn_seq, request_id)`,
    /// réduites à ce que le rejeu en fait : l'outil a-t-il eu le droit de
    /// tourner. Rejouées telles quelles, jamais redemandées à l'utilisateur
    /// (spec S4).
    pub approvals: HashMap<(i64, String), bool>,
}

fn payload(event: &SessionEvent) -> Option<Value> {
    serde_json::from_str(&event.payload_json).ok()
}

fn turn_seq(event: &SessionEvent, payload: &Value) -> i64 {
    payload
        .get("turn_seq")
        .and_then(Value::as_i64)
        .unwrap_or(event.turn_seq)
}

fn call_key(event: &SessionEvent, payload: &Value) -> Option<(i64, u32)> {
    let call_idx = payload.get("call_idx").and_then(Value::as_u64)?;
    Some((turn_seq(event, payload), call_idx as u32))
}

impl EventCursor {
    /// Comme [`Self::load_until`], sans borne : une troncature de fin de
    /// journal est toujours refusée.
    pub async fn load(session_manager: &SessionManager, session_id: &str) -> Result<Self> {
        Self::load_until(session_manager, session_id, None).await
    }

    /// Charge le journal, en tolérant une troncature de fin de journal quand
    /// elle se situe strictement après `until_turn` : le tour interrompu
    /// n'est de toute façon jamais rejoué si l'appelant borne le rejeu à un
    /// tour antérieur (CLI `kaji replay --until`, message d'erreur de
    /// `TruncatedAt` — S3 « replay jusqu'au tour N-1 possible »).
    /// `until_turn: None` refuse toute troncature, comme `load`.
    pub async fn load_until(
        session_manager: &SessionManager,
        session_id: &str,
        until_turn: Option<i64>,
    ) -> Result<Self> {
        let events = session_manager.session_events(session_id).await?;

        let log_meta = events
            .iter()
            .find(|event| event.kind == "log_meta")
            .and_then(payload)
            .and_then(|payload| serde_json::from_value::<LogMeta>(payload).ok());
        let Some(log_meta) = log_meta else {
            return Err(ReplayUnavailable::PreV2.into());
        };

        if !session_manager
            .get_session(session_id, false)
            .await?
            .replayable
        {
            return Err(ReplayUnavailable::Purged.into());
        }

        if let Some(interrupted) = session_manager.last_turn_is_interrupted(session_id).await? {
            let tolerated = until_turn.is_some_and(|until| until < interrupted.turn_seq);
            if !tolerated {
                return Err(ReplayUnavailable::TruncatedAt(interrupted.turn_seq).into());
            }
        }

        let mut cursor = Self {
            log_meta,
            llm_responses: HashMap::new(),
            tool_results: HashMap::new(),
            memory_blocks: HashMap::new(),
            tool_manifests: HashMap::new(),
            clock_reads: HashMap::new(),
            condense_turns: HashSet::new(),
            condense_summaries: HashMap::new(),
            approvals: HashMap::new(),
        };

        // Les deux moitiés d'un échange arrivent dans des events séparés :
        // `llm_request` porte le hash, `llm_response` les chunks. L'ordre du
        // journal les donne dans cet ordre, mais l'index n'en dépend pas.
        let mut hashes: HashMap<(i64, u32), String> = HashMap::new();

        for event in &events {
            let Some(payload) = payload(event) else {
                continue;
            };
            match event.kind.as_str() {
                "llm_request" => {
                    let (Some(key), Some(hash)) = (
                        call_key(event, &payload),
                        payload.get("request_hash").and_then(Value::as_str),
                    ) else {
                        continue;
                    };
                    hashes.insert(key, hash.to_string());
                }
                "llm_response" => {
                    let Some(key) = call_key(event, &payload) else {
                        continue;
                    };
                    cursor.llm_responses.insert(
                        key,
                        LlmExchange {
                            request_hash: hashes.get(&key).cloned().unwrap_or_default(),
                            chunks: payload
                                .get("chunks")
                                .and_then(Value::as_array)
                                .cloned()
                                .unwrap_or_default(),
                            finish: payload
                                .get("finish")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                        },
                    );
                }
                "tool_result" => {
                    let (Some(id), Some(result)) = (
                        payload.get("tool_call_id").and_then(Value::as_str),
                        payload.get("result"),
                    ) else {
                        continue;
                    };
                    cursor
                        .tool_results
                        .insert(id.to_string(), result.to_string());
                }
                "memory_block" => {
                    if let Some(block) = payload.get("block").and_then(Value::as_str) {
                        cursor
                            .memory_blocks
                            .insert(turn_seq(event, &payload), block.to_string());
                    }
                }
                "tool_manifest" => {
                    if let Ok(manifest) = serde_json::from_value::<ToolManifest>(payload.clone()) {
                        cursor
                            .tool_manifests
                            .insert(turn_seq(event, &payload), manifest);
                    }
                }
                "clock_reads" => {
                    if let Some(reads) = payload.get("reads").and_then(Value::as_array) {
                        let reads = reads
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect();
                        cursor.clock_reads.insert(turn_seq(event, &payload), reads);
                    }
                }
                "condense_triggered" => {
                    cursor.condense_turns.insert(turn_seq(event, &payload));
                }
                "condense_summary" => {
                    if let Some(summary) = payload
                        .get("summary")
                        .and_then(|summary| serde_json::from_value::<Message>(summary.clone()).ok())
                    {
                        cursor
                            .condense_summaries
                            .insert(turn_seq(event, &payload), summary);
                    }
                }
                "approval" => {
                    let (Some(request_id), Some(permission)) = (
                        payload.get("request_id").and_then(Value::as_str),
                        payload
                            .get("permission")
                            .and_then(|permission| {
                                serde_json::from_value::<Permission>(permission.clone()).ok()
                            })
                            .as_ref()
                            .map(Permission::allows_execution),
                    ) else {
                        continue;
                    };
                    cursor.approvals.insert(
                        (turn_seq(event, &payload), request_id.to_string()),
                        permission,
                    );
                }
                _ => {}
            }
        }

        Ok(cursor)
    }
}
