use kaji::config::KajiMode;
use kaji::session::session_manager::SessionType;
use kaji::session::{Session, SessionManager};
use std::path::PathBuf;
use tempfile::TempDir;

async fn temp_session() -> (TempDir, SessionManager, Session) {
    let temp_dir = TempDir::new().unwrap();
    let mgr = SessionManager::new(temp_dir.path().to_path_buf());
    let session = mgr
        .create_session(
            PathBuf::from("/tmp/replay-schema-test"),
            "replay schema test".to_string(),
            SessionType::User,
            KajiMode::default(),
        )
        .await
        .unwrap();
    (temp_dir, mgr, session)
}

#[tokio::test]
async fn log_meta_written_once() {
    let (_tmp, mgr, session) = temp_session().await;
    mgr.append_log_meta_if_absent(&session.id).await.unwrap();
    mgr.append_log_meta_if_absent(&session.id).await.unwrap();
    let events = mgr.session_events(&session.id).await.unwrap();
    assert_eq!(events.iter().filter(|e| e.kind == "log_meta").count(), 1);
}

#[tokio::test]
async fn mark_not_replayable_flips_flag() {
    let (_tmp, mgr, session) = temp_session().await;
    assert!(
        mgr.get_session(&session.id, false)
            .await
            .unwrap()
            .replayable
    );
    mgr.mark_not_replayable(&session.id).await.unwrap();
    assert!(
        !mgr.get_session(&session.id, false)
            .await
            .unwrap()
            .replayable
    );
}

#[tokio::test]
async fn mark_not_replayable_errors_on_unknown_session() {
    let (_tmp, mgr, _session) = temp_session().await;
    assert!(mgr.mark_not_replayable("does-not-exist").await.is_err());
}

// `idx_session_events_turn_alloc` (migration 17) enforces the real
// allocation invariant: two turns must never both claim the same turn_seq
// for the same session. It is deliberately *not* a plain
// `UNIQUE(session_id, turn_seq)` — a turn legitimately owns several rows
// (turn_start, message(s), checkpoint, turn_end) sharing one turn_seq — so
// the index is scoped to the one kind that marks a claim.
#[tokio::test]
async fn second_turn_start_for_the_same_turn_seq_is_rejected() {
    let (_tmp, mgr, session) = temp_session().await;
    mgr.append_event(&session.id, 1, "turn_start", "{}")
        .await
        .unwrap();
    let collision = mgr.append_event(&session.id, 1, "turn_start", "{}").await;
    assert!(
        collision.is_err(),
        "two turn_start rows must never share (session_id, turn_seq)"
    );
}

#[tokio::test]
async fn non_turn_start_kinds_may_repeat_within_the_same_turn() {
    let (_tmp, mgr, session) = temp_session().await;
    mgr.append_event(&session.id, 1, "turn_start", "{}")
        .await
        .unwrap();
    mgr.append_event(&session.id, 1, "message", "{}")
        .await
        .unwrap();
    mgr.append_event(&session.id, 1, "message", "{}")
        .await
        .unwrap();
    let events = mgr.session_events(&session.id).await.unwrap();
    assert_eq!(events.iter().filter(|e| e.kind == "message").count(), 2);
}

#[tokio::test]
async fn turn_start_may_repeat_across_different_sessions_for_the_same_turn_seq() {
    let (tmp, mgr, session_a) = temp_session().await;
    let session_b = mgr
        .create_session(
            tmp.path().to_path_buf(),
            "second session".to_string(),
            SessionType::User,
            KajiMode::default(),
        )
        .await
        .unwrap();

    mgr.append_event(&session_a.id, 1, "turn_start", "{}")
        .await
        .unwrap();
    mgr.append_event(&session_b.id, 1, "turn_start", "{}")
        .await
        .unwrap();

    assert_eq!(mgr.session_events(&session_a.id).await.unwrap().len(), 1);
    assert_eq!(mgr.session_events(&session_b.id).await.unwrap().len(), 1);
}

#[tokio::test]
async fn session_events_orders_by_turn_seq_then_id() {
    let (_tmp, mgr, session) = temp_session().await;
    mgr.append_event(&session.id, 2, "turn_start", "{}")
        .await
        .unwrap();
    mgr.append_event(&session.id, 1, "turn_start", "{}")
        .await
        .unwrap();
    let events = mgr.session_events(&session.id).await.unwrap();
    let seqs: Vec<i64> = events.iter().map(|e| e.turn_seq).collect();
    assert_eq!(
        seqs,
        vec![1, 2],
        "ordonné par turn_seq puis id, pas id seul"
    );
}
