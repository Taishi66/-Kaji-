use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use kaji_providers::errors::ProviderError;
use serde_json::{json, Value};
use tracing::warn;

use crate::conversation::message::Message;
use crate::replay::manifest::ToolManifest;
use crate::session::SessionManager;

fn parse_or_wrap(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
}

/// Écrit les kinds v2 de l'event log (`llm_request`, `llm_response`,
/// `tool_result`, `memory_block`, `tool_manifest`, `clock_reads`,
/// `condense_triggered`, `condense_summary`, `turn_context`) —
/// voir `docs/superpowers/specs/2026-08-27-event-log-v2-replay-exact-design.md`.
/// Toutes les méthodes sont non-fatales : un échec d'écriture ne remonte
/// jamais au tour en cours, il marque la session non rejouable.
#[derive(Clone)]
pub struct RecordSink {
    session_manager: Arc<SessionManager>,
    session_id: String,
}

impl RecordSink {
    pub fn new(session_manager: Arc<SessionManager>, session_id: String) -> Self {
        Self {
            session_manager,
            session_id,
        }
    }

    async fn append(&self, turn_seq: i64, kind: &str, payload: Value) {
        let payload_json = payload.to_string();
        if let Err(e) = self
            .session_manager
            .append_event(&self.session_id, turn_seq, kind, &payload_json)
            .await
        {
            warn!(
                error = %e,
                kind,
                session_id = %self.session_id,
                "event log v2: écriture échouée — session marquée non rejouable"
            );
            if let Err(e2) = self
                .session_manager
                .mark_not_replayable(&self.session_id)
                .await
            {
                warn!(
                    error = %e2,
                    session_id = %self.session_id,
                    "event log v2: mark_not_replayable a aussi échoué"
                );
            }
        }
    }

    pub async fn record_llm_request(
        &self,
        turn_seq: i64,
        call_idx: u32,
        request_hash: &str,
        model: &str,
        provider: &str,
    ) {
        self.append(
            turn_seq,
            "llm_request",
            json!({
                "turn_seq": turn_seq,
                "call_idx": call_idx,
                "request_hash": request_hash,
                "model": model,
                "provider": provider,
            }),
        )
        .await;
    }

    pub async fn record_llm_response(
        &self,
        turn_seq: i64,
        call_idx: u32,
        chunks_json: &str,
        finish: &str,
    ) {
        self.append(
            turn_seq,
            "llm_response",
            json!({
                "turn_seq": turn_seq,
                "call_idx": call_idx,
                "chunks": parse_or_wrap(chunks_json),
                "finish": finish,
            }),
        )
        .await;
    }

    /// Un appel provider qui a échoué, journalisé sous le même kind que les
    /// autres avec `finish: "error"`. `error_kind` porte la variante exacte :
    /// les deux boucles choisissent leur bras de `match` dessus — compaction de
    /// secours sur `ContextLengthExceeded`, notification sur `CreditsExhausted`,
    /// message d'erreur sinon — et le rejeu doit prendre le même.
    pub async fn record_llm_error(&self, turn_seq: i64, call_idx: u32, error: &ProviderError) {
        self.append(
            turn_seq,
            "llm_response",
            json!({
                "turn_seq": turn_seq,
                "call_idx": call_idx,
                "chunks": Vec::<Value>::new(),
                "finish": "error",
                "error": error.to_string(),
                "error_kind": error,
            }),
        )
        .await;
    }

    pub async fn record_tool_result(&self, turn_seq: i64, tool_call_id: &str, result_json: &str) {
        self.append(
            turn_seq,
            "tool_result",
            json!({
                "turn_seq": turn_seq,
                "tool_call_id": tool_call_id,
                "result": parse_or_wrap(result_json),
            }),
        )
        .await;
    }

    pub async fn record_memory_block(&self, turn_seq: i64, call_idx: u32, block: &str) {
        self.append(
            turn_seq,
            "memory_block",
            json!({
                "turn_seq": turn_seq,
                "call_idx": call_idx,
                "block": block,
            }),
        )
        .await;
    }

    pub async fn record_tool_manifest(
        &self,
        turn_seq: i64,
        call_idx: u32,
        manifest: &ToolManifest,
    ) {
        let mut payload = serde_json::to_value(manifest).unwrap_or_else(|_| json!({}));
        if let Some(object) = payload.as_object_mut() {
            object.insert("turn_seq".to_string(), json!(turn_seq));
            object.insert("call_idx".to_string(), json!(call_idx));
        }
        self.append(turn_seq, "tool_manifest", payload).await;
    }

    pub async fn record_clock_reads(&self, turn_seq: i64, reads: &[String]) {
        self.append(
            turn_seq,
            "clock_reads",
            json!({
                "turn_seq": turn_seq,
                "reads": reads,
            }),
        )
        .await;
    }

    pub async fn record_turn_context(&self, turn_seq: i64, call_idx: u32, block: &str) {
        self.append(
            turn_seq,
            "turn_context",
            json!({
                "turn_seq": turn_seq,
                "call_idx": call_idx,
                "block": block,
            }),
        )
        .await;
    }

    pub async fn record_condense_summary(&self, turn_seq: i64, call_idx: u32, summary: &Message) {
        self.append(
            turn_seq,
            "condense_summary",
            json!({
                "turn_seq": turn_seq,
                "call_idx": call_idx,
                "summary": summary,
            }),
        )
        .await;
    }

    pub async fn record_tool_pair_summary(
        &self,
        turn_seq: i64,
        tool_call_id: &str,
        summary: &Message,
    ) {
        self.append(
            turn_seq,
            "tool_pair_summary",
            json!({
                "turn_seq": turn_seq,
                "tool_call_id": tool_call_id,
                "summary": summary,
            }),
        )
        .await;
    }

    pub async fn record_condense_triggered(&self, turn_seq: i64, reason: &str) {
        self.append(
            turn_seq,
            "condense_triggered",
            json!({
                "turn_seq": turn_seq,
                "reason": reason,
            }),
        )
        .await;
    }
}

/// Capture d'un tour : le sink, le `turn_seq` alloué par l'enveloppe
/// `Agent::reply()` et le compteur d'appels LLM du tour. L'enveloppe en crée un
/// par tour, donc le compteur repart de 0 à chaque tour par construction.
pub struct TurnRecorder {
    sink: RecordSink,
    turn_seq: i64,
    next_call_idx: AtomicU32,
}

impl TurnRecorder {
    pub fn new(session_manager: Arc<SessionManager>, session_id: String, turn_seq: i64) -> Self {
        Self {
            sink: RecordSink::new(session_manager, session_id),
            turn_seq,
            next_call_idx: AtomicU32::new(0),
        }
    }

    /// L'appel LLM que la boucle est en train d'assembler, sans le réserver :
    /// `next_llm_call` le consommera juste après.
    fn current_call_idx(&self) -> u32 {
        self.next_call_idx.load(Ordering::SeqCst)
    }

    pub fn tool_capture(&self, tool_call_id: &str) -> ToolCapture {
        ToolCapture {
            sink: self.sink.clone(),
            turn_seq: self.turn_seq,
            tool_call_id: tool_call_id.to_string(),
        }
    }
}

/// Où et sous quelle clé écrire le `tool_result` d'un appel d'outil : le sink,
/// le tour, et l'id de corrélation (`ToolResponse.id`) auquel le rejeu
/// s'adressera.
pub struct ToolCapture {
    pub sink: RecordSink,
    pub turn_seq: i64,
    pub tool_call_id: String,
}

/// Réserve le prochain `call_idx` du tour et rend les deux paramètres de
/// capture de `stream_response_from_provider`. Point unique partagé par les
/// deux boucles : chacune n'en porte qu'un appel, aucune logique d'adressage
/// n'est dupliquée dans les fichiers de boucle.
pub fn next_llm_call(
    recorder: Option<&Arc<TurnRecorder>>,
) -> (Option<RecordSink>, Option<(i64, u32)>) {
    let Some(recorder) = recorder else {
        return (None, None);
    };
    let call_idx = recorder.next_call_idx.fetch_add(1, Ordering::SeqCst);
    (
        Some(recorder.sink.clone()),
        Some((recorder.turn_seq, call_idx)),
    )
}

/// Records the memory block spliced into the system prompt of the call the
/// loop is assembling. Keyed by `(turn_seq, call_idx)`: the state-machine loop
/// re-splices before every provider call of the turn, on a conversation that
/// has grown since the last one, so the recall can differ from call to call.
pub async fn record_memory_block(recorder: Option<&Arc<TurnRecorder>>, block: Option<&str>) {
    let (Some(recorder), Some(block)) = (recorder, block) else {
        return;
    };
    recorder
        .sink
        .record_memory_block(recorder.turn_seq, recorder.current_call_idx(), block)
        .await;
}

/// Records the tool environment presented to the model for the call the loop is
/// assembling. Keyed by `(turn_seq, call_idx)`: the environment moves inside a
/// turn — the model installs an extension, a subdirectory hint appears — and
/// both loops reassemble before the calls that follow.
pub async fn record_tool_manifest(recorder: Option<&Arc<TurnRecorder>>, manifest: &ToolManifest) {
    let Some(recorder) = recorder else {
        return;
    };
    recorder
        .sink
        .record_tool_manifest(recorder.turn_seq, recorder.current_call_idx(), manifest)
        .await;
}

/// Records the summary the compaction LLM call produced. The call itself goes
/// through `Provider::complete`, off the loop's `next_llm_call` channel, but it
/// is addressed by the call it ran in front of: a turn compacts on opening and
/// again as salvage on `ContextLengthExceeded`.
pub async fn record_condense_summary(recorder: Option<&Arc<TurnRecorder>>, summary: &Message) {
    let Some(recorder) = recorder else {
        return;
    };
    recorder
        .sink
        .record_condense_summary(recorder.turn_seq, recorder.current_call_idx(), summary)
        .await;
}

/// Records the turn-context block that goes in front of the call the loop is
/// assembling. Keyed by `(turn_seq, call_idx)` like the LLM exchange it
/// belongs to: the state-machine loop recomposes before every provider call of
/// the turn and the block moves with the turn budget, so one block per turn
/// would not be enough to replay the turn's later calls.
pub async fn record_turn_context(recorder: Option<&Arc<TurnRecorder>>, block: Option<String>) {
    let (Some(recorder), Some(block)) = (recorder, block) else {
        return;
    };
    recorder
        .sink
        .record_turn_context(recorder.turn_seq, recorder.current_call_idx(), &block)
        .await;
}

/// Records the summary that replaced one tool request/response pair. Like the
/// compaction summary it comes from an LLM call off the loop's channel, but it
/// is addressed by the pair it replaces: the summary is persisted into the
/// conversation and the pair is hidden, so the next turn's request depends on
/// it.
pub async fn record_tool_pair_summary(
    recorder: Option<&Arc<TurnRecorder>>,
    tool_call_id: &str,
    summary: &Message,
) {
    let Some(recorder) = recorder else {
        return;
    };
    recorder
        .sink
        .record_tool_pair_summary(recorder.turn_seq, tool_call_id, summary)
        .await;
}

pub async fn record_clock_reads(recorder: Option<&Arc<TurnRecorder>>, reads: &[String]) {
    let Some(recorder) = recorder else {
        return;
    };
    recorder
        .sink
        .record_clock_reads(recorder.turn_seq, reads)
        .await;
}

/// Records why this turn compacted. `None` is the ordinary case — the turn
/// stayed under the threshold — and journals nothing.
pub async fn record_condense_triggered(recorder: Option<&Arc<TurnRecorder>>, reason: Option<&str>) {
    let (Some(recorder), Some(reason)) = (recorder, reason) else {
        return;
    };
    recorder
        .sink
        .record_condense_triggered(recorder.turn_seq, reason)
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::KajiMode;
    use crate::session::session_manager::SessionType;
    use std::path::PathBuf;
    use tempfile::TempDir;

    async fn temp_session() -> (TempDir, Arc<SessionManager>, crate::session::Session) {
        let temp_dir = TempDir::new().unwrap();
        let mgr = Arc::new(SessionManager::new(temp_dir.path().to_path_buf()));
        let session = mgr
            .create_session(
                PathBuf::from("/tmp/record-sink-test"),
                "record sink test".to_string(),
                SessionType::User,
                KajiMode::default(),
            )
            .await
            .unwrap();
        (temp_dir, mgr, session)
    }

    #[tokio::test]
    async fn write_failure_is_nonfatal_and_marks_session_not_replayable() {
        let (_tmp, mgr, session) = temp_session().await;
        let sink = RecordSink::new(mgr.clone(), session.id.clone());

        // Casse spécifiquement le chemin d'écriture des events (la table
        // `sessions` reste intacte) — `mark_not_replayable` doit donc
        // réussir même si l'écriture de l'event a échoué.
        sqlx::query("DROP TABLE session_events")
            .execute(mgr.storage().pool().await.unwrap())
            .await
            .unwrap();

        sink.record_tool_result(1, "call_broken", r#"{"ok":true}"#)
            .await;

        let reloaded = mgr.get_session(&session.id, false).await.unwrap();
        assert!(
            !reloaded.replayable,
            "un échec d'écriture d'event doit marquer la session non rejouable"
        );
    }
}
