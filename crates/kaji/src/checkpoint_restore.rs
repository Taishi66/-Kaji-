use crate::checkpoint::{CheckpointId, CheckpointStore};
use crate::session::session_manager::{SessionEvent, SessionManager};
use anyhow::{bail, Context, Result};
use tracing::warn;

/// Outcome of a successful [`restore_checkpoint`]: the `turn_seq` of the
/// checkpoint that was restored to (i.e. the turn whose pre-turn state the
/// working tree and conversation now reflect).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreOutcome {
    pub restored_turn: i64,
}

/// Finds `target`'s `checkpoint` event in `events` and returns its
/// `(boundary_message_id, turn_seq)`. `turn_seq` comes off the event row
/// itself (not the payload) — it is the same column every other event kind
/// uses. `session_id` is only for the error message.
fn checkpoint_boundary(
    events: &[SessionEvent],
    session_id: &str,
    target: &CheckpointId,
) -> Result<(Option<String>, i64)> {
    for event in events {
        if event.kind != "checkpoint" {
            continue;
        }
        let Ok(payload) = serde_json::from_str::<serde_json::Value>(&event.payload_json) else {
            continue;
        };
        if payload.get("checkpoint_id").and_then(|v| v.as_str()) != Some(target.0.as_str()) {
            continue;
        }
        let boundary_message_id = payload
            .get("boundary_message_id")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        return Ok((boundary_message_id, event.turn_seq));
    }
    bail!(
        "checkpoint {} introuvable pour la session {session_id}",
        target.0
    )
}

/// The store's git I/O is synchronous subprocess work. `block_in_place`
/// keeps the executor healthy on the multi-thread runtime but *panics* on a
/// current-thread one — there, run inline: that runtime has a single caller
/// by construction (tokio-test defaults, one-shot tools), and briefly
/// blocking it is the only remaining option.
fn run_store_blocking<T>(f: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(f)
        }
        _ => f(),
    }
}

/// Journals a pre-restore snapshot as a `checkpoint` event (payload shape of
/// `Agent::snapshot_checkpoint`, `captured: "pre_restore"`), on the
/// session's latest `turn_seq`. Best-effort: a failed append costs only the
/// *visibility* of the net (the snapshot itself is already in the store),
/// never the restore.
async fn journal_pre_restore(
    sm: &SessionManager,
    session_id: &str,
    events: &[SessionEvent],
    id: CheckpointId,
    tree_sha: String,
) {
    let boundary_message_id = match sm.last_message_id(session_id).await {
        Ok(id) => id,
        Err(error) => {
            warn!(
                ?error,
                "checkpoint: last_message_id lookup failed, pre-restore boundary will be null"
            );
            None
        }
    };
    let turn_seq = events.iter().map(|event| event.turn_seq).max().unwrap_or(0);
    let payload = serde_json::json!({
        "checkpoint_id": id.0,
        "tree_sha": tree_sha,
        "captured": "pre_restore",
        "boundary_message_id": boundary_message_id,
    })
    .to_string();
    if let Err(error) = sm
        .append_event(session_id, turn_seq, "checkpoint", &payload)
        .await
    {
        warn!(?error, "event log append failed for pre-restore checkpoint");
    }
}

/// Restores a project's working tree *and* its session's conversation to the
/// state captured by `target`, as one logical transaction.
///
/// The order below — and step 5's fatality in particular — are safety
/// invariants, not style choices: see premortem PM2
/// (`09 - Meta/premortems/kaji-checkpoints-2026-08-12.md`), which is exactly
/// the "restore left files and conversation out of sync" failure mode this
/// function exists to prevent.
///
/// 1. **Pre-restore snapshot** ("undo-the-undo"), taken before any mutation
///    of the project or the conversation. Best-effort: if it fails, the only
///    thing lost is the safety net for *this* restore — the restore itself
///    must still proceed (`warn!`, never `?`).
/// 2. **Read-only gates**, all before any mutation:
///    - the target's `checkpoint` event must exist;
///    - its `boundary_message_id` must be non-`null` (no message had been
///      persisted yet when it was captured) — otherwise the conversation
///      half of a coupled restore is meaningless: refuse loudly rather than
///      truncate at a guess;
///    - the boundary message must *still exist* in `messages`: compaction
///      (or a previous restore's truncation) may have deleted it, and
///      `truncate_conversation_from_message` on a vanished id is a silent
///      no-op — the restore would then "succeed" having restored files but
///      left the conversation untouched, the exact PM2 divergence.
/// 3. **Journal the pre-restore snapshot** as a `checkpoint` event — after
///    the gates (a refused restore leaves no event noise), before the git
///    restore (if step 5 fails, the net is already visible in
///    `/checkpoints`). Its own boundary points at a message step 5 is about
///    to delete, so a coupled restore *back* will refuse at gate 2c: the net
///    is a files-level net, recovered via the store's git history.
/// 4. **Git restore** of the working tree. On error, abort: nothing about
///    the conversation has been touched yet, so the session stays intact.
/// 5. **Truncate the conversation** from the checkpoint's boundary message.
///    By the time this runs, step 4 has already overwritten the working
///    tree — a silently-swallowed failure here would be precisely the
///    half-restored, files-and-chat-disagree state PM2 warns about. This
///    step must stay `?` + `.context(...)`, never `warn!` + continue.
pub async fn restore_checkpoint(
    store: &CheckpointStore,
    sm: &SessionManager,
    session_id: &str,
    target: &CheckpointId,
) -> Result<RestoreOutcome> {
    let pre_restore = match run_store_blocking(|| store.snapshot("pre-restore")) {
        Ok(snapshot) => Some(snapshot),
        Err(error) => {
            warn!(
                ?error,
                "checkpoint: pre-restore snapshot failed, continuing restore without an undo-the-undo net"
            );
            None
        }
    };

    let events = sm.events_for_session(session_id).await?;
    let (boundary_message_id, turn_seq) = checkpoint_boundary(&events, session_id, target)?;
    let Some(message_id) = boundary_message_id else {
        bail!("restore couplé impossible : frontière conversation absente pour ce checkpoint");
    };
    if !sm.message_exists(session_id, &message_id).await? {
        bail!(
            "restore couplé impossible : la frontière conversation ({message_id}) a été supprimée depuis (compaction ?) — arbre et conversation laissés intacts"
        );
    }

    if let Some((pre_restore_id, pre_restore_tree)) = pre_restore {
        journal_pre_restore(sm, session_id, &events, pre_restore_id, pre_restore_tree).await;
    }

    run_store_blocking(|| store.restore(target))
        .context("restore de l'arbre a échoué — conversation intacte")?;

    sm.truncate_conversation_from_message(session_id, &message_id)
        .await
        .context(
            "restore: troncature conversation échouée APRÈS restore de l'arbre — état incohérent, re-tenter /restore",
        )?;

    Ok(RestoreOutcome {
        restored_turn: turn_seq,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::KajiMode;
    use crate::conversation::message::Message;
    use crate::session::session_manager::SessionType;
    use crate::subprocess::git_command;
    use std::fs;
    use std::path::{Path, PathBuf};
    use tempfile::TempDir;

    fn store_for(project: &Path) -> CheckpointStore {
        CheckpointStore::for_project(project).expect("store")
    }

    async fn new_session(sm: &SessionManager) -> String {
        sm.create_session(
            PathBuf::from("/tmp"),
            "s".to_string(),
            SessionType::User,
            KajiMode::default(),
        )
        .await
        .unwrap()
        .id
    }

    /// Reads back every commit message of the store's own bare repo (not the
    /// project's git, if any). Locates the store the same way
    /// `a_failed_snapshot_does_not_abort_the_turn` in agent.rs does: exactly
    /// one directory under `kaji/checkpoints` exists per test because each
    /// test runs with its own `KAJI_PATH_ROOT`.
    fn store_commit_messages() -> Vec<String> {
        let checkpoints_dir = crate::config::paths::Paths::in_data_dir("kaji/checkpoints");
        let store_git_dir = std::fs::read_dir(&checkpoints_dir)
            .expect("checkpoints dir must exist after for_project")
            .next()
            .expect("exactly one store dir")
            .unwrap()
            .path();
        let output = git_command()
            .arg(format!("--git-dir={}", store_git_dir.display()))
            .args(["log", "--all", "--format=%s"])
            .output()
            .expect("git log");
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::to_string)
            .collect()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn restore_takes_a_pre_restore_snapshot_then_restores_tree_then_truncates() {
        let data_root = TempDir::new().unwrap();
        let project_root = TempDir::new().unwrap();
        let session_root = TempDir::new().unwrap();
        let _guard = env_lock::lock_env([(
            "KAJI_PATH_ROOT",
            Some(data_root.path().to_str().expect("utf8 temp path")),
        )]);
        let project = project_root.path();
        let store = store_for(project);
        let sm = SessionManager::new(session_root.path().to_path_buf());
        let sid = new_session(&sm).await;

        // An earlier, already-completed turn 0 — must survive the restore.
        sm.add_message(&sid, &Message::user().with_text("turn 0").with_id("m0"))
            .await
            .unwrap();

        // Turn 1 opens: its own user message is persisted before the
        // checkpoint snapshot runs, matching `Agent::snapshot_checkpoint`'s
        // real ordering (`boundary_message_id` already includes it).
        sm.add_message(&sid, &Message::user().with_text("turn 1").with_id("m1"))
            .await
            .unwrap();
        fs::write(project.join("a.txt"), "v1").unwrap();
        let (checkpoint_id, tree1) = store.snapshot("turn-1").unwrap();
        let payload = serde_json::json!({
            "checkpoint_id": checkpoint_id.0,
            "tree_sha": tree1,
            "captured": "pre_turn",
            "boundary_message_id": "m1",
        })
        .to_string();
        sm.append_event(&sid, 1, "checkpoint", &payload)
            .await
            .unwrap();

        // Turn 1 runs: files change, an assistant reply is persisted.
        fs::write(project.join("a.txt"), "v2").unwrap();
        fs::write(project.join("b.txt"), "new").unwrap();
        sm.add_message(&sid, &Message::assistant().with_text("done").with_id("m2"))
            .await
            .unwrap();

        let outcome = restore_checkpoint(&store, &sm, &sid, &checkpoint_id)
            .await
            .expect("restore should succeed");

        assert_eq!(outcome.restored_turn, 1);
        assert_eq!(
            fs::read_to_string(project.join("a.txt")).unwrap(),
            "v1",
            "arbre restauré au tree du checkpoint ciblé"
        );
        assert!(
            !project.join("b.txt").exists(),
            "fichier créé pendant le tour supprimé par le restore"
        );
        // `truncate_conversation_from_message` removes the given message_id
        // itself and everything after it (established semantics — see
        // `test_truncate_conversation_from_message_keeps_same_second_previous_rows`
        // in session_manager.rs). `boundary_message_id` is turn 1's own
        // opening prompt, so restoring checkpoint(turn 1) undoes turn 1
        // *entirely*, prompt included — only the prior turn 0 survives.
        assert_eq!(
            sm.last_message_id(&sid).await.unwrap().as_deref(),
            Some("m0"),
            "conversation tronquée à la frontière du checkpoint : le tour 1 (prompt + réponse) est annulé, le tour 0 survit"
        );
        assert!(
            store_commit_messages()
                .iter()
                .any(|message| message == "pre-restore"),
            "un checkpoint pre-restore doit avoir été pris avant toute mutation"
        );
    }

    /// BARRIER premortem PM2 — a coupled restore is all-or-nothing. If the
    /// git restore succeeds but the conversation truncation fails,
    /// `restore_checkpoint` must return `Err`, never claim success. This is
    /// the literal scenario PM2 warns about: never make step 4 non-fatal.
    #[tokio::test(flavor = "multi_thread")]
    async fn restore_errors_when_truncation_fails_after_tree_restore() {
        let data_root = TempDir::new().unwrap();
        let project_root = TempDir::new().unwrap();
        let session_root = TempDir::new().unwrap();
        let _guard = env_lock::lock_env([(
            "KAJI_PATH_ROOT",
            Some(data_root.path().to_str().expect("utf8 temp path")),
        )]);
        let project = project_root.path();
        let store = store_for(project);
        let sm = SessionManager::new(session_root.path().to_path_buf());
        let sid = new_session(&sm).await;

        sm.add_message(&sid, &Message::user().with_text("turn 1").with_id("m1"))
            .await
            .unwrap();
        fs::write(project.join("a.txt"), "v1").unwrap();
        let (checkpoint_id, tree1) = store.snapshot("turn-1").unwrap();
        let payload = serde_json::json!({
            "checkpoint_id": checkpoint_id.0,
            "tree_sha": tree1,
            "captured": "pre_turn",
            "boundary_message_id": "m1",
        })
        .to_string();
        sm.append_event(&sid, 1, "checkpoint", &payload)
            .await
            .unwrap();
        fs::write(project.join("a.txt"), "v2").unwrap();

        // The injected failure must hit the truncation's DELETE itself — a
        // dropped table would already trip the read-only existence gate and
        // refuse before any mutation, which is the *other* barrier test.
        let pool = sm.storage().pool().await.unwrap();
        sqlx::query(
            "CREATE TRIGGER block_deletes BEFORE DELETE ON messages BEGIN SELECT RAISE(ABORT, 'delete blocked by test'); END",
        )
        .execute(pool)
        .await
        .unwrap();

        let result = restore_checkpoint(&store, &sm, &sid, &checkpoint_id).await;

        assert!(
            result.is_err(),
            "truncation failure after a successful tree restore must surface as Err, never claim 'restored'"
        );
        assert_eq!(
            fs::read_to_string(project.join("a.txt")).unwrap(),
            "v1",
            "the tree restore itself did succeed before truncation failed (documents the ordering)"
        );
    }

    /// BARRIER — la frontière est relue de `messages` juste avant de
    /// trancher : si la compaction (ou la troncature d'un restore précédent)
    /// l'a supprimée, `truncate_conversation_from_message` serait un no-op
    /// silencieux (`fetch_optional` → `None` → `Ok(())`), produisant
    /// précisément l'état demi-restauré — fichiers changés, conversation
    /// intacte — que PM2 interdit. Le refus doit précéder toute mutation.
    #[tokio::test(flavor = "multi_thread")]
    async fn restore_refuses_when_boundary_message_was_deleted_by_compaction() {
        let data_root = TempDir::new().unwrap();
        let project_root = TempDir::new().unwrap();
        let session_root = TempDir::new().unwrap();
        let _guard = env_lock::lock_env([(
            "KAJI_PATH_ROOT",
            Some(data_root.path().to_str().expect("utf8 temp path")),
        )]);
        let project = project_root.path();
        let store = store_for(project);
        let sm = SessionManager::new(session_root.path().to_path_buf());
        let sid = new_session(&sm).await;

        sm.add_message(&sid, &Message::user().with_text("turn 1").with_id("m1"))
            .await
            .unwrap();
        fs::write(project.join("a.txt"), "v1").unwrap();
        let (checkpoint_id, tree1) = store.snapshot("turn-1").unwrap();
        let payload = serde_json::json!({
            "checkpoint_id": checkpoint_id.0,
            "tree_sha": tree1,
            "captured": "pre_turn",
            "boundary_message_id": "m1",
        })
        .to_string();
        sm.append_event(&sid, 1, "checkpoint", &payload)
            .await
            .unwrap();

        sm.truncate_conversation_from_message(&sid, "m1")
            .await
            .unwrap();
        assert!(
            !sm.message_exists(&sid, "m1").await.unwrap(),
            "précondition : la frontière a bien disparu de `messages`"
        );
        fs::write(project.join("a.txt"), "v2").unwrap();

        let result = restore_checkpoint(&store, &sm, &sid, &checkpoint_id).await;

        let error =
            result.expect_err("a vanished boundary message must refuse the coupled restore");
        assert!(
            error.to_string().contains("supprimée"),
            "error must name the deleted boundary: {error}"
        );
        assert_eq!(
            fs::read_to_string(project.join("a.txt")).unwrap(),
            "v2",
            "the tree must NOT have been restored — the existence check runs before any mutation"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn restore_journals_the_pre_restore_snapshot_as_a_checkpoint_event() {
        let data_root = TempDir::new().unwrap();
        let project_root = TempDir::new().unwrap();
        let session_root = TempDir::new().unwrap();
        let _guard = env_lock::lock_env([(
            "KAJI_PATH_ROOT",
            Some(data_root.path().to_str().expect("utf8 temp path")),
        )]);
        let project = project_root.path();
        let store = store_for(project);
        let sm = SessionManager::new(session_root.path().to_path_buf());
        let sid = new_session(&sm).await;

        sm.add_message(&sid, &Message::user().with_text("turn 1").with_id("m1"))
            .await
            .unwrap();
        fs::write(project.join("a.txt"), "v1").unwrap();
        let (checkpoint_id, tree1) = store.snapshot("turn-1").unwrap();
        let payload = serde_json::json!({
            "checkpoint_id": checkpoint_id.0,
            "tree_sha": tree1,
            "captured": "pre_turn",
            "boundary_message_id": "m1",
        })
        .to_string();
        sm.append_event(&sid, 1, "checkpoint", &payload)
            .await
            .unwrap();
        fs::write(project.join("a.txt"), "v2").unwrap();
        sm.add_message(&sid, &Message::assistant().with_text("done").with_id("m2"))
            .await
            .unwrap();

        restore_checkpoint(&store, &sm, &sid, &checkpoint_id)
            .await
            .expect("restore should succeed");

        let events = sm.events_for_session(&sid).await.unwrap();
        let pre_restore = events
            .iter()
            .filter(|event| event.kind == "checkpoint")
            .filter_map(|event| {
                serde_json::from_str::<serde_json::Value>(&event.payload_json)
                    .ok()
                    .map(|payload| (event.turn_seq, payload))
            })
            .find(|(_, payload)| {
                payload.get("captured").and_then(|v| v.as_str()) == Some("pre_restore")
            })
            .expect("the pre-restore snapshot must be journaled as a checkpoint event");
        let (event_turn_seq, payload) = pre_restore;
        assert_eq!(
            event_turn_seq, 1,
            "l'event pre-restore porte le turn_seq courant (le dernier de la session)"
        );
        let pre_restore_id = payload
            .get("checkpoint_id")
            .and_then(|v| v.as_str())
            .expect("pre-restore event carries its own restorable id");
        assert_ne!(
            pre_restore_id, checkpoint_id.0,
            "l'event pre-restore référence SON snapshot, pas la cible"
        );
        assert_eq!(
            payload.get("boundary_message_id").and_then(|v| v.as_str()),
            Some("m2"),
            "la frontière du pre-restore est le dernier message AVANT troncature"
        );
    }

    /// BARRIER — `block_in_place` panique sur un runtime current-thread ;
    /// le restore doit fonctionner sur les deux flavors (les tests tokio par
    /// défaut et tout futur appelant one-shot sont current-thread).
    #[tokio::test]
    async fn restore_succeeds_on_a_current_thread_runtime() {
        let data_root = TempDir::new().unwrap();
        let project_root = TempDir::new().unwrap();
        let session_root = TempDir::new().unwrap();
        let _guard = env_lock::lock_env([(
            "KAJI_PATH_ROOT",
            Some(data_root.path().to_str().expect("utf8 temp path")),
        )]);
        let project = project_root.path();
        let store = store_for(project);
        let sm = SessionManager::new(session_root.path().to_path_buf());
        let sid = new_session(&sm).await;

        sm.add_message(&sid, &Message::user().with_text("turn 1").with_id("m1"))
            .await
            .unwrap();
        fs::write(project.join("a.txt"), "v1").unwrap();
        let (checkpoint_id, tree1) = store.snapshot("turn-1").unwrap();
        let payload = serde_json::json!({
            "checkpoint_id": checkpoint_id.0,
            "tree_sha": tree1,
            "captured": "pre_turn",
            "boundary_message_id": "m1",
        })
        .to_string();
        sm.append_event(&sid, 1, "checkpoint", &payload)
            .await
            .unwrap();
        fs::write(project.join("a.txt"), "v2").unwrap();

        restore_checkpoint(&store, &sm, &sid, &checkpoint_id)
            .await
            .expect("restore must not panic nor fail on a current-thread runtime");

        assert_eq!(fs::read_to_string(project.join("a.txt")).unwrap(), "v1");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn restore_refuses_coupling_when_boundary_message_id_is_null() {
        let data_root = TempDir::new().unwrap();
        let project_root = TempDir::new().unwrap();
        let session_root = TempDir::new().unwrap();
        let _guard = env_lock::lock_env([(
            "KAJI_PATH_ROOT",
            Some(data_root.path().to_str().expect("utf8 temp path")),
        )]);
        let project = project_root.path();
        let store = store_for(project);
        let sm = SessionManager::new(session_root.path().to_path_buf());
        let sid = new_session(&sm).await;

        fs::write(project.join("a.txt"), "v1").unwrap();
        let (checkpoint_id, tree1) = store.snapshot("turn-1").unwrap();
        let payload = serde_json::json!({
            "checkpoint_id": checkpoint_id.0,
            "tree_sha": tree1,
            "captured": "pre_turn",
            "boundary_message_id": null,
        })
        .to_string();
        sm.append_event(&sid, 1, "checkpoint", &payload)
            .await
            .unwrap();
        fs::write(project.join("a.txt"), "v2").unwrap();

        let result = restore_checkpoint(&store, &sm, &sid, &checkpoint_id).await;

        let error = result.expect_err("a null boundary_message_id must refuse the coupled restore");
        assert!(
            error.to_string().contains("frontière conversation absente"),
            "error must explain why: {error}"
        );
        assert_eq!(
            fs::read_to_string(project.join("a.txt")).unwrap(),
            "v2",
            "the tree must NOT have been restored — the boundary check runs before any mutation"
        );
        assert!(
            store_commit_messages()
                .iter()
                .any(|message| message == "pre-restore"),
            "the pre-restore snapshot still runs first, even though the restore is then refused"
        );
    }
}
