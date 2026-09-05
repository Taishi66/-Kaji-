//! L'ordre des `tool_response` suit l'ordre des requêtes, pas celui
//! d'achèvement des futures.
//!
//! Deux appels d'outil d'une même réponse s'exécutent en parallèle. Ici le
//! premier demandé attend explicitement que le second ait rendu son résultat :
//! ils finissent donc dans l'ordre **inverse** de leurs requêtes, sans dépendre
//! d'un délai ni de l'ordonnanceur. La conversation persistée doit malgré tout
//! porter les réponses dans l'ordre des requêtes.
//!
//! Sinon l'enregistrement est aléatoire là où le rejeu est déterministe : le
//! tour suivant se réassemble sur une conversation différente et diverge sur
//! son hash de requête (échec bruyant, mais spurieux). C'est la cause de flake
//! mesurée sur les suites `replay_*`.

use anyhow::Result;
use async_trait::async_trait;
use futures::StreamExt;
use kaji::agents::{Agent, AgentConfig, ExtensionConfig, KajiPlatform, SessionConfig};
use kaji::config::permission::PermissionManager;
use kaji::config::KajiMode;
use kaji::conversation::message::{Message, MessageContent};
use kaji::providers::base::{stream_from_single_message, MessageStream, Provider};
use kaji::session::session_manager::SessionType;
use kaji::session::SessionManager;
use kaji_providers::conversation::token_usage::{ProviderUsage, Usage};
use kaji_providers::errors::ProviderError;
use kaji_providers::model::ModelConfig;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ContentBlock, Implementation, InitializeResult,
    ProtocolVersion, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
};
use rmcp::{tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tokio::task::JoinHandle;

/// Les deux extensions montées : le nom préfixe les outils envoyés au modèle.
const SLOW_EXTENSION: &str = "slowside";
const FAST_EXTENSION: &str = "fastside";

const SLOW_TOOL: &str = "slowside__wait_for_peer";
const FAST_TOOL: &str = "fastside__finish_first";

/// Le premier appel demandé est celui qui finit en dernier.
const FIRST_REQUEST_ID: &str = "order-probe-slow";
const SECOND_REQUEST_ID: &str = "order-probe-fast";

/// Armé par l'outil rapide, attendu par le lent : l'ordre d'achèvement est
/// imposé par le fixture, pas par l'ordonnanceur.
static FAST_DONE: AtomicBool = AtomicBool::new(false);

/// Garde-fou : si l'outil rapide n'a jamais tourné, le test doit échouer sur
/// son assertion, pas se figer.
const WAIT_CAP: Duration = Duration::from_secs(10);

#[derive(Clone, Default)]
struct OrderProbe;

#[tool_router]
impl OrderProbe {
    #[tool(
        description = "Wait until the peer tool has finished",
        annotations(read_only_hint = true)
    )]
    async fn wait_for_peer(&self) -> Result<CallToolResult, McpError> {
        let deadline = tokio::time::Instant::now() + WAIT_CAP;
        while !FAST_DONE.load(Ordering::SeqCst) {
            if tokio::time::Instant::now() >= deadline {
                return Ok(CallToolResult::error(vec![ContentBlock::text(
                    "peer tool never completed",
                )]));
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        Ok(CallToolResult::success(vec![ContentBlock::text(
            "slow-payload",
        )]))
    }

    #[tool(description = "Finish immediately", annotations(read_only_hint = true))]
    async fn finish_first(&self) -> Result<CallToolResult, McpError> {
        FAST_DONE.store(true, Ordering::SeqCst);
        Ok(CallToolResult::success(vec![ContentBlock::text(
            "fast-payload",
        )]))
    }
}

#[tool_handler]
impl ServerHandler for OrderProbe {
    fn get_info(&self) -> ServerInfo {
        InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
            .with_protocol_version(ProtocolVersion::LATEST)
            .with_server_info(Implementation::new("order-probe", "1.0.0"))
            .with_instructions("Ordering probe.")
    }
}

/// Deux serveurs distincts : chaque appel part sur sa propre connexion, donc
/// rien ne les sérialise côté client.
struct ProbeFixture {
    slow_url: String,
    fast_url: String,
    handles: Vec<JoinHandle<()>>,
}

impl Drop for ProbeFixture {
    fn drop(&mut self) {
        for handle in &self.handles {
            handle.abort();
        }
    }
}

impl ProbeFixture {
    async fn new() -> Self {
        let (slow_url, slow) = Self::serve().await;
        let (fast_url, fast) = Self::serve().await;
        Self {
            slow_url,
            fast_url,
            handles: vec![slow, fast],
        }
    }

    async fn serve() -> (String, JoinHandle<()>) {
        let service = StreamableHttpService::new(
            || Ok::<_, std::io::Error>(OrderProbe),
            LocalSessionManager::default().into(),
            StreamableHttpServerConfig::default(),
        );
        let router = axum::Router::new().nest_service("/mcp", service);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let handle = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        (url, handle)
    }
}

/// Le premier tour demande les deux outils dans l'ordre lent puis rapide ; une
/// fois répondus, le modèle conclut.
struct OrderingProvider;

#[async_trait]
impl Provider for OrderingProvider {
    async fn stream(
        &self,
        _model_config: &ModelConfig,
        _system: &str,
        messages: &[Message],
        _tools: &[Tool],
    ) -> Result<MessageStream, ProviderError> {
        let already_called = messages.iter().any(|message| {
            message
                .content
                .iter()
                .any(|content| matches!(content, MessageContent::ToolResponse(_)))
        });
        let usage = ProviderUsage::new(
            "mock-model".to_string(),
            Usage::new(Some(11), Some(22), Some(33)),
        );
        let message = if already_called {
            Message::assistant().with_text("done")
        } else {
            Message::assistant()
                .with_tool_request(FIRST_REQUEST_ID, Ok(CallToolRequestParams::new(SLOW_TOOL)))
                .with_tool_request(SECOND_REQUEST_ID, Ok(CallToolRequestParams::new(FAST_TOOL)))
        };
        Ok(stream_from_single_message(message, usage))
    }

    fn get_name(&self) -> &str {
        "ordering-mock"
    }
}

fn session_config(id: &str) -> SessionConfig {
    SessionConfig {
        id: id.to_string(),
        schedule_id: None,
        max_turns: Some(3),
        retry_config: None,
    }
}

fn env<'a>(state_machine: Option<&'a str>, memory_dir: &'a TempDir) -> env_lock::EnvGuard<'a> {
    env_lock::lock_env([
        ("KAJI_STATE_MACHINE", state_machine),
        (
            "KAJI_MEMORY_DIR",
            Some(memory_dir.path().to_str().expect("utf8 temp path")),
        ),
    ])
}

/// Les ids de `tool_response` de la conversation persistée, dans l'ordre où
/// elle les porte.
async fn persisted_response_ids(
    session_manager: &Arc<SessionManager>,
    session_id: &str,
) -> Result<Vec<String>> {
    let session = session_manager.get_session(session_id, true).await?;
    Ok(session
        .conversation
        .map(|conversation| {
            conversation
                .messages()
                .iter()
                .flat_map(|message| message.content.iter())
                .filter_map(|content| match content {
                    MessageContent::ToolResponse(response) => Some(response.id.clone()),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default())
}

async fn assert_tool_responses_follow_request_order(state_machine: Option<&str>) -> Result<()> {
    let label = format!("KAJI_STATE_MACHINE={state_machine:?}");
    let memory_dir = tempfile::tempdir()?;
    let _guard = env(state_machine, &memory_dir);

    FAST_DONE.store(false, Ordering::SeqCst);
    let probe = ProbeFixture::new().await;

    let data_dir = tempfile::tempdir()?;
    let working_dir = data_dir.path().join("workspace");
    std::fs::create_dir_all(&working_dir)?;

    let session_manager = Arc::new(SessionManager::new(data_dir.path().join("data")));
    let agent = Agent::with_config(AgentConfig::new(
        Arc::clone(&session_manager),
        Arc::new(PermissionManager::new(data_dir.path().join("config"))),
        None,
        KajiMode::Auto,
        false,
        KajiPlatform::KajiCli,
    ));
    let session = session_manager
        .create_session(
            working_dir,
            "replay-tool-order-test".to_string(),
            SessionType::Hidden,
            KajiMode::Auto,
        )
        .await?;

    agent
        .update_provider(
            Arc::new(OrderingProvider),
            ModelConfig::new("mock-model"),
            &session.id,
        )
        .await?;
    agent
        .add_extension(
            ExtensionConfig::streamable_http(
                SLOW_EXTENSION,
                &probe.slow_url,
                "slow side of the probe",
                30_u64,
            ),
            &session.id,
        )
        .await?;
    agent
        .add_extension(
            ExtensionConfig::streamable_http(
                FAST_EXTENSION,
                &probe.fast_url,
                "fast side of the probe",
                30_u64,
            ),
            &session.id,
        )
        .await?;

    let stream = agent
        .reply(
            Message::user().with_text("call both tools"),
            session_config(&session.id),
            None,
        )
        .await?;
    tokio::pin!(stream);
    while let Some(event) = stream.next().await {
        let _ = event?;
    }

    assert!(
        FAST_DONE.load(Ordering::SeqCst),
        "{label}: l'outil rapide n'a pas tourné — le fixture n'impose aucun ordre inverse"
    );

    let ids = persisted_response_ids(&session_manager, &session.id).await?;
    assert_eq!(
        ids,
        vec![FIRST_REQUEST_ID.to_string(), SECOND_REQUEST_ID.to_string()],
        "{label}: les tool_response suivent l'ordre des requêtes, pas celui d'achèvement"
    );
    Ok(())
}

#[tokio::test]
async fn tool_responses_follow_request_order_on_the_legacy_loop() -> Result<()> {
    assert_tool_responses_follow_request_order(None).await
}

#[tokio::test]
async fn tool_responses_follow_request_order_on_the_state_machine_loop() -> Result<()> {
    assert_tool_responses_follow_request_order(Some("1")).await
}
