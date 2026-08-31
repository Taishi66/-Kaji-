//! `EventCursor::load_until` : une troncature de fin de journal (dernier tour
//! sans `turn_end`) est refusée par défaut, comme `load`, mais tolérée quand
//! l'appelant borne explicitement le rejeu à un tour antérieur au tour
//! interrompu — c'est ce qui rend vrai le message d'erreur de `TruncatedAt`
//! (« replay jusqu'au tour N-1 possible avec --until »), que le CLI de rejeu
//! (Task 11) affiche.

use anyhow::Result;
use kaji::config::KajiMode;
use kaji::replay::cursor::{EventCursor, ReplayUnavailable};
use kaji::session::session_manager::SessionType;
use kaji::session::SessionManager;
use tempfile::TempDir;

/// Session à deux tours : le premier complet (`turn_start` + `turn_end`), le
/// second interrompu (`turn_start` seul, comme un crash pendant
/// l'enregistrement).
async fn session_truncated_at_turn_2() -> Result<(TempDir, SessionManager, String)> {
    let data_dir = tempfile::tempdir()?;
    let session_manager = SessionManager::new(data_dir.path().join("data"));
    let session = session_manager
        .create_session(
            data_dir.path().join("workspace"),
            "replay-cursor-until-test".to_string(),
            SessionType::Hidden,
            KajiMode::Auto,
        )
        .await?;

    session_manager
        .append_log_meta_if_absent(&session.id)
        .await?;
    session_manager
        .append_event(&session.id, 1, "turn_start", "{}")
        .await?;
    session_manager
        .append_event(&session.id, 1, "turn_end", "{}")
        .await?;
    session_manager
        .append_event(&session.id, 2, "turn_start", "{}")
        .await?;

    Ok((data_dir, session_manager, session.id))
}

#[tokio::test]
async fn load_refuses_the_truncated_tail_with_no_bound() -> Result<()> {
    let (_data_dir, session_manager, session_id) = session_truncated_at_turn_2().await?;

    let result = EventCursor::load(&session_manager, &session_id).await;
    let error = result.unwrap_err_for_test("an unbounded load never tolerates a truncated tail");

    assert_eq!(
        error.downcast_ref::<ReplayUnavailable>(),
        Some(&ReplayUnavailable::TruncatedAt(2))
    );
    Ok(())
}

#[tokio::test]
async fn load_until_refuses_the_truncated_tail_when_the_bound_reaches_it() -> Result<()> {
    let (_data_dir, session_manager, session_id) = session_truncated_at_turn_2().await?;

    for until in [None, Some(2), Some(3)] {
        let result = EventCursor::load_until(&session_manager, &session_id, until).await;
        let error = result.unwrap_err_for_test(&format!("until={until:?} still reaches turn 2"));
        assert_eq!(
            error.downcast_ref::<ReplayUnavailable>(),
            Some(&ReplayUnavailable::TruncatedAt(2)),
            "until={until:?}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn load_until_serves_the_safe_prefix_when_the_bound_excludes_the_truncated_tail() -> Result<()>
{
    let (_data_dir, session_manager, session_id) = session_truncated_at_turn_2().await?;

    let cursor = EventCursor::load_until(&session_manager, &session_id, Some(1))
        .await
        .expect("until=1 stays strictly before the interrupted turn 2");
    assert_eq!(cursor.log_meta.idgen_seed, session_id);
    Ok(())
}

trait UnwrapErrForTest<T> {
    fn unwrap_err_for_test(self, message: &str) -> anyhow::Error;
}

impl<T> UnwrapErrForTest<T> for Result<T> {
    fn unwrap_err_for_test(self, message: &str) -> anyhow::Error {
        match self {
            Ok(_) => panic!("{message}: expected an error, got Ok"),
            Err(error) => error,
        }
    }
}
