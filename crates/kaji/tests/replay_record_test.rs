use kaji::config::KajiMode;
use kaji::replay::record::RecordSink;
use kaji::session::session_manager::SessionType;
use kaji::session::{Session, SessionManager};
use std::path::PathBuf;
use std::sync::Arc;
use tempfile::TempDir;

async fn temp_session() -> (TempDir, Arc<SessionManager>, Session) {
    let temp_dir = TempDir::new().unwrap();
    let mgr = Arc::new(SessionManager::new(temp_dir.path().to_path_buf()));
    let session = mgr
        .create_session(
            PathBuf::from("/tmp/replay-record-test"),
            "replay record test".to_string(),
            SessionType::User,
            KajiMode::default(),
        )
        .await
        .unwrap();
    (temp_dir, mgr, session)
}

#[tokio::test]
async fn tool_result_roundtrip() {
    let (_tmp, mgr, session) = temp_session().await;
    let sink = RecordSink::new(mgr.clone(), session.id.clone());
    sink.record_tool_result(1, "call_42", r#"{"ok":true}"#)
        .await;
    let events = mgr.session_events(&session.id).await.unwrap();
    let ev = events.iter().find(|e| e.kind == "tool_result").unwrap();
    assert!(ev.payload_json.contains("call_42"));
}

#[tokio::test]
async fn llm_request_payload_carries_addressing_keys() {
    let (_tmp, mgr, session) = temp_session().await;
    let sink = RecordSink::new(mgr.clone(), session.id.clone());
    sink.record_llm_request(3, 1, "deadbeef", "gpt-5", "openai")
        .await;
    let events = mgr.session_events(&session.id).await.unwrap();
    let ev = events.iter().find(|e| e.kind == "llm_request").unwrap();
    assert_eq!(ev.turn_seq, 3);
    let payload: serde_json::Value = serde_json::from_str(&ev.payload_json).unwrap();
    assert_eq!(payload["turn_seq"], 3);
    assert_eq!(payload["call_idx"], 1);
    assert_eq!(payload["request_hash"], "deadbeef");
    assert_eq!(payload["model"], "gpt-5");
    assert_eq!(payload["provider"], "openai");
}

#[tokio::test]
async fn llm_response_payload_embeds_chunks_as_json() {
    let (_tmp, mgr, session) = temp_session().await;
    let sink = RecordSink::new(mgr.clone(), session.id.clone());
    sink.record_llm_response(2, 0, r#"[{"text":"hi"}]"#, "stop")
        .await;
    let events = mgr.session_events(&session.id).await.unwrap();
    let ev = events.iter().find(|e| e.kind == "llm_response").unwrap();
    let payload: serde_json::Value = serde_json::from_str(&ev.payload_json).unwrap();
    assert_eq!(payload["turn_seq"], 2);
    assert_eq!(payload["call_idx"], 0);
    assert_eq!(payload["finish"], "stop");
    assert_eq!(payload["chunks"][0]["text"], "hi");
}

#[tokio::test]
async fn memory_block_payload_keeps_block_verbatim() {
    let (_tmp, mgr, session) = temp_session().await;
    let sink = RecordSink::new(mgr.clone(), session.id.clone());
    sink.record_memory_block(1, "# facts\n- foo").await;
    let events = mgr.session_events(&session.id).await.unwrap();
    let ev = events.iter().find(|e| e.kind == "memory_block").unwrap();
    let payload: serde_json::Value = serde_json::from_str(&ev.payload_json).unwrap();
    assert_eq!(payload["turn_seq"], 1);
    assert_eq!(payload["block"], "# facts\n- foo");
}

#[tokio::test]
async fn clock_reads_payload_preserves_order() {
    let (_tmp, mgr, session) = temp_session().await;
    let sink = RecordSink::new(mgr.clone(), session.id.clone());
    let reads = vec![
        "2026-08-27 09:00 +00:00".to_string(),
        "2026-08-27 10:00 +00:00".to_string(),
    ];
    sink.record_clock_reads(1, &reads).await;
    let events = mgr.session_events(&session.id).await.unwrap();
    let ev = events.iter().find(|e| e.kind == "clock_reads").unwrap();
    let payload: serde_json::Value = serde_json::from_str(&ev.payload_json).unwrap();
    assert_eq!(payload["reads"], serde_json::json!(reads));
}

#[tokio::test]
async fn condense_triggered_payload_carries_reason() {
    let (_tmp, mgr, session) = temp_session().await;
    let sink = RecordSink::new(mgr.clone(), session.id.clone());
    sink.record_condense_triggered(4, "token_budget_exceeded")
        .await;
    let events = mgr.session_events(&session.id).await.unwrap();
    let ev = events
        .iter()
        .find(|e| e.kind == "condense_triggered")
        .unwrap();
    let payload: serde_json::Value = serde_json::from_str(&ev.payload_json).unwrap();
    assert_eq!(payload["turn_seq"], 4);
    assert_eq!(payload["reason"], "token_budget_exceeded");
}
