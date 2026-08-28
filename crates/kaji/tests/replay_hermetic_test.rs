use anyhow::Result;
use async_trait::async_trait;
use futures::StreamExt;
use kaji::agents::{Agent, AgentConfig, KajiPlatform, SessionConfig};
use kaji::config::permission::PermissionManager;
use kaji::config::KajiMode;
use kaji::conversation::message::Message;
use kaji::kaji::SessionMemory;
use kaji::providers::base::{stream_from_single_message, MessageStream, Provider};
use kaji::replay::mode::ReplayMode;
use kaji::session::session_manager::SessionType;
use kaji::session::SessionManager;
use kaji_providers::conversation::token_usage::{ProviderUsage, Usage};
use kaji_providers::errors::ProviderError;
use kaji_providers::model::ModelConfig;
use rmcp::model::{CallToolRequestParams, Tool};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

const TOOL_REQUEST_ID: &str = "hermetic-probe";
const RECORDED_QUERY: &str = "record this turn into the log";
const REPLAYED_QUERY: &str = "replay this turn and write nothing";

/// Two provider calls per turn — the first answers with a tool request, so both
/// loops run inference twice — each reporting non-zero tokens so a
/// `usage_ledger` row is visible in the footprint when one is written.
struct TwoCallProvider {
    calls: AtomicUsize,
}

#[async_trait]
impl Provider for TwoCallProvider {
    async fn stream(
        &self,
        _model_config: &ModelConfig,
        _system_prompt: &str,
        _messages: &[Message],
        _tools: &[Tool],
    ) -> Result<MessageStream, ProviderError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let usage = ProviderUsage::new(
            "mock-model".to_string(),
            Usage::new(Some(10), Some(5), Some(15)),
        );
        let message = if call.is_multiple_of(2) {
            Message::assistant().with_tool_request(
                TOOL_REQUEST_ID,
                Ok(CallToolRequestParams::new("missing__probe")),
            )
        } else {
            Message::assistant().with_text("done")
        };
        Ok(stream_from_single_message(message, usage))
    }

    fn get_name(&self) -> &str {
        "two-call-mock"
    }
}

/// Everything a replayed turn must leave untouched: the source log, the
/// checkpoints it carries, the usage ledger and the memory journal.
#[derive(Debug, PartialEq)]
struct Footprint {
    events: Vec<String>,
    checkpoints: usize,
    ledger_tokens: i64,
    memory: Vec<(u64, String)>,
}

async fn footprint(session_manager: &Arc<SessionManager>, session_id: &str) -> Result<Footprint> {
    let events = session_manager.session_events(session_id).await?;
    let checkpoints = events.iter().filter(|e| e.kind == "checkpoint").count();
    Ok(Footprint {
        events: events
            .iter()
            .map(|event| format!("{}|{}|{}", event.turn_seq, event.kind, event.payload_json))
            .collect(),
        checkpoints,
        ledger_tokens: session_manager.usage_since(0).await?.total_tokens,
        memory: SessionMemory::load(session_id)
            .list()
            .into_iter()
            .map(|entry| (entry.id, entry.body))
            .collect(),
    })
}

/// A checkpoint store is only wired for a git work tree, so the turn's
/// `checkpoint` event exists to be counted at all.
fn git_init(dir: &Path) {
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["init", "--quiet"])
        .status()
        .expect("spawning git init");
    assert!(status.success(), "git init failed in {}", dir.display());
}

fn session_config(id: &str) -> SessionConfig {
    SessionConfig {
        id: id.to_string(),
        schedule_id: None,
        max_turns: Some(3),
        retry_config: None,
    }
}

async fn drain(
    stream: impl futures::Stream<Item = Result<kaji::agents::AgentEvent>>,
) -> Result<()> {
    tokio::pin!(stream);
    while let Some(event) = stream.next().await {
        event?;
    }
    Ok(())
}

/// Records one real turn, then replays a second one with the agent in
/// `ReplayMode`: the footprint taken between the two must survive the replay
/// byte for byte. The recorded turn doubles as the control — it proves each of
/// the four observables actually moves when the mode is off, so the equality
/// below is not vacuous.
async fn assert_hermetic(state_machine: Option<&str>) -> Result<()> {
    let label = format!("KAJI_STATE_MACHINE={state_machine:?}");
    let memory_dir = tempfile::tempdir()?;
    let data_root = tempfile::tempdir()?;
    let _guard = env_lock::lock_env([
        ("KAJI_STATE_MACHINE", state_machine),
        (
            "KAJI_MEMORY_DIR",
            Some(memory_dir.path().to_str().expect("utf8 temp path")),
        ),
        (
            "KAJI_PATH_ROOT",
            Some(data_root.path().to_str().expect("utf8 temp path")),
        ),
    ]);

    let temp_dir = tempfile::tempdir()?;
    let working_dir = temp_dir.path().join("workspace");
    std::fs::create_dir_all(&working_dir)?;
    git_init(&working_dir);

    let session_manager = Arc::new(SessionManager::new(temp_dir.path().join("data")));
    let mut agent = Agent::with_config(AgentConfig::new(
        Arc::clone(&session_manager),
        Arc::new(PermissionManager::new(temp_dir.path().join("config"))),
        None,
        KajiMode::Auto,
        true,
        KajiPlatform::KajiCli,
    ));
    let session = session_manager
        .create_session(
            working_dir.clone(),
            "replay-hermetic-test".to_string(),
            SessionType::Hidden,
            KajiMode::Auto,
        )
        .await?;
    agent
        .update_provider(
            Arc::new(TwoCallProvider {
                calls: AtomicUsize::new(0),
            }),
            ModelConfig::new("mock-model"),
            &session.id,
        )
        .await?;
    agent.wire_checkpoint_store(&session.id).await;
    assert!(
        agent.checkpoint_store().is_some(),
        "{label}: the control turn needs a checkpoint store to snapshot: {:?}",
        agent.checkpoint_disabled_reason()
    );

    drain(
        agent
            .reply(
                Message::user().with_text(RECORDED_QUERY),
                session_config(&session.id),
                None,
            )
            .await?,
    )
    .await?;

    let before = footprint(&session_manager, &session.id).await?;
    assert!(
        !before.events.is_empty(),
        "{label}: the recorded turn journaled events"
    );
    assert_eq!(
        before.checkpoints, 1,
        "{label}: the recorded turn snapshotted a checkpoint: {:?}",
        before.events
    );
    assert!(
        before.ledger_tokens > 0,
        "{label}: the recorded turn billed the usage ledger"
    );
    assert!(
        before
            .memory
            .iter()
            .any(|(_, body)| body.contains(RECORDED_QUERY)),
        "{label}: the recorded turn ingested its instruction: {:?}",
        before.memory
    );

    agent.set_replay_mode(ReplayMode::new(session.id.clone()));
    drain(
        agent
            .reply(
                Message::user().with_text(REPLAYED_QUERY),
                session_config(&session.id),
                None,
            )
            .await?,
    )
    .await?;

    let after = footprint(&session_manager, &session.id).await?;
    assert_eq!(
        before, after,
        "{label}: a replayed turn writes nothing — log, checkpoints, ledger and memory journal are untouched"
    );

    Ok(())
}

#[tokio::test]
async fn a_replayed_turn_writes_nothing_on_the_legacy_loop() -> Result<()> {
    assert_hermetic(None).await
}

#[tokio::test]
async fn a_replayed_turn_writes_nothing_on_the_state_machine_loop() -> Result<()> {
    assert_hermetic(Some("1")).await
}
