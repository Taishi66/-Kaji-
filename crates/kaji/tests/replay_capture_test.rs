use anyhow::Result;
use async_trait::async_trait;
use futures::StreamExt;
use kaji::agents::{Agent, AgentConfig, ExtensionConfig, KajiPlatform, SessionConfig};
use kaji::config::permission::PermissionManager;
use kaji::config::KajiMode;
use kaji::conversation::message::Message;
use kaji::conversation::Conversation;
use kaji::kaji::{splice_memory_block, SessionMemory};
use kaji::providers::base::{stream_from_single_message, MessageStream, Provider};
use kaji::session::session_manager::{SessionEvent, SessionType};
use kaji::session::SessionManager;
use kaji_providers::conversation::token_usage::{ProviderUsage, Usage};
use kaji_providers::errors::ProviderError;
use kaji_providers::model::ModelConfig;
use kaji_test_support::{McpFixture, FAKE_CODE};
use rmcp::model::{CallToolRequestParams, Role, Tool};
use serde_json::Value;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

const TOOL_REQUEST_ID: &str = "probe-1";

/// Fact seeded in the cross-session store before the turn runs, and the user
/// instruction that recalls it — the turn's system prompt then carries a real
/// memory block instead of the empty one an isolated store would produce.
const MEMORY_FACT: &str = "Onboarding lives on the PO dashboard";
const MEMORY_QUERY: &str = "po dashboard onboarding";

/// Forces two provider calls inside a single turn: the first response asks for
/// `tool`, so both loops answer it and run inference again.
struct TwoCallProvider {
    calls: AtomicUsize,
    tool: &'static str,
}

impl TwoCallProvider {
    fn new(tool: &'static str) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            tool,
        }
    }
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
        let usage = ProviderUsage::new("mock-model".to_string(), Usage::default());
        let message = if call == 0 {
            Message::assistant()
                .with_tool_request(TOOL_REQUEST_ID, Ok(CallToolRequestParams::new(self.tool)))
        } else {
            Message::assistant().with_text("done")
        };
        Ok(stream_from_single_message(message, usage))
    }

    fn get_name(&self) -> &str {
        "two-call-mock"
    }
}

async fn run_turn(state_machine: Option<&str>) -> Result<Vec<SessionEvent>> {
    run_turn_with(state_machine, "missing__probe", None).await
}

async fn run_turn_with(
    state_machine: Option<&str>,
    tool: &'static str,
    extension: Option<ExtensionConfig>,
) -> Result<Vec<SessionEvent>> {
    let memory_dir = tempfile::tempdir()?;
    let _guard = env_lock::lock_env([
        ("KAJI_STATE_MACHINE", state_machine),
        (
            "KAJI_MEMORY_DIR",
            Some(memory_dir.path().to_str().expect("utf8 temp path")),
        ),
    ]);

    let temp_dir = tempfile::tempdir()?;
    let session_manager = Arc::new(SessionManager::new(temp_dir.path().join("data")));
    let agent = Agent::with_config(AgentConfig::new(
        Arc::clone(&session_manager),
        Arc::new(PermissionManager::new(temp_dir.path().join("config"))),
        None,
        KajiMode::Auto,
        true,
        KajiPlatform::KajiCli,
    ));
    let session = session_manager
        .create_session(
            PathBuf::from("."),
            "replay-capture-test".to_string(),
            SessionType::Hidden,
            KajiMode::Auto,
        )
        .await?;
    agent
        .update_provider(
            Arc::new(TwoCallProvider::new(tool)),
            ModelConfig::new("mock-model"),
            &session.id,
        )
        .await?;
    if let Some(extension) = extension {
        agent.add_extension(extension, &session.id).await?;
    }

    let stream = agent
        .reply(
            Message::user().with_text("capture this turn"),
            SessionConfig {
                id: session.id.clone(),
                schedule_id: None,
                max_turns: Some(3),
                retry_config: None,
            },
            None,
        )
        .await?;
    tokio::pin!(stream);
    while let Some(event) = stream.next().await {
        event?;
    }

    session_manager.session_events(&session.id).await
}

fn payloads(events: &[SessionEvent], kind: &str) -> Vec<Value> {
    events
        .iter()
        .filter(|event| event.kind == kind)
        .map(|event| serde_json::from_str(&event.payload_json).expect("payload is json"))
        .collect()
}

fn kinds(events: &[SessionEvent]) -> Vec<&str> {
    events.iter().map(|event| event.kind.as_str()).collect()
}

async fn assert_capture(state_machine: Option<&str>) -> Result<()> {
    let label = format!("KAJI_STATE_MACHINE={state_machine:?}");
    let events = run_turn(state_machine).await?;
    let kinds = kinds(&events);

    assert_eq!(
        kinds.iter().filter(|kind| **kind == "log_meta").count(),
        1,
        "{label}: exactly one log_meta per session: {kinds:?}"
    );
    let log_meta_at = kinds
        .iter()
        .position(|kind| *kind == "log_meta")
        .expect("log_meta present");
    let turn_start_at = kinds
        .iter()
        .position(|kind| *kind == "turn_start")
        .unwrap_or_else(|| panic!("{label}: turn_start present: {kinds:?}"));
    assert!(
        log_meta_at < turn_start_at,
        "{label}: log_meta must open the log, before turn_start: {kinds:?}"
    );

    let requests = payloads(&events, "llm_request");
    let responses = payloads(&events, "llm_response");
    assert_eq!(
        requests.len(),
        2,
        "{label}: two provider calls in the turn: {kinds:?}"
    );
    assert_eq!(
        responses.len(),
        requests.len(),
        "{label}: one llm_response per llm_request: {kinds:?}"
    );

    for (index, request) in requests.iter().enumerate() {
        let hash = request["request_hash"]
            .as_str()
            .unwrap_or_else(|| panic!("{label}: request_hash is a string: {request}"));
        assert_eq!(
            hash.len(),
            64,
            "{label}: request_hash is a SHA-256 hex digest: {request}"
        );
        assert!(
            hash.chars().all(|c| c.is_ascii_hexdigit()),
            "{label}: request_hash is hex: {request}"
        );
        assert_eq!(
            request["call_idx"], index as u64,
            "{label}: call_idx counts from zero within the turn: {request}"
        );
        assert_eq!(
            request["model"], "mock-model",
            "{label}: model is recorded: {request}"
        );
        assert_eq!(
            request["provider"], "two-call-mock",
            "{label}: provider is recorded: {request}"
        );
    }
    assert_eq!(
        requests[0]["turn_seq"], requests[1]["turn_seq"],
        "{label}: both calls belong to the same turn: {requests:?}"
    );
    assert_ne!(
        requests[0]["request_hash"], requests[1]["request_hash"],
        "{label}: the second call sees a longer conversation: {requests:?}"
    );

    for (index, response) in responses.iter().enumerate() {
        assert_eq!(
            response["call_idx"], index as u64,
            "{label}: llm_response is addressed by call_idx: {response}"
        );
        let chunks = response["chunks"]
            .as_array()
            .unwrap_or_else(|| panic!("{label}: chunks is an ordered array: {response}"));
        assert!(
            !chunks.is_empty(),
            "{label}: the stream's chunks are recorded: {response}"
        );
        assert_eq!(
            response["finish"], "stop",
            "{label}: a fully drained stream finishes with stop: {response}"
        );
    }

    let first_chunk = &responses[0]["chunks"][0][0];
    assert_eq!(
        first_chunk["content"][0]["type"], "toolRequest",
        "{label}: the recorded chunk is the provider's own message: {first_chunk}"
    );

    Ok(())
}

/// The id of the `toolResponse` the loop actually handed back to the model,
/// read from the v1 `message` rows of the same log.
fn tool_response_id_from_messages(events: &[SessionEvent]) -> Option<String> {
    payloads(events, "message").into_iter().find_map(|message| {
        message["content"].as_array()?.iter().find_map(|content| {
            (content["type"] == "toolResponse")
                .then(|| content["id"].as_str().map(str::to_string))
                .flatten()
        })
    })
}

async fn assert_tool_capture(state_machine: Option<&str>) -> Result<()> {
    let label = format!("KAJI_STATE_MACHINE={state_machine:?}");
    let mcp = McpFixture::new().await;
    let extension =
        ExtensionConfig::streamable_http("mcp-fixture", &mcp.url, "MCP fixture", 30_u64);
    let events = run_turn_with(state_machine, "mcp-fixture__get_code", Some(extension)).await?;
    let kinds = kinds(&events);

    let tool_results = payloads(&events, "tool_result");
    assert_eq!(
        tool_results.len(),
        1,
        "{label}: one tool_result per tool call: {kinds:?}"
    );
    let tool_result = &tool_results[0];

    let answered = tool_response_id_from_messages(&events)
        .unwrap_or_else(|| panic!("{label}: the turn carries a tool response: {kinds:?}"));
    assert_eq!(
        tool_result["tool_call_id"], answered,
        "{label}: the recorded id is the one the model was answered with: {tool_result}"
    );
    assert_eq!(
        tool_result["tool_call_id"], TOOL_REQUEST_ID,
        "{label}: the recorded id is the provider's own request id: {tool_result}"
    );

    let requests = payloads(&events, "llm_request");
    assert_eq!(
        tool_result["turn_seq"], requests[0]["turn_seq"],
        "{label}: the tool call belongs to the turn that asked for it: {tool_result}"
    );

    assert_eq!(
        tool_result["result"]["id"], TOOL_REQUEST_ID,
        "{label}: the result round-trips as a ToolResponse: {tool_result}"
    );
    assert_eq!(
        tool_result["result"]["toolResult"]["status"], "success",
        "{label}: the tool succeeded: {tool_result}"
    );
    assert_eq!(
        tool_result["result"]["toolResult"]["value"]["content"][0]["text"], FAKE_CODE,
        "{label}: the tool's own payload is recorded verbatim: {tool_result}"
    );

    Ok(())
}

#[tokio::test]
async fn llm_calls_are_captured_on_the_legacy_loop() -> Result<()> {
    assert_capture(None).await
}

#[tokio::test]
async fn llm_calls_are_captured_on_the_state_machine_loop() -> Result<()> {
    assert_capture(Some("1")).await
}

#[tokio::test]
async fn tool_results_are_captured_on_the_legacy_loop() -> Result<()> {
    assert_tool_capture(None).await
}

#[tokio::test]
async fn tool_results_are_captured_on_the_state_machine_loop() -> Result<()> {
    assert_tool_capture(Some("1")).await
}

/// Answers every call with plain text: the compaction summary goes through the
/// same provider, so a tool request would derail it.
struct TextProvider;

#[async_trait]
impl Provider for TextProvider {
    async fn stream(
        &self,
        _model_config: &ModelConfig,
        _system_prompt: &str,
        _messages: &[Message],
        _tools: &[Tool],
    ) -> Result<MessageStream, ProviderError> {
        let usage = ProviderUsage::new(
            "mock-model".to_string(),
            Usage::new(Some(100), Some(100), Some(200)),
        );
        Ok(stream_from_single_message(
            Message::assistant().with_text("mock summary"),
            usage,
        ))
    }

    fn get_name(&self) -> &str {
        "text-mock"
    }
}

fn agent_for(session_manager: &Arc<SessionManager>, config_dir: PathBuf) -> Agent {
    Agent::with_config(AgentConfig::new(
        Arc::clone(session_manager),
        Arc::new(PermissionManager::new(config_dir)),
        None,
        KajiMode::Auto,
        true,
        KajiPlatform::KajiCli,
    ))
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

/// One turn whose recall query matches a fact seeded before it runs. The
/// splice's own contract — the second element is exactly the text appended to
/// the prompt — is asserted here, where the seeded store is still mounted.
async fn run_memory_turn(state_machine: Option<&str>) -> Result<Vec<SessionEvent>> {
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

    let temp_dir = tempfile::tempdir()?;
    let working_dir = temp_dir.path().join("workspace");
    std::fs::create_dir_all(&working_dir)?;
    let session_manager = Arc::new(SessionManager::new(temp_dir.path().join("data")));
    let agent = agent_for(&session_manager, temp_dir.path().join("config"));
    let session = session_manager
        .create_session(
            working_dir.clone(),
            "replay-memory-test".to_string(),
            SessionType::Hidden,
            KajiMode::Auto,
        )
        .await?;
    agent
        .update_provider(
            Arc::new(TwoCallProvider::new("missing__probe")),
            ModelConfig::new("mock-model"),
            &session.id,
        )
        .await?;

    drain(
        agent
            .reply(
                Message::user().with_text(MEMORY_QUERY),
                session_config(&session.id),
                None,
            )
            .await?,
    )
    .await?;

    let (prompt, block) = splice_memory_block("SYSTEM", &session.id, MEMORY_QUERY, &working_dir);
    let block = block.expect("the seeded fact is recalled for this query");
    assert_eq!(
        prompt,
        format!("SYSTEM\n\n{block}"),
        "the returned block is exactly what the splice appended to the prompt"
    );

    session_manager.session_events(&session.id).await
}

async fn assert_memory_capture(state_machine: Option<&str>) -> Result<()> {
    let label = format!("KAJI_STATE_MACHINE={state_machine:?}");
    let events = run_memory_turn(state_machine).await?;
    let kinds = kinds(&events);

    let blocks = payloads(&events, "memory_block");
    assert!(
        !blocks.is_empty(),
        "{label}: the splice is journaled: {kinds:?}"
    );
    let keys: HashSet<(i64, u64)> = blocks
        .iter()
        .filter_map(|block| Some((block["turn_seq"].as_i64()?, block["call_idx"].as_u64()?)))
        .collect();
    assert_eq!(
        keys.len(),
        blocks.len(),
        "{label}: each splice is addressed by the call it fed, never positionally: {blocks:?}"
    );
    let block = blocks[0]["block"]
        .as_str()
        .unwrap_or_else(|| panic!("{label}: the block is recorded as text: {:?}", blocks[0]));
    assert!(
        block.starts_with("## KAJI memory"),
        "{label}: the block is the rendered recall, verbatim from its header on: {block}"
    );
    assert!(
        block.contains(MEMORY_FACT),
        "{label}: the recorded block carries the seeded fact: {block}"
    );

    let requests = payloads(&events, "llm_request");
    assert_eq!(
        blocks[0]["turn_seq"], requests[0]["turn_seq"],
        "{label}: the block belongs to the turn that spliced it: {:?}",
        blocks[0]
    );

    Ok(())
}

async fn assert_clock_capture(state_machine: Option<&str>) -> Result<()> {
    let label = format!("KAJI_STATE_MACHINE={state_machine:?}");
    let events = run_turn(state_machine).await?;
    let kinds = kinds(&events);

    let reads = payloads(&events, "clock_reads");
    assert_eq!(
        reads.len(),
        1,
        "{label}: exactly one clock_reads per turn: {kinds:?}"
    );
    let values = reads[0]["reads"]
        .as_array()
        .unwrap_or_else(|| panic!("{label}: reads is an ordered array: {:?}", reads[0]));
    assert_eq!(
        values.len(),
        1,
        "{label}: the turn reads the prompt clock once: {values:?}"
    );
    let served = values[0]
        .as_str()
        .unwrap_or_else(|| panic!("{label}: a clock read is a string: {values:?}"));
    assert!(
        served.contains(":00 "),
        "{label}: the recorded read is the hour-floored stamp the prompt carries: {served}"
    );

    let requests = payloads(&events, "llm_request");
    assert_eq!(
        reads[0]["turn_seq"], requests[0]["turn_seq"],
        "{label}: the reads belong to the turn that served them: {:?}",
        reads[0]
    );

    Ok(())
}

/// A turn started over the auto-compact threshold: the loop compacts before it
/// infers, so the decision must be journaled.
async fn run_compaction_turn(state_machine: Option<&str>) -> Result<Vec<SessionEvent>> {
    let memory_dir = tempfile::tempdir()?;
    let _guard = env_lock::lock_env([
        ("KAJI_STATE_MACHINE", state_machine),
        (
            "KAJI_MEMORY_DIR",
            Some(memory_dir.path().to_str().expect("utf8 temp path")),
        ),
        ("KAJI_AUTO_COMPACT_THRESHOLD", Some("0.5")),
    ]);

    let temp_dir = tempfile::tempdir()?;
    let working_dir = temp_dir.path().join("workspace");
    std::fs::create_dir_all(&working_dir)?;
    let session_manager = Arc::new(SessionManager::new(temp_dir.path().join("data")));
    let agent = agent_for(&session_manager, temp_dir.path().join("config"));
    let session = session_manager
        .create_session(
            working_dir,
            "replay-compaction-test".to_string(),
            SessionType::Hidden,
            KajiMode::Auto,
        )
        .await?;

    let history = (0..4)
        .flat_map(|i| {
            [
                Message::user().with_text(format!("question {i}")),
                Message::assistant().with_text(format!("answer {i}")),
            ]
        })
        .collect::<Vec<_>>();
    session_manager
        .replace_conversation(&session.id, &Conversation::new_unvalidated(history))
        .await?;
    session_manager
        .update(&session.id)
        .usage(Usage::new(Some(90_000), Some(10_000), Some(100_000)))
        .accumulated_usage(Usage::new(Some(90_000), Some(10_000), Some(100_000)))
        .apply()
        .await?;

    agent
        .update_provider(
            Arc::new(TextProvider),
            ModelConfig::new("mock-model"),
            &session.id,
        )
        .await?;

    drain(
        agent
            .reply(
                Message::user().with_text("keep going"),
                session_config(&session.id),
                None,
            )
            .await?,
    )
    .await?;

    session_manager.session_events(&session.id).await
}

async fn assert_condense_capture(state_machine: Option<&str>) -> Result<()> {
    let label = format!("KAJI_STATE_MACHINE={state_machine:?}");
    let events = run_compaction_turn(state_machine).await?;
    let kinds = kinds(&events);

    assert!(
        kinds.contains(&"history_replaced"),
        "{label}: the turn really compacted, so the test is not vacuous: {kinds:?}"
    );

    let triggered = payloads(&events, "condense_triggered");
    assert_eq!(
        triggered.len(),
        1,
        "{label}: the compaction decision is journaled once: {kinds:?}"
    );
    assert_eq!(
        triggered[0]["reason"], "auto_compact_threshold",
        "{label}: the reason names what tripped the decision: {:?}",
        triggered[0]
    );

    let summaries = payloads(&events, "condense_summary");
    assert_eq!(
        summaries.len(),
        1,
        "{label}: the summarization call is journaled too — it goes through \
         Provider::complete, off the loop's call_idx channel: {kinds:?}"
    );
    assert!(
        summaries[0]["summary"]["content"].is_array(),
        "{label}: the summary is journaled as the message the replay will splice back: {:?}",
        summaries[0]
    );

    let turn_start = payloads(&events, "turn_start");
    assert_eq!(turn_start.len(), 1, "{label}: one turn ran: {kinds:?}");
    let condense_at = kinds
        .iter()
        .position(|kind| *kind == "condense_triggered")
        .expect("condense_triggered present");
    let turn_start_at = kinds
        .iter()
        .position(|kind| *kind == "turn_start")
        .expect("turn_start present");
    assert!(
        turn_start_at < condense_at,
        "{label}: the decision is journaled inside its own turn: {kinds:?}"
    );

    Ok(())
}

/// A turn under the threshold must not journal a decision it never took.
async fn assert_no_condense_without_compaction(state_machine: Option<&str>) -> Result<()> {
    let label = format!("KAJI_STATE_MACHINE={state_machine:?}");
    let events = run_turn(state_machine).await?;
    assert!(
        payloads(&events, "condense_triggered").is_empty(),
        "{label}: no compaction, no condense_triggered: {:?}",
        kinds(&events)
    );
    Ok(())
}

#[tokio::test]
async fn memory_block_is_captured_on_the_legacy_loop() -> Result<()> {
    assert_memory_capture(None).await
}

#[tokio::test]
async fn memory_block_is_captured_on_the_state_machine_loop() -> Result<()> {
    assert_memory_capture(Some("1")).await
}

#[tokio::test]
async fn clock_reads_are_captured_on_the_legacy_loop() -> Result<()> {
    assert_clock_capture(None).await
}

#[tokio::test]
async fn clock_reads_are_captured_on_the_state_machine_loop() -> Result<()> {
    assert_clock_capture(Some("1")).await
}

#[tokio::test]
async fn condense_is_captured_on_the_legacy_loop() -> Result<()> {
    assert_condense_capture(None).await
}

#[tokio::test]
async fn condense_is_captured_on_the_state_machine_loop() -> Result<()> {
    assert_condense_capture(Some("1")).await
}

/// Un tour ouvert par une slash-command. Les deux boucles rendent le message
/// de commande en `AgentEvent` en plus de l'enveloppe qui le journalise déjà.
async fn run_slash_command_turn(state_machine: Option<&str>) -> Result<Vec<SessionEvent>> {
    let memory_dir = tempfile::tempdir()?;
    let _guard = env_lock::lock_env([
        ("KAJI_STATE_MACHINE", state_machine),
        (
            "KAJI_MEMORY_DIR",
            Some(memory_dir.path().to_str().expect("utf8 temp path")),
        ),
    ]);

    let temp_dir = tempfile::tempdir()?;
    let session_manager = Arc::new(SessionManager::new(temp_dir.path().join("data")));
    let agent = agent_for(&session_manager, temp_dir.path().join("config"));
    let session = session_manager
        .create_session(
            PathBuf::from("."),
            "replay-slash-command-test".to_string(),
            SessionType::Hidden,
            KajiMode::Auto,
        )
        .await?;
    agent
        .update_provider(
            Arc::new(TextProvider),
            ModelConfig::new("mock-model"),
            &session.id,
        )
        .await?;

    drain(
        agent
            .reply(
                Message::user().with_text("/clear"),
                session_config(&session.id),
                None,
            )
            .await?,
    )
    .await?;

    session_manager.session_events(&session.id).await
}

fn message_rows(events: &[SessionEvent], role: Role) -> Vec<Message> {
    events
        .iter()
        .filter(|event| event.kind == "message")
        .filter_map(|event| serde_json::from_str::<Message>(&event.payload_json).ok())
        .filter(|message| message.role == role)
        .collect()
}

/// Les lignes `message` qui portent le texte du message d'ouverture — la
/// réponse d'outil d'un tour est elle aussi un message de rôle user.
fn opening_message_rows(events: &[SessionEvent], text: &str) -> Vec<Message> {
    message_rows(events, Role::User)
        .into_iter()
        .filter(|message| message.as_concat_text().contains(text))
        .collect()
}

/// Un message d'ouverture, une ligne `message`. L'enveloppe de `reply()`
/// journalise le message qui ouvre le tour ; le chemin des slash-commands le
/// rend en plus en `AgentEvent`, et l'enveloppe journalise tout événement —
/// deux lignes pour un seul message, avec des payloads différents.
async fn assert_one_opening_message_row(state_machine: Option<&str>) -> Result<()> {
    let label = format!("KAJI_STATE_MACHINE={state_machine:?}");
    let events = run_slash_command_turn(state_machine).await?;

    let confirmations = message_rows(&events, Role::Assistant);
    assert!(
        confirmations
            .iter()
            .any(|message| message.as_concat_text().contains("Conversation cleared")),
        "{label}: la commande a bien tourné, sinon le test ne prouve rien: {:?}",
        kinds(&events)
    );

    let opening = opening_message_rows(&events, "/clear");
    assert_eq!(
        opening.len(),
        1,
        "{label}: une seule ligne `message` pour le message qui ouvre le tour: {opening:?}"
    );

    Ok(())
}

#[tokio::test]
async fn the_opening_message_is_journaled_once_on_the_legacy_loop() -> Result<()> {
    assert_one_opening_message_row(None).await
}

#[tokio::test]
async fn the_opening_message_is_journaled_once_on_the_state_machine_loop() -> Result<()> {
    assert_one_opening_message_row(Some("1")).await
}

/// Le chemin normal (aucune slash-command) n'a jamais journalisé qu'une ligne :
/// le contrôle ci-dessus ne doit pas la faire disparaître.
async fn assert_one_opening_message_row_without_command(state_machine: Option<&str>) -> Result<()> {
    let label = format!("KAJI_STATE_MACHINE={state_machine:?}");
    let events = run_turn(state_machine).await?;

    let opening = opening_message_rows(&events, "capture this turn");
    assert_eq!(
        opening.len(),
        1,
        "{label}: le message qui ouvre un tour normal est journalisé une fois: {opening:?}"
    );

    Ok(())
}

#[tokio::test]
async fn a_plain_turn_journals_its_opening_message_on_the_legacy_loop() -> Result<()> {
    assert_one_opening_message_row_without_command(None).await
}

#[tokio::test]
async fn a_plain_turn_journals_its_opening_message_on_the_state_machine_loop() -> Result<()> {
    assert_one_opening_message_row_without_command(Some("1")).await
}

#[tokio::test]
async fn a_quiet_turn_journals_no_condense_on_the_legacy_loop() -> Result<()> {
    assert_no_condense_without_compaction(None).await
}

#[tokio::test]
async fn a_quiet_turn_journals_no_condense_on_the_state_machine_loop() -> Result<()> {
    assert_no_condense_without_compaction(Some("1")).await
}

/// L'environnement d'outils bouge en cours de tour : la machine à états
/// réassemble avant chaque appel du provider, la boucle legacy réassemble dès
/// qu'une extension est installée ou qu'un hint apparaît. Un manifeste par tour
/// servirait donc celui de l'appel 0 aux appels suivants — le manifeste est
/// adressé `(turn_seq, call_idx)` comme le bloc mémoire.
async fn assert_manifest_is_addressed_per_call(state_machine: Option<&str>) -> Result<()> {
    let label = format!("KAJI_STATE_MACHINE={state_machine:?}");
    let events = run_turn(state_machine).await?;

    let manifests = payloads(&events, "tool_manifest");
    assert!(
        !manifests.is_empty(),
        "{label}: le tour journalise son environnement d'outils: {:?}",
        kinds(&events)
    );
    for manifest in &manifests {
        assert!(
            manifest["call_idx"].is_u64(),
            "{label}: chaque manifeste nomme l'appel qu'il a nourri: {manifest}"
        );
        assert!(
            manifest["turn_seq"].is_i64(),
            "{label}: chaque manifeste nomme son tour: {manifest}"
        );
    }

    let calls: Vec<u64> = manifests
        .iter()
        .filter_map(|manifest| manifest["call_idx"].as_u64())
        .collect();
    let requests = payloads(&events, "llm_request");
    assert_eq!(
        requests.len(),
        2,
        "{label}: le tour fait bien deux appels, sinon le test ne prouve rien"
    );
    if state_machine.is_some() {
        assert_eq!(
            calls,
            vec![0, 1],
            "{label}: la machine à états réassemble avant chaque appel — un manifeste par appel"
        );
    } else {
        assert_eq!(
            calls,
            vec![0],
            "{label}: la boucle legacy n'assemble qu'une fois par tour, à l'appel 0"
        );
    }

    Ok(())
}

#[tokio::test]
async fn the_tool_manifest_is_addressed_per_call_on_the_legacy_loop() -> Result<()> {
    assert_manifest_is_addressed_per_call(None).await
}

#[tokio::test]
async fn the_tool_manifest_is_addressed_per_call_on_the_state_machine_loop() -> Result<()> {
    assert_manifest_is_addressed_per_call(Some("1")).await
}

/// Un tour peut compacter deux fois (auto-compaction d'ouverture, puis
/// compaction de secours sur `ContextLengthExceeded`). Sans `call_idx`, le
/// curseur écrase le premier résumé par le second et le rejeu sert le mauvais.
async fn assert_condense_summary_is_addressed_per_call(state_machine: Option<&str>) -> Result<()> {
    let label = format!("KAJI_STATE_MACHINE={state_machine:?}");
    let events = run_compaction_turn(state_machine).await?;

    let summaries = payloads(&events, "condense_summary");
    assert_eq!(
        summaries.len(),
        1,
        "{label}: le tour a compacté une fois: {:?}",
        kinds(&events)
    );
    assert!(
        summaries[0]["call_idx"].is_u64(),
        "{label}: le résumé nomme l'appel devant lequel il a été produit: {:?}",
        summaries[0]
    );

    Ok(())
}

/// Assez de paires d'outils agent-visibles pour dépasser
/// `cutoff + TOOLCALL_SUMMARIZATION_BATCH_SIZE`, seuil au-delà duquel la boucle
/// résume le plus vieux lot.
const SEEDED_PAIRS: usize = 14;

/// Un tour dont l'historique dépasse le seuil de résumé de paires d'outils. Le
/// résumé masque les deux messages de la paire et en persiste un troisième :
/// c'est de la conversation persistée, donc du hash du tour suivant.
async fn run_tool_pair_summarization_turn(
    state_machine: Option<&str>,
) -> Result<(Vec<SessionEvent>, Vec<Message>)> {
    let memory_dir = tempfile::tempdir()?;
    let _guard = env_lock::lock_env([
        ("KAJI_STATE_MACHINE", state_machine),
        (
            "KAJI_MEMORY_DIR",
            Some(memory_dir.path().to_str().expect("utf8 temp path")),
        ),
        ("KAJI_TOOL_CALL_CUTOFF", Some("0")),
    ]);

    let temp_dir = tempfile::tempdir()?;
    let session_manager = Arc::new(SessionManager::new(temp_dir.path().join("data")));
    let agent = agent_for(&session_manager, temp_dir.path().join("config"));
    let session = session_manager
        .create_session(
            PathBuf::from("."),
            "replay-tool-pair-test".to_string(),
            SessionType::Hidden,
            KajiMode::Auto,
        )
        .await?;

    let history = (0..SEEDED_PAIRS)
        .flat_map(|i| {
            let call_id = format!("seeded-call-{i}");
            [
                Message::assistant()
                    .with_tool_request(
                        &call_id,
                        Ok(CallToolRequestParams::new("seeded__probe".to_string())),
                    )
                    .with_id(format!("msg-request-{i}")),
                Message::user()
                    .with_tool_response(
                        &call_id,
                        Ok(rmcp::model::CallToolResult::success(vec![
                            rmcp::model::ContentBlock::text(format!("result {i}")),
                        ])),
                    )
                    .with_id(format!("msg-response-{i}")),
            ]
        })
        .collect::<Vec<_>>();
    session_manager
        .replace_conversation(&session.id, &Conversation::new_unvalidated(history))
        .await?;

    agent
        .update_provider(
            Arc::new(TextProvider),
            ModelConfig::new("mock-model"),
            &session.id,
        )
        .await?;

    drain(
        agent
            .reply(
                Message::user().with_text("keep going"),
                session_config(&session.id),
                None,
            )
            .await?,
    )
    .await?;

    let conversation = session_manager
        .get_session(&session.id, true)
        .await?
        .conversation
        .map(|conversation| conversation.messages().clone())
        .unwrap_or_default();
    Ok((
        session_manager.session_events(&session.id).await?,
        conversation,
    ))
}

/// Le résumé de paires d'outils modifie la conversation persistée : sans
/// capture, le tour suivant du rejeu porte encore la paire complète et aucun
/// résumé, donc un hash divergent. Il est journalisé comme `condense_summary`,
/// sous la clé qui l'adresse — l'id de l'appel d'outil résumé.
async fn assert_tool_pair_summary_is_captured(state_machine: Option<&str>) -> Result<()> {
    let label = format!("KAJI_STATE_MACHINE={state_machine:?}");
    let (events, conversation) = run_tool_pair_summarization_turn(state_machine).await?;

    assert!(
        conversation.iter().any(|message| message
            .id
            .as_deref()
            .is_some_and(|id| id.starts_with("msg-request-"))
            && !message.is_agent_visible()),
        "{label}: le tour a bien résumé des paires — sinon le test ne prouve rien"
    );

    let summaries = payloads(&events, "tool_pair_summary");
    assert!(
        !summaries.is_empty(),
        "{label}: le tour a résumé des paires, elles sont journalisées: {:?}",
        kinds(&events)
    );
    for summary in &summaries {
        assert!(
            summary["tool_call_id"]
                .as_str()
                .is_some_and(|id| id.starts_with("seeded-call-")),
            "{label}: le résumé est adressé par l'id de l'appel qu'il remplace: {summary}"
        );
        assert!(
            summary["summary"]["content"].is_array(),
            "{label}: le résumé est journalisé comme le message que le rejeu resplicera: {summary}"
        );
        assert!(
            summary["summary"]["id"].is_string(),
            "{label}: le résumé porte l'id que la conversation persistée a gardé: {summary}"
        );
    }

    Ok(())
}

#[tokio::test]
async fn the_tool_pair_summary_is_captured_on_the_legacy_loop() -> Result<()> {
    assert_tool_pair_summary_is_captured(None).await
}

#[tokio::test]
async fn the_tool_pair_summary_is_captured_on_the_state_machine_loop() -> Result<()> {
    assert_tool_pair_summary_is_captured(Some("1")).await
}

#[tokio::test]
async fn the_condense_summary_is_addressed_per_call_on_the_legacy_loop() -> Result<()> {
    assert_condense_summary_is_addressed_per_call(None).await
}

#[tokio::test]
async fn the_condense_summary_is_addressed_per_call_on_the_state_machine_loop() -> Result<()> {
    assert_condense_summary_is_addressed_per_call(Some("1")).await
}
