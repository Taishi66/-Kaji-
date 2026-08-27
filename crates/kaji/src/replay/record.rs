use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use serde_json::{json, Value};
use tracing::warn;

use crate::session::SessionManager;

fn parse_or_wrap(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
}

/// Écrit les kinds v2 de l'event log (`llm_request`, `llm_response`,
/// `tool_result`, `memory_block`, `clock_reads`, `condense_triggered`) —
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

    pub async fn record_memory_block(&self, turn_seq: i64, block: &str) {
        self.append(
            turn_seq,
            "memory_block",
            json!({
                "turn_seq": turn_seq,
                "block": block,
            }),
        )
        .await;
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
    memory_block_recorded: AtomicBool,
}

impl TurnRecorder {
    pub fn new(session_manager: Arc<SessionManager>, session_id: String, turn_seq: i64) -> Self {
        Self {
            sink: RecordSink::new(session_manager, session_id),
            turn_seq,
            next_call_idx: AtomicU32::new(0),
            memory_block_recorded: AtomicBool::new(false),
        }
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

/// Records the memory block spliced into this turn's system prompt. Only the
/// turn's first splice is journaled: the state-machine loop re-splices before
/// every provider call of the turn, the legacy loop splices once, and the log
/// carries one block per turn on both.
pub async fn record_memory_block(recorder: Option<&Arc<TurnRecorder>>, block: Option<&str>) {
    let (Some(recorder), Some(block)) = (recorder, block) else {
        return;
    };
    if recorder.memory_block_recorded.swap(true, Ordering::SeqCst) {
        return;
    }
    recorder
        .sink
        .record_memory_block(recorder.turn_seq, block)
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
