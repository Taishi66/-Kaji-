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
use kaji_providers::errors::ProviderError;
use serde::Deserialize;
use serde_json::Value;
use thiserror::Error;

use crate::conversation::message::Message;
use crate::permission::Permission;
use crate::replay::manifest::ToolManifest;
use crate::session::session_manager::SessionEvent;
use crate::session::SessionManager;
use crate::workflow::events::{GATE_DECISION, WORKFLOW_ARTIFACT};
use crate::workflow::gate::GateDecision;

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
    /// La variante d'erreur exacte, quand l'appel a échoué à l'enregistrement.
    /// Absente d'un journal antérieur au champ `error_kind` : le rejeu retombe
    /// alors sur une erreur d'exécution, comme avant.
    pub error: Option<ProviderError>,
}

/// Le journal v2 d'une session, prêt à être interrogé par clé.
pub struct EventCursor {
    pub log_meta: LogMeta,
    pub llm_responses: HashMap<(i64, u32), LlmExchange>,
    pub tool_results: HashMap<String, String>,
    /// Le bloc mémoire splicé dans le prompt système, par appel : la machine à
    /// états resplice avant chaque appel du tour, la boucle legacy une fois par
    /// tour (cf. [`per_call`]).
    pub memory_blocks: HashMap<(i64, u32), String>,
    /// Le bloc `turn-context` (`agents::moim`) de chaque appel : composé d'état
    /// vivant, il entre pourtant dans la requête hachée, et il bouge d'un appel
    /// à l'autre du même tour quand le budget de tours avance.
    pub turn_contexts: HashMap<(i64, u32), String>,
    /// L'environnement d'extensions de chaque appel : outils et fragments de
    /// prompt système qui en dérivent (`replay::manifest`). Adressé par appel
    /// parce qu'il bouge en cours de tour — une extension installée par le
    /// modèle, un hint découvert — sur les deux boucles.
    pub tool_manifests: HashMap<(i64, u32), ToolManifest>,
    /// La réponse rendue par la post-passe `toolshim`, par appel. Les chunks
    /// enregistrés sont ceux d'avant elle, et elle interroge un interpréteur
    /// vivant : le rejeu sert ce message au lieu de la relancer. Vide pour une
    /// session sans `toolshim` — et pour un journal antérieur à ce kind.
    pub toolshim_messages: HashMap<(i64, u32), Message>,
    pub clock_reads: HashMap<i64, Vec<String>>,
    pub condense_turns: HashSet<i64>,
    /// Le résumé que l'appel LLM de compaction a rendu. Cet appel passe par
    /// `Provider::complete`, hors du canal du provider, mais il est adressé par
    /// l'appel devant lequel il a été produit : un tour compacte à l'ouverture
    /// puis, sur `ContextLengthExceeded`, une seconde fois.
    pub condense_summaries: HashMap<(i64, u32), Message>,
    /// Le résumé qui a remplacé une paire d'outils, par `(turn_seq,
    /// tool_call_id)`. Il mute la conversation persistée — la paire devient
    /// invisible à l'agent, le résumé prend sa place — donc le tour suivant en
    /// dépend.
    pub tool_pair_summaries: HashMap<(i64, String), Message>,
    /// Les décisions d'approbation v1 du journal, par `(turn_seq, request_id)`,
    /// réduites à ce que le rejeu en fait : l'outil a-t-il eu le droit de
    /// tourner. Rejouées telles quelles, jamais redemandées à l'utilisateur
    /// (spec S4).
    pub approvals: HashMap<(i64, String), bool>,
    /// Les décisions des gates de workflow, par nom de stage. Même règle que
    /// les approbations d'outils : servies au rejeu, jamais redemandées. Un
    /// stage n'ouvre sa gate qu'une fois par exécution et une session parente
    /// ne porte qu'un workflow, donc le nom du stage suffit comme clé.
    pub gate_decisions: HashMap<String, GateDecision>,
    /// Les sorties d'agents du workflow, par `(stage, agent)`. Purgeable (c'est
    /// le seul payload volumineux de la famille) : absent d'un journal
    /// tronqué par la rétention.
    pub workflow_artifacts: HashMap<(String, String), String>,
}

/// Ce qu'un appel retrouve dans un index adressé par `(turn_seq, call_idx)` :
/// l'entrée de cet appel, ou à défaut la plus récente qui le précède dans le
/// tour. Les deux boucles n'assemblent pas au même rythme — la legacy compose
/// le bloc mémoire et le bloc turn-context une fois par tour, la machine à
/// états avant chaque appel — et chacune doit retrouver ce qu'elle avait sous
/// les yeux au moment de l'appel.
pub fn per_call<T>(index: &HashMap<(i64, u32), T>, turn_seq: i64, call_idx: u32) -> Option<&T> {
    (0..=call_idx)
        .rev()
        .find_map(|candidate| index.get(&(turn_seq, candidate)))
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
                            error: payload.get("error_kind").and_then(|kind| {
                                serde_json::from_value::<ProviderError>(kind.clone()).ok()
                            }),
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
                    let (Some(key), Some(block)) = (
                        call_key(event, &payload),
                        payload.get("block").and_then(Value::as_str),
                    ) else {
                        continue;
                    };
                    cursor.memory_blocks.insert(key, block.to_string());
                }
                "turn_context" => {
                    let (Some(key), Some(block)) = (
                        call_key(event, &payload),
                        payload.get("block").and_then(Value::as_str),
                    ) else {
                        continue;
                    };
                    cursor.turn_contexts.insert(key, block.to_string());
                }
                "tool_manifest" => {
                    let (Some(key), Ok(manifest)) = (
                        call_key(event, &payload),
                        serde_json::from_value::<ToolManifest>(payload.clone()),
                    ) else {
                        continue;
                    };
                    cursor.tool_manifests.insert(key, manifest);
                }
                "toolshim_message" => {
                    let (Some(key), Some(message)) = (
                        call_key(event, &payload),
                        payload.get("message").and_then(|message| {
                            serde_json::from_value::<Message>(message.clone()).ok()
                        }),
                    ) else {
                        continue;
                    };
                    cursor.toolshim_messages.insert(key, message);
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
                    let (Some(key), Some(summary)) = (
                        call_key(event, &payload),
                        payload.get("summary").and_then(|summary| {
                            serde_json::from_value::<Message>(summary.clone()).ok()
                        }),
                    ) else {
                        continue;
                    };
                    cursor.condense_summaries.insert(key, summary);
                }
                "tool_pair_summary" => {
                    let (Some(tool_call_id), Some(summary)) = (
                        payload.get("tool_call_id").and_then(Value::as_str),
                        payload.get("summary").and_then(|summary| {
                            serde_json::from_value::<Message>(summary.clone()).ok()
                        }),
                    ) else {
                        continue;
                    };
                    cursor.tool_pair_summaries.insert(
                        (turn_seq(event, &payload), tool_call_id.to_string()),
                        summary,
                    );
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
                GATE_DECISION => {
                    let (Some(stage), Some(decision)) = (
                        payload.get("stage").and_then(Value::as_str),
                        payload.get("decision").and_then(|decision| {
                            serde_json::from_value::<GateDecision>(decision.clone()).ok()
                        }),
                    ) else {
                        continue;
                    };
                    cursor.gate_decisions.insert(stage.to_string(), decision);
                }
                WORKFLOW_ARTIFACT => {
                    let (Some(stage), Some(agent), Some(output)) = (
                        payload.get("stage").and_then(Value::as_str),
                        payload.get("agent").and_then(Value::as_str),
                        payload.get("output").and_then(Value::as_str),
                    ) else {
                        continue;
                    };
                    cursor
                        .workflow_artifacts
                        .insert((stage.to_string(), agent.to_string()), output.to_string());
                }
                _ => {}
            }
        }

        Ok(cursor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::Tool;

    fn manifest(tool: &str) -> ToolManifest {
        ToolManifest {
            tools: vec![Tool::new(tool.to_string(), "", serde_json::Map::new())],
            ..ToolManifest::default()
        }
    }

    fn tool_of(manifest: Option<&ToolManifest>) -> Option<String> {
        Some(manifest?.tools.first()?.name.to_string())
    }

    /// La règle de péremption vaut pour tout index adressé par appel, manifeste
    /// d'outils compris : l'entrée de l'appel gagne, à défaut la plus récente
    /// qui le précède **dans le tour** — jamais celle d'un autre tour.
    #[test]
    fn a_per_call_index_serves_the_entry_of_the_call_then_the_latest_before_it() {
        let index = HashMap::from([
            ((1, 0), manifest("avant")),
            ((1, 2), manifest("après installation")),
            ((2, 0), manifest("autre tour")),
        ]);

        assert_eq!(
            tool_of(per_call(&index, 1, 0)).as_deref(),
            Some("avant"),
            "l'appel 0 lit son propre manifeste"
        );
        assert_eq!(
            tool_of(per_call(&index, 1, 1)).as_deref(),
            Some("avant"),
            "un appel sans manifeste propre garde celui qui le précède"
        );
        assert_eq!(
            tool_of(per_call(&index, 1, 2)).as_deref(),
            Some("après installation"),
            "l'appel qui a réassemblé lit le manifeste réassemblé, pas celui de l'appel 0"
        );
        assert_eq!(
            per_call(&index, 3, 5).map(|_| ()),
            None,
            "un tour sans entrée n'emprunte pas celle d'un autre"
        );
    }
}
