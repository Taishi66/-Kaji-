//! Rétention par kind du journal v2 : les payloads volumineux (requêtes et
//! réponses LLM, résultats d'outils, bloc mémoire, lectures d'horloge)
//! s'effacent passé la fenêtre de rétention, la structure du tour reste. Une
//! session amputée de ses payloads n'est plus rejouable exactement : elle est
//! marquée `replayable = 0` pour que le chargement du curseur la refuse avec
//! `ReplayUnavailable::Purged` plutôt que de rejouer un journal troué
//! (`docs/superpowers/plans/2026-08-27-event-log-v2-replay-exact.md`, Task 12).

use anyhow::Result;
use kaji::config::KajiMode;
use kaji::replay::cursor::{EventCursor, ReplayUnavailable};
use kaji::session::session_manager::{SessionType, DB_NAME, SESSIONS_FOLDER};
use kaji::session::SessionManager;
use tempfile::TempDir;

const PURGEABLE_KINDS: [&str; 6] = [
    "llm_request",
    "llm_response",
    "tool_result",
    "memory_block",
    "tool_manifest",
    "clock_reads",
];

const PERMANENT_KINDS: [&str; 6] = [
    "log_meta",
    "turn_start",
    "message",
    "usage",
    "condense_triggered",
    "turn_end",
];

const DAY_MS: i64 = 24 * 60 * 60 * 1000;

#[allow(clippy::disallowed_methods)] // test
fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

struct Fixture {
    data_dir: TempDir,
    session_manager: SessionManager,
}

impl Fixture {
    async fn new() -> Result<Self> {
        let data_dir = tempfile::tempdir()?;
        let session_manager = SessionManager::new(data_dir.path().join("data"));
        Ok(Self {
            data_dir,
            session_manager,
        })
    }

    /// Un tour complet enregistré : les cinq kinds purgeables et les kinds
    /// permanents qui décrivent le même tour.
    async fn recorded_session(&self, name: &str) -> Result<String> {
        let session = self
            .session_manager
            .create_session(
                self.data_dir.path().join("workspace"),
                name.to_string(),
                SessionType::Hidden,
                KajiMode::Auto,
            )
            .await?;

        self.session_manager
            .append_log_meta_if_absent(&session.id)
            .await?;
        for kind in ["turn_start", "message"]
            .into_iter()
            .chain(PURGEABLE_KINDS)
            .chain(["usage", "condense_triggered", "turn_end"])
        {
            self.session_manager
                .append_event(&session.id, 1, kind, "{}")
                .await?;
        }

        Ok(session.id)
    }

    /// Antidate tout le journal d'une session : l'API publique n'écrit que
    /// l'instant courant.
    async fn backdate(&self, session_id: &str, days: i64) -> Result<()> {
        let db_path = self
            .data_dir
            .path()
            .join("data")
            .join(SESSIONS_FOLDER)
            .join(DB_NAME);
        let pool = sqlx::SqlitePool::connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .filename(db_path)
                .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal),
        )
        .await?;
        sqlx::query("UPDATE session_events SET ts_ms = ? WHERE session_id = ?")
            .bind(now_ms() - days * DAY_MS)
            .bind(session_id)
            .execute(&pool)
            .await?;
        pool.close().await;
        Ok(())
    }

    async fn kinds(&self, session_id: &str) -> Result<Vec<String>> {
        Ok(self
            .session_manager
            .session_events(session_id)
            .await?
            .into_iter()
            .map(|event| event.kind)
            .collect())
    }

    async fn replayable(&self, session_id: &str) -> Result<bool> {
        Ok(self
            .session_manager
            .get_session(session_id, false)
            .await?
            .replayable)
    }
}

#[tokio::test]
async fn purge_drops_the_payload_kinds_and_keeps_the_turn_structure() -> Result<()> {
    let fixture = Fixture::new().await?;
    let old = fixture.recorded_session("old").await?;
    fixture.backdate(&old, 60).await?;

    let purged = fixture.session_manager.purge_replay_payloads(30).await?;
    assert_eq!(
        purged,
        PURGEABLE_KINDS.len() as u64,
        "un event par kind purgeable"
    );

    let kinds = fixture.kinds(&old).await?;
    for kind in PURGEABLE_KINDS {
        assert!(
            !kinds.contains(&kind.to_string()),
            "{kind} aurait dû partir"
        );
    }
    for kind in PERMANENT_KINDS {
        assert!(kinds.contains(&kind.to_string()), "{kind} aurait dû rester");
    }
    Ok(())
}

#[tokio::test]
async fn purge_marks_the_amputated_session_not_replayable() -> Result<()> {
    let fixture = Fixture::new().await?;
    let old = fixture.recorded_session("old").await?;
    fixture.backdate(&old, 60).await?;

    fixture.session_manager.purge_replay_payloads(30).await?;

    assert!(!fixture.replayable(&old).await?);
    let error = EventCursor::load(&fixture.session_manager, &old)
        .await
        .err()
        .expect("une session purgée ne se charge plus");
    assert_eq!(
        error.downcast_ref::<ReplayUnavailable>(),
        Some(&ReplayUnavailable::Purged),
        "{error}"
    );
    Ok(())
}

#[tokio::test]
async fn purge_leaves_the_sessions_inside_the_window_untouched() -> Result<()> {
    let fixture = Fixture::new().await?;
    let old = fixture.recorded_session("old").await?;
    let recent = fixture.recorded_session("recent").await?;
    fixture.backdate(&old, 60).await?;
    fixture.backdate(&recent, 2).await?;

    fixture.session_manager.purge_replay_payloads(30).await?;

    let kinds = fixture.kinds(&recent).await?;
    for kind in PURGEABLE_KINDS.into_iter().chain(PERMANENT_KINDS) {
        assert!(kinds.contains(&kind.to_string()), "{kind} aurait dû rester");
    }
    assert!(fixture.replayable(&recent).await?);
    Ok(())
}

#[tokio::test]
async fn a_negative_retention_never_purges() -> Result<()> {
    let fixture = Fixture::new().await?;
    let old = fixture.recorded_session("old").await?;
    fixture.backdate(&old, 3650).await?;

    let purged = fixture.session_manager.purge_replay_payloads(-1).await?;

    assert_eq!(purged, 0);
    let kinds = fixture.kinds(&old).await?;
    for kind in PURGEABLE_KINDS {
        assert!(kinds.contains(&kind.to_string()), "{kind} aurait dû rester");
    }
    assert!(fixture.replayable(&old).await?);
    Ok(())
}

#[tokio::test]
async fn a_zero_retention_purges_everything() -> Result<()> {
    let fixture = Fixture::new().await?;
    let recent = fixture.recorded_session("recent").await?;

    let purged = fixture.session_manager.purge_replay_payloads(0).await?;

    assert_eq!(purged, PURGEABLE_KINDS.len() as u64);
    assert!(!fixture.replayable(&recent).await?);
    Ok(())
}
