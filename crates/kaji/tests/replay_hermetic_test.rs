use anyhow::Result;
use async_trait::async_trait;
use futures::StreamExt;
use kaji::agents::{Agent, AgentConfig, ExtensionConfig, KajiPlatform, SessionConfig};
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
use rmcp::model::{CallToolRequestParams, Tool, ToolAnnotations};
use serde_json::Map;
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

    agent.set_replay_mode(ReplayMode::new(session.id.clone(), KajiMode::Auto));
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

/// Un outil annoté en écriture : c'est lui que SmartApprove rétrograde en
/// `ask_before`, en réécrivant `permission.yaml`.
const WRITE_TOOL: &str = "hermetic-frontend__write_thing";

fn write_annotated_extension() -> ExtensionConfig {
    let tool = Tool::new(WRITE_TOOL.to_string(), "writes something", Map::new())
        .annotate(ToolAnnotations::new().read_only(false));
    ExtensionConfig::Frontend {
        name: "hermetic-frontend".to_string(),
        description: "frontend fixture".to_string(),
        tools: vec![tool],
        instructions: None,
        bundled: None,
        available_tools: Vec::new(),
    }
}

/// Le mode de la session enregistrée, pas celui de la machine de rejeu : le
/// prompt système en dépend (`is_autonomous`, branche `Chat`) donc la requête
/// hachée aussi. Et l'assemblage d'un tour rejoué doit être **pur** — le
/// SmartApprove d'aujourd'hui ne doit pas réécrire le `permission.yaml` de
/// l'utilisateur pendant un rejeu.
#[tokio::test]
async fn replay_restores_the_recorded_mode_and_assembles_without_writing_permissions() -> Result<()>
{
    let memory_dir = tempfile::tempdir()?;
    let _guard = env_lock::lock_env([
        ("KAJI_STATE_MACHINE", None),
        (
            "KAJI_MEMORY_DIR",
            Some(memory_dir.path().to_str().expect("utf8 temp path")),
        ),
    ]);

    let temp_dir = tempfile::tempdir()?;
    let working_dir = temp_dir.path().join("workspace");
    std::fs::create_dir_all(&working_dir)?;
    let config_dir = temp_dir.path().join("config");
    let permissions = Arc::new(PermissionManager::new(config_dir.clone()));
    let session_manager = Arc::new(SessionManager::new(temp_dir.path().join("data")));

    let build = |mode: KajiMode| {
        Agent::with_config(AgentConfig::new(
            Arc::clone(&session_manager),
            Arc::clone(&permissions),
            None,
            mode,
            true,
            KajiPlatform::KajiCli,
        ))
    };

    let session = session_manager
        .create_session(
            working_dir.clone(),
            "replay-mode-test".to_string(),
            SessionType::Hidden,
            KajiMode::SmartApprove,
        )
        .await?;

    // Contrôle : hors rejeu, SmartApprove rétrograde bien l'outil et persiste —
    // sans quoi l'assertion de pureté ci-dessous ne prouverait rien.
    let live = build(KajiMode::SmartApprove);
    live.update_provider(
        Arc::new(TwoCallProvider {
            calls: AtomicUsize::new(0),
        }),
        ModelConfig::new("mock-model"),
        &session.id,
    )
    .await?;
    live.add_extension(write_annotated_extension(), &session.id)
        .await?;
    live.prepare_tools_and_prompt(&session.id, &working_dir)
        .await?;
    assert!(
        permissions
            .get_smart_approve_permission(WRITE_TOOL)
            .is_some(),
        "le contrôle vivant réécrit bien les permissions"
    );
    assert!(config_dir.join("permission.yaml").exists());

    std::fs::remove_file(config_dir.join("permission.yaml"))?;
    let fresh = Arc::new(PermissionManager::new(config_dir.clone()));
    let mut replayed = Agent::with_config(AgentConfig::new(
        Arc::clone(&session_manager),
        Arc::clone(&fresh),
        None,
        KajiMode::Auto,
        true,
        KajiPlatform::KajiCli,
    ));
    replayed
        .update_provider(
            Arc::new(TwoCallProvider {
                calls: AtomicUsize::new(0),
            }),
            ModelConfig::new("mock-model"),
            &session.id,
        )
        .await?;
    replayed
        .add_extension(write_annotated_extension(), &session.id)
        .await?;
    replayed.set_replay_mode(ReplayMode::new(session.id.clone(), KajiMode::SmartApprove));

    assert_eq!(
        replayed.kaji_mode().await,
        KajiMode::SmartApprove,
        "le rejeu prend le mode de la session enregistrée, pas celui de la machine"
    );

    replayed
        .prepare_tools_and_prompt(&session.id, &working_dir)
        .await?;
    assert!(
        fresh.get_smart_approve_permission(WRITE_TOOL).is_none(),
        "un assemblage rejoué est pur : il ne rétrograde aucun outil"
    );
    assert!(
        !config_dir.join("permission.yaml").exists(),
        "un rejeu ne réécrit pas le permission.yaml de l'utilisateur"
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
