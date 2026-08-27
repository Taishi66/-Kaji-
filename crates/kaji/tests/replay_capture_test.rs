use anyhow::Result;
use async_trait::async_trait;
use futures::StreamExt;
use kaji::agents::{Agent, AgentConfig, ExtensionConfig, KajiPlatform, SessionConfig};
use kaji::config::permission::PermissionManager;
use kaji::config::KajiMode;
use kaji::conversation::message::Message;
use kaji::providers::base::{stream_from_single_message, MessageStream, Provider};
use kaji::session::session_manager::{SessionEvent, SessionType};
use kaji::session::SessionManager;
use kaji_providers::conversation::token_usage::{ProviderUsage, Usage};
use kaji_providers::errors::ProviderError;
use kaji_providers::model::ModelConfig;
use kaji_test_support::{McpFixture, FAKE_CODE};
use rmcp::model::{CallToolRequestParams, Tool};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

const TOOL_REQUEST_ID: &str = "probe-1";

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
