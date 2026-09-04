//! Le rejeu adressé par clé : `EventCursor` indexe le journal v2 d'une session
//! enregistrée par le vrai pipeline (Tasks 5-7), `ReplayProvider` resert les
//! chunks de chaque appel LLM après vérification du `request_hash`.

use anyhow::Result;
use async_trait::async_trait;
use futures::StreamExt;
use kaji::agents::{Agent, AgentConfig, ExtensionConfig, KajiPlatform, SessionConfig};
use kaji::config::permission::PermissionManager;
use kaji::config::KajiMode;
use kaji::conversation::message::Message;
use kaji::kaji::SessionMemory;
use kaji::providers::base::{stream_from_single_message, MessageStream, Provider};
use kaji::replay::cursor::{EventCursor, ReplayUnavailable};
use kaji::replay::provider::ReplayProvider;
use kaji::replay::record::RecordSink;
use kaji::session::session_manager::{SessionEvent, SessionType, DB_NAME, SESSIONS_FOLDER};
use kaji::session::SessionManager;
use kaji_providers::conversation::token_usage::{ProviderUsage, Usage};
use kaji_providers::errors::ProviderError;
use kaji_providers::model::ModelConfig;
use kaji_test_support::{McpFixture, FAKE_CODE};
use rmcp::model::{CallToolRequestParams, Tool};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

const TOOL_REQUEST_ID: &str = "probe-1";
const TOOL: &str = "mcp-fixture__get_code";
const MEMORY_FACT: &str = "Onboarding lives on the PO dashboard";
const MEMORY_QUERY: &str = "po dashboard onboarding";

/// Ce que le provider a réellement reçu à l'enregistrement : les arguments
/// exacts sur lesquels le `request_hash` journalisé a été calculé, donc ceux
/// que le rejeu doit resoumettre pour retrouver son échange.
#[derive(Clone)]
struct RecordedCall {
    system: String,
    messages: Vec<Message>,
    tools: Vec<Tool>,
}

/// Deux appels au premier tour (le premier demande l'outil), un seul aux tours
/// suivants — et mémorise ses arguments pour le rejeu.
struct FixtureProvider {
    calls: Arc<Mutex<Vec<RecordedCall>>>,
}

impl FixtureProvider {
    fn new() -> Self {
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

#[async_trait]
impl Provider for FixtureProvider {
    async fn stream(
        &self,
        _model_config: &ModelConfig,
        system: &str,
        messages: &[Message],
        tools: &[Tool],
    ) -> Result<MessageStream, ProviderError> {
        let first_call = {
            let mut calls = self.calls.lock().unwrap();
            calls.push(RecordedCall {
                system: system.to_string(),
                messages: messages.to_vec(),
                tools: tools.to_vec(),
            });
            calls.len() == 1
        };
        let usage = ProviderUsage::new(
            "mock-model".to_string(),
            Usage::new(Some(11), Some(22), Some(33)),
        );
        let message = if first_call {
            Message::assistant()
                .with_tool_request(TOOL_REQUEST_ID, Ok(CallToolRequestParams::new(TOOL)))
        } else {
            Message::assistant().with_text("done")
        };
        Ok(stream_from_single_message(message, usage))
    }

    fn get_name(&self) -> &str {
        "fixture-mock"
    }
}

/// La session enregistrée et tout ce qu'il faut pour l'interroger après coup.
struct Fixture {
    data_dir: TempDir,
    session_manager: Arc<SessionManager>,
    session_id: String,
    calls: Vec<RecordedCall>,
    events: Vec<SessionEvent>,
}

impl Fixture {
    /// Connexion directe au fichier SQLite de la fixture : le test ampute le
    /// journal par un `DELETE` que l'API publique n'expose pas.
    async fn raw_pool(&self) -> Result<sqlx::SqlitePool> {
        let db_path = self
            .data_dir
            .path()
            .join("data")
            .join(SESSIONS_FOLDER)
            .join(DB_NAME);
        Ok(sqlx::SqlitePool::connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .filename(db_path)
                .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal),
        )
        .await?)
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

fn session_config(id: &str) -> SessionConfig {
    SessionConfig {
        id: id.to_string(),
        schedule_id: None,
        max_turns: Some(3),
        retry_config: None,
    }
}

fn payloads(events: &[SessionEvent], kind: &str) -> Vec<Value> {
    events
        .iter()
        .filter(|event| event.kind == kind)
        .map(|event| serde_json::from_str(&event.payload_json).expect("payload is json"))
        .collect()
}

/// Deux tours enregistrés par le vrai pipeline : le premier appelle l'outil MCP
/// (deux appels LLM), le second répond en texte. La mémoire est amorcée pour que
/// le tour splice un vrai bloc, donc que `memory_block` soit journalisé.
async fn record_fixture(state_machine: Option<&str>) -> Result<Fixture> {
    let memory_dir = tempfile::tempdir()?;
    let _guard = env_lock::lock_env([
        ("KAJI_STATE_MACHINE", state_machine),
        (
            "KAJI_MEMORY_DIR",
            Some(memory_dir.path().to_str().expect("utf8 temp path")),
        ),
    ]);

    let mut seed = SessionMemory::load("seeding-session");
    seed.remember(MEMORY_FACT, &["dashboard"], None);
    drop(seed);

    let mcp = McpFixture::new().await;
    let data_dir = tempfile::tempdir()?;
    let working_dir = data_dir.path().join("workspace");
    std::fs::create_dir_all(&working_dir)?;

    let session_manager = Arc::new(SessionManager::new(data_dir.path().join("data")));
    let agent = Agent::with_config(AgentConfig::new(
        Arc::clone(&session_manager),
        Arc::new(PermissionManager::new(data_dir.path().join("config"))),
        None,
        KajiMode::Auto,
        true,
        KajiPlatform::KajiCli,
    ));
    let session = session_manager
        .create_session(
            working_dir,
            "replay-provider-test".to_string(),
            SessionType::Hidden,
            KajiMode::Auto,
        )
        .await?;

    let provider = Arc::new(FixtureProvider::new());
    let calls = Arc::clone(&provider.calls);
    agent
        .update_provider(provider, ModelConfig::new("mock-model"), &session.id)
        .await?;
    agent
        .add_extension(
            ExtensionConfig::streamable_http("mcp-fixture", &mcp.url, "MCP fixture", 30_u64),
            &session.id,
        )
        .await?;

    for query in [MEMORY_QUERY, "et ensuite ?"] {
        drain(
            agent
                .reply(
                    Message::user().with_text(query),
                    session_config(&session.id),
                    None,
                )
                .await?,
        )
        .await?;
    }

    let events = session_manager.session_events(&session.id).await?;
    let calls = calls.lock().unwrap().clone();
    Ok(Fixture {
        data_dir,
        session_manager,
        session_id: session.id,
        calls,
        events,
    })
}

/// `(turn_seq, call_idx)` de chaque appel LLM, dans l'ordre du journal — le même
/// que celui des appels du provider à l'enregistrement.
fn logged_positions(events: &[SessionEvent]) -> Vec<(i64, u32)> {
    payloads(events, "llm_request")
        .iter()
        .map(|request| {
            (
                request["turn_seq"]
                    .as_i64()
                    .expect("turn_seq is an integer"),
                request["call_idx"]
                    .as_u64()
                    .expect("call_idx is an integer") as u32,
            )
        })
        .collect()
}

async fn collect_chunks(stream: MessageStream) -> Result<Vec<Value>> {
    let chunks: Vec<_> = stream.collect::<Vec<_>>().await;
    chunks
        .into_iter()
        .map(|chunk| {
            let (message, usage) = chunk.map_err(anyhow::Error::from)?;
            Ok(serde_json::json!([message, usage]))
        })
        .collect()
}

async fn assert_cursor_indexes_every_kind(state_machine: Option<&str>) -> Result<()> {
    let label = format!("KAJI_STATE_MACHINE={state_machine:?}");
    let fixture = record_fixture(state_machine).await?;
    let cursor = EventCursor::load(&fixture.session_manager, &fixture.session_id).await?;

    let positions = logged_positions(&fixture.events);
    assert_eq!(
        positions.len(),
        3,
        "{label}: two calls on the tool turn, one on the next: {positions:?}"
    );
    assert_eq!(
        cursor.llm_responses.len(),
        positions.len(),
        "{label}: one indexed exchange per logged call"
    );
    for position in &positions {
        let exchange = cursor
            .llm_responses
            .get(position)
            .unwrap_or_else(|| panic!("{label}: exchange indexed at {position:?}"));
        assert_eq!(
            exchange.request_hash.len(),
            64,
            "{label}: the request hash of {position:?} comes from llm_request"
        );
        assert_eq!(
            exchange.finish, "stop",
            "{label}: a drained stream finished on stop at {position:?}"
        );
        assert!(
            !exchange.chunks.is_empty(),
            "{label}: the chunks of {position:?} are indexed with their request"
        );
    }

    let tool_result = cursor
        .tool_results
        .get(TOOL_REQUEST_ID)
        .unwrap_or_else(|| panic!("{label}: tool result indexed by tool_call_id"));
    assert!(
        tool_result.contains(FAKE_CODE),
        "{label}: the tool's own payload is served verbatim: {tool_result}"
    );

    let turns: Vec<i64> = positions.iter().map(|(turn, _)| *turn).collect();
    let block = cursor
        .memory_blocks
        .get(&(turns[0], 0))
        .unwrap_or_else(|| panic!("{label}: memory block indexed by (turn, appel): {turns:?}"));
    assert!(
        block.contains(MEMORY_FACT),
        "{label}: the recorded block carries the seeded fact: {block}"
    );

    let reads = cursor
        .clock_reads
        .get(&turns[0])
        .unwrap_or_else(|| panic!("{label}: clock reads indexed by turn: {turns:?}"));
    assert_eq!(
        reads.len(),
        1,
        "{label}: the turn reads the prompt clock once: {reads:?}"
    );

    assert_eq!(
        cursor.log_meta.idgen_seed, fixture.session_id,
        "{label}: log_meta carries the session's IdGen seed"
    );

    Ok(())
}

async fn assert_provider_serves_recorded_chunks(state_machine: Option<&str>) -> Result<()> {
    let label = format!("KAJI_STATE_MACHINE={state_machine:?}");
    let fixture = record_fixture(state_machine).await?;
    let cursor = EventCursor::load(&fixture.session_manager, &fixture.session_id).await?;
    let recorded = payloads(&fixture.events, "llm_response");
    let positions = logged_positions(&fixture.events);
    assert_eq!(
        fixture.calls.len(),
        positions.len(),
        "{label}: every provider call was logged"
    );

    let provider = ReplayProvider::new(Arc::new(cursor), false);
    let position = provider.position();
    let model_config = ModelConfig::new("mock-model");

    for (index, call) in fixture.calls.iter().enumerate() {
        let (turn_seq, call_idx) = positions[index];
        if call_idx == 0 {
            position.begin_turn(turn_seq);
        }
        let stream = provider
            .stream(&model_config, &call.system, &call.messages, &call.tools)
            .await
            .unwrap_or_else(|error| panic!("{label}: strict replay serves call {index}: {error}"));
        assert_eq!(
            collect_chunks(stream).await?,
            recorded[index]["chunks"]
                .as_array()
                .expect("chunks is an array")
                .clone(),
            "{label}: call {index} replays its recorded chunks verbatim"
        );
    }

    Ok(())
}

async fn assert_altered_request_is_refused(state_machine: Option<&str>) -> Result<()> {
    let label = format!("KAJI_STATE_MACHINE={state_machine:?}");
    let fixture = record_fixture(state_machine).await?;
    let cursor = Arc::new(EventCursor::load(&fixture.session_manager, &fixture.session_id).await?);
    let (turn_seq, _) = logged_positions(&fixture.events)[0];
    let call = &fixture.calls[0];
    let altered = format!("{}\n\naltered", call.system);
    let model_config = ModelConfig::new("mock-model");

    let strict = ReplayProvider::new(Arc::clone(&cursor), false);
    strict.position().begin_turn(turn_seq);
    let error = strict
        .stream(&model_config, &altered, &call.messages, &call.tools)
        .await
        .err()
        .unwrap_or_else(|| panic!("{label}: strict replay refuses an altered request"));
    let rendered = error.to_string();
    let expected_hash = &cursor.llm_responses[&(turn_seq, 0)].request_hash;
    assert!(
        rendered.contains(expected_hash),
        "{label}: the error carries the recorded hash: {rendered}"
    );
    assert!(
        rendered.contains(&format!("{turn_seq}")),
        "{label}: the error names the turn: {rendered}"
    );

    let lenient = ReplayProvider::new(Arc::clone(&cursor), true);
    lenient.position().begin_turn(turn_seq);
    let stream = lenient
        .stream(&model_config, &altered, &call.messages, &call.tools)
        .await
        .unwrap_or_else(|error| panic!("{label}: lenient replay serves anyway: {error}"));
    assert!(
        !collect_chunks(stream).await?.is_empty(),
        "{label}: lenient replay still serves the recorded chunks"
    );

    Ok(())
}

async fn assert_truncated_log_is_unavailable(state_machine: Option<&str>) -> Result<()> {
    let label = format!("KAJI_STATE_MACHINE={state_machine:?}");
    let fixture = record_fixture(state_machine).await?;
    let last_turn = logged_positions(&fixture.events)
        .last()
        .expect("the fixture logged calls")
        .0;

    let pool = fixture.raw_pool().await?;
    sqlx::query(
        "DELETE FROM session_events WHERE session_id = ? AND kind = 'turn_end' AND turn_seq = ?",
    )
    .bind(&fixture.session_id)
    .bind(last_turn)
    .execute(&pool)
    .await?;
    pool.close().await;

    let error = EventCursor::load(&fixture.session_manager, &fixture.session_id)
        .await
        .err()
        .unwrap_or_else(|| panic!("{label}: a truncated log has no cursor"));
    assert!(
        matches!(
            error.downcast_ref::<ReplayUnavailable>(),
            Some(ReplayUnavailable::TruncatedAt(turn)) if *turn == last_turn
        ),
        "{label}: the last turn without turn_end is reported: {error}"
    );

    Ok(())
}

#[tokio::test]
async fn cursor_indexes_every_kind_on_the_legacy_loop() -> Result<()> {
    assert_cursor_indexes_every_kind(None).await
}

#[tokio::test]
async fn cursor_indexes_every_kind_on_the_state_machine_loop() -> Result<()> {
    assert_cursor_indexes_every_kind(Some("1")).await
}

#[tokio::test]
async fn provider_serves_recorded_chunks_on_the_legacy_loop() -> Result<()> {
    assert_provider_serves_recorded_chunks(None).await
}

#[tokio::test]
async fn provider_serves_recorded_chunks_on_the_state_machine_loop() -> Result<()> {
    assert_provider_serves_recorded_chunks(Some("1")).await
}

#[tokio::test]
async fn altered_request_is_refused_on_the_legacy_loop() -> Result<()> {
    assert_altered_request_is_refused(None).await
}

#[tokio::test]
async fn altered_request_is_refused_on_the_state_machine_loop() -> Result<()> {
    assert_altered_request_is_refused(Some("1")).await
}

#[tokio::test]
async fn truncated_log_is_unavailable_on_the_legacy_loop() -> Result<()> {
    assert_truncated_log_is_unavailable(None).await
}

#[tokio::test]
async fn truncated_log_is_unavailable_on_the_state_machine_loop() -> Result<()> {
    assert_truncated_log_is_unavailable(Some("1")).await
}

/// Une session sans `log_meta` est antérieure au replay v2 : rien à indexer.
#[tokio::test]
async fn a_session_without_log_meta_is_pre_v2() -> Result<()> {
    let data_dir = tempfile::tempdir()?;
    let session_manager = Arc::new(SessionManager::new(data_dir.path().join("data")));
    let session = session_manager
        .create_session(
            PathBuf::from("."),
            "pre-v2".to_string(),
            SessionType::Hidden,
            KajiMode::Auto,
        )
        .await?;

    let error = EventCursor::load(&session_manager, &session.id)
        .await
        .err()
        .expect("a log without log_meta has no cursor");
    assert!(
        matches!(
            error.downcast_ref::<ReplayUnavailable>(),
            Some(ReplayUnavailable::PreV2)
        ),
        "the absence of log_meta is reported as pre-v2: {error}"
    );
    Ok(())
}

/// Une session dont les payloads ont été purgés porte `replayable = false` : le
/// journal est là mais incomplet, il ne doit jamais être servi comme intact.
#[tokio::test]
async fn a_purged_session_is_refused() -> Result<()> {
    let data_dir = tempfile::tempdir()?;
    let session_manager = Arc::new(SessionManager::new(data_dir.path().join("data")));
    let session = session_manager
        .create_session(
            PathBuf::from("."),
            "purged".to_string(),
            SessionType::Hidden,
            KajiMode::Auto,
        )
        .await?;
    session_manager
        .append_log_meta_if_absent(&session.id)
        .await?;
    session_manager.mark_not_replayable(&session.id).await?;

    let error = EventCursor::load(&session_manager, &session.id)
        .await
        .err()
        .expect("a purged log has no cursor");
    assert!(
        matches!(
            error.downcast_ref::<ReplayUnavailable>(),
            Some(ReplayUnavailable::Purged)
        ),
        "a session marked not replayable is reported as purged: {error}"
    );
    Ok(())
}

/// `condense_triggered` n'est émis que par un tour qui compacte : il est indexé
/// depuis le sink de capture plutôt que par une fixture de compaction complète.
#[tokio::test]
async fn condense_turns_are_indexed() -> Result<()> {
    let data_dir = tempfile::tempdir()?;
    let session_manager = Arc::new(SessionManager::new(data_dir.path().join("data")));
    let session = session_manager
        .create_session(
            PathBuf::from("."),
            "condense".to_string(),
            SessionType::Hidden,
            KajiMode::Auto,
        )
        .await?;
    session_manager
        .append_log_meta_if_absent(&session.id)
        .await?;

    let sink = RecordSink::new(Arc::clone(&session_manager), session.id.clone());
    sink.record_condense_triggered(4, "auto_compact_threshold")
        .await;

    let cursor = EventCursor::load(&session_manager, &session.id).await?;
    assert!(
        cursor.condense_turns.contains(&4),
        "the compacted turn is indexed: {:?}",
        cursor.condense_turns
    );
    assert!(
        !cursor.condense_turns.contains(&3),
        "a turn that never compacted is not indexed: {:?}",
        cursor.condense_turns
    );
    Ok(())
}
