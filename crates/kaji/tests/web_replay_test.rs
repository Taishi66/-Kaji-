//! Le rejeu d'un tour qui a fait du web : `web_fetch` passe par le dispatch
//! standard, donc son `tool_result` est journalisé puis servi depuis le journal.
//! Le serveur cible compte ses requêtes — le rejeu n'en ajoute aucune.

use anyhow::Result;
use async_trait::async_trait;
use futures::StreamExt;
use kaji::agents::platform_extensions::web;
use kaji::agents::{Agent, AgentConfig, AgentEvent, ExtensionConfig, KajiPlatform, SessionConfig};
use kaji::config::permission::PermissionManager;
use kaji::config::KajiMode;
use kaji::conversation::message::{Message, MessageContent};
use kaji::providers::base::{stream_from_single_message, MessageStream, Provider};
use kaji::replay::cursor::EventCursor;
use kaji::replay::idgen::SessionIdGen;
use kaji::replay::mode::ReplayMode;
use kaji::replay::provider::ReplayProvider;
use kaji::replay::source::ReplaySource;
use kaji::session::session_manager::SessionType;
use kaji::session::SessionManager;
use kaji_providers::conversation::token_usage::{ProviderUsage, Usage};
use kaji_providers::errors::ProviderError;
use kaji_providers::model::ModelConfig;
use rmcp::model::{CallToolRequestParams, Tool};
use rmcp::object;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tempfile::TempDir;

const TOOL_REQUEST_ID: &str = "web-1";
const PAYLOAD: &str = "le-corps-enregistre";

struct Site {
    base: String,
    hits: Arc<AtomicUsize>,
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for Site {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn site() -> Site {
    use axum::extract::State;
    use axum::http::header;
    use axum::response::IntoResponse;
    use axum::routing::get;

    let hits = Arc::new(AtomicUsize::new(0));

    async fn html(State(hits): State<Arc<AtomicUsize>>) -> impl IntoResponse {
        hits.fetch_add(1, Ordering::SeqCst);
        (
            [(header::CONTENT_TYPE, "text/html")],
            format!("<html><body><p>{PAYLOAD}</p></body></html>"),
        )
    }

    let router = axum::Router::new()
        .route("/p", get(html))
        .with_state(Arc::clone(&hits));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let handle = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    Site { base, hits, handle }
}

struct FixtureProvider {
    url: String,
}

#[async_trait]
impl Provider for FixtureProvider {
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
            Usage::new(Some(1), Some(2), Some(3)),
        );
        let message = if already_called {
            Message::assistant().with_text("done")
        } else {
            Message::assistant().with_tool_request(
                TOOL_REQUEST_ID,
                Ok(
                    CallToolRequestParams::new(web::WEB_FETCH_TOOL).with_arguments(object!({
                        "url": self.url.clone(),
                        "mode": "markdown",
                    })),
                ),
            )
        };
        Ok(stream_from_single_message(message, usage))
    }

    fn get_name(&self) -> &str {
        "fixture-mock"
    }
}

async fn drain(stream: impl futures::Stream<Item = Result<AgentEvent>>) -> Result<Vec<AgentEvent>> {
    tokio::pin!(stream);
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event?);
    }
    Ok(events)
}

fn session_config(id: &str) -> SessionConfig {
    SessionConfig {
        id: id.to_string(),
        schedule_id: None,
        max_turns: Some(3),
        retry_config: None,
    }
}

fn new_agent(session_manager: &Arc<SessionManager>, data_dir: &TempDir) -> Agent {
    Agent::with_config(AgentConfig::new(
        Arc::clone(session_manager),
        Arc::new(PermissionManager::new(data_dir.path().join("config"))),
        None,
        KajiMode::Auto,
        true,
        KajiPlatform::KajiCli,
    ))
}

fn web_extension() -> ExtensionConfig {
    ExtensionConfig::Platform {
        name: web::EXTENSION_NAME.to_string(),
        description: "web".to_string(),
        display_name: Some("Web".to_string()),
        bundled: Some(true),
        available_tools: vec![],
    }
}

async fn conversation_text(manager: &Arc<SessionManager>, session_id: &str) -> Result<String> {
    let session = manager.get_session(session_id, true).await?;
    let conversation = session
        .conversation
        .expect("la session porte sa conversation");
    Ok(conversation
        .messages()
        .iter()
        .map(|message| format!("{message:?}"))
        .collect::<Vec<_>>()
        .join("\n"))
}

async fn assert_web_fetch_replays_without_network(state_machine: Option<&str>) -> Result<()> {
    let label = format!("KAJI_STATE_MACHINE={state_machine:?}");
    let memory_dir = tempfile::tempdir()?;
    let _guard = env_lock::lock_env([
        ("KAJI_STATE_MACHINE", state_machine),
        ("KAJI_WEB_ALLOW_PRIVATE", Some("1")),
        (
            "KAJI_MEMORY_DIR",
            Some(memory_dir.path().to_str().expect("chemin utf8")),
        ),
    ]);

    let site = site().await;
    let url = format!("{}/p", site.base);

    let data_dir = tempfile::tempdir()?;
    let working_dir = data_dir.path().join("workspace");
    std::fs::create_dir_all(&working_dir)?;
    let session_manager = Arc::new(SessionManager::new(data_dir.path().join("data")));

    let agent = new_agent(&session_manager, &data_dir);
    let session = session_manager
        .create_session(
            working_dir.clone(),
            "web-replay-test".to_string(),
            SessionType::Hidden,
            KajiMode::Auto,
        )
        .await?;
    agent
        .update_provider(
            Arc::new(FixtureProvider { url: url.clone() }),
            ModelConfig::new("mock-model"),
            &session.id,
        )
        .await?;
    agent.add_extension(web_extension(), &session.id).await?;

    drain(
        agent
            .reply(
                Message::user().with_text("lis cette page"),
                session_config(&session.id),
                None,
            )
            .await?,
    )
    .await?;

    let recorded = conversation_text(&session_manager, &session.id).await?;
    assert!(
        recorded.contains(PAYLOAD),
        "{label}: le tour enregistré a bien récupéré la page : {recorded}"
    );
    let hits_after_record = site.hits.load(Ordering::SeqCst);
    assert_eq!(hits_after_record, 1, "{label}: une requête réelle");

    let cursor = Arc::new(EventCursor::load(&session_manager, &session.id).await?);
    assert!(
        cursor.tool_results.contains_key(TOOL_REQUEST_ID),
        "{label}: le résultat de web_fetch est au journal"
    );
    let turn = session_manager
        .session_events(&session.id)
        .await?
        .into_iter()
        .find(|event| event.kind == "llm_request")
        .expect("un appel llm journalisé")
        .turn_seq;

    // Le rejeu vise une URL injoignable : si la garde ou le dispatch tentaient
    // le réseau, le tour échouerait au lieu de servir le corps du journal.
    let replay_session = session_manager
        .create_session(
            working_dir,
            format!("replay-of-{}", session.id),
            SessionType::Hidden,
            KajiMode::Auto,
        )
        .await?;
    let provider = ReplayProvider::new(Arc::clone(&cursor), true);
    let position = provider.position();
    let mut replay = new_agent(&session_manager, &data_dir);
    replay.set_idgen(Arc::new(SessionIdGen::new(&cursor.log_meta.idgen_seed)));
    replay.set_replay_mode(ReplayMode::new(session.id.clone(), KajiMode::Auto));
    replay.set_replay_source(ReplaySource::new(cursor, Arc::clone(&position)));
    replay
        .update_provider(
            Arc::new(provider),
            ModelConfig::new("mock-model"),
            &replay_session.id,
        )
        .await?;
    replay
        .add_extension(web_extension(), &replay_session.id)
        .await?;

    site.handle.abort();
    position.begin_turn(turn);
    drain(
        replay
            .reply(
                Message::user().with_text("lis cette page"),
                session_config(&replay_session.id),
                None,
            )
            .await?,
    )
    .await?;

    let replayed = conversation_text(&session_manager, &replay_session.id).await?;
    assert!(
        replayed.contains(PAYLOAD),
        "{label}: le corps rejoué vient du journal : {replayed}"
    );
    assert_eq!(
        site.hits.load(Ordering::SeqCst),
        hits_after_record,
        "{label}: le rejeu n'a touché personne"
    );
    Ok(())
}

#[tokio::test]
async fn web_fetch_replays_without_network_on_the_legacy_loop() -> Result<()> {
    assert_web_fetch_replays_without_network(None).await
}

#[tokio::test]
async fn web_fetch_replays_without_network_on_the_state_machine_loop() -> Result<()> {
    assert_web_fetch_replays_without_network(Some("1")).await
}
