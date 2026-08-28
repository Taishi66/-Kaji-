//! Les intercepteurs du rejeu : un tour rejoué sert ses résultats d'outils, son
//! bloc mémoire et son horloge depuis le journal — il n'exécute jamais l'outil.
//!
//! L'outil de la fixture est instrumenté : il compte ses exécutions et panique
//! s'il est appelé pendant un rejeu. Le compteur est l'assertion, la panique la
//! sécurité — un intercepteur manquant se voit des deux côtés.

use anyhow::Result;
use async_trait::async_trait;
use futures::StreamExt;
use kaji::agents::{Agent, AgentConfig, AgentEvent, ExtensionConfig, KajiPlatform, SessionConfig};
use kaji::config::permission::PermissionManager;
use kaji::config::KajiMode;
use kaji::conversation::message::{Message, MessageContent};
use kaji::kaji::SessionMemory;
use kaji::permission::permission_confirmation::PrincipalType;
use kaji::permission::{Permission, PermissionConfirmation};
use kaji::providers::base::{stream_from_single_message, MessageStream, Provider};
use kaji::replay::cursor::EventCursor;
use kaji::replay::idgen::SessionIdGen;
use kaji::replay::mode::ReplayMode;
use kaji::replay::provider::{ReplayPosition, ReplayProvider};
use kaji::replay::source::ReplaySource;
use kaji::session::session_manager::{SessionEvent, SessionType, DB_NAME, SESSIONS_FOLDER};
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
use serde_json::json;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tempfile::TempDir;
use tokio::task::JoinHandle;

const TOOL_REQUEST_ID: &str = "probe-1";
const TOOL: &str = "probe__get_code";
const PROBE_PAYLOAD: &str = "probe-payload-42";
const MEMORY_FACT: &str = "Onboarding lives on the PO dashboard";
const RECORDED_QUERY: &str = "po dashboard onboarding";
const REPLAYED_BLOCK: &str = "BLOC-MEMOIRE-DU-JOURNAL";
const REPLAYED_CLOCK: &str = "1999-01-01 00:00 +00:00";

/// Ces tests rejouent en lenient : la conversation enregistrée porte un message
/// `turn-context` estampillé à la minute par `chrono::Local::now()`
/// (`agent.rs`, `moim::turn_context_message`), que le journal ne capture pas —
/// aucun intercepteur de cette tâche ne peut donc garantir l'égalité de hash
/// d'un rejeu qui traverse une minute. Ce que ces tests vérifient est ce que le
/// journal sert, pas la vérification de hash (couverte par `replay_provider_test`).
const LENIENT: bool = true;

/// Le prompt système par défaut ne rend jamais l'estampille d'horloge : le test
/// d'horloge passe par un override qui, lui, la rend.
const CLOCK_PLACEHOLDER: &str = "{{current_date_time}}";

/// Exécutions réelles de l'outil, tous tours confondus. Le serveur MCP tourne
/// dans le processus de test, donc le compteur est directement observable.
static TOOL_CALLS: AtomicUsize = AtomicUsize::new(0);
/// Armé pour la durée du rejeu : toute exécution réelle devient une panique.
static REPLAYING: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Default)]
struct ProbeServer;

#[tool_router]
impl ProbeServer {
    #[tool(description = "Get the code", annotations(read_only_hint = true))]
    fn get_code(&self) -> Result<CallToolResult, McpError> {
        TOOL_CALLS.fetch_add(1, Ordering::SeqCst);
        assert!(
            !REPLAYING.load(Ordering::SeqCst),
            "the replay executed the tool for real"
        );
        Ok(CallToolResult::success(vec![ContentBlock::text(
            PROBE_PAYLOAD,
        )]))
    }
}

#[tool_handler]
impl ServerHandler for ProbeServer {
    fn get_info(&self) -> ServerInfo {
        InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
            .with_protocol_version(ProtocolVersion::LATEST)
            .with_server_info(Implementation::new("probe", "1.0.0"))
            .with_instructions("Instrumented probe tool.")
    }
}

struct ProbeFixture {
    url: String,
    handle: JoinHandle<()>,
}

impl Drop for ProbeFixture {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

impl ProbeFixture {
    async fn new() -> Self {
        let service = StreamableHttpService::new(
            || Ok::<_, std::io::Error>(ProbeServer),
            LocalSessionManager::default().into(),
            StreamableHttpServerConfig::default(),
        );
        let router = axum::Router::new().nest_service("/mcp", service);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let handle = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Self { url, handle }
    }
}

/// Deux appels au premier tour (le premier demande l'outil), un seul ensuite.
/// Le rejeu ne s'en sert pas : il tourne sur le `ReplayProvider`.
struct FixtureProvider;

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
            Usage::new(Some(11), Some(22), Some(33)),
        );
        let message = if already_called {
            Message::assistant().with_text("done")
        } else {
            Message::assistant()
                .with_tool_request(TOOL_REQUEST_ID, Ok(CallToolRequestParams::new(TOOL)))
        };
        Ok(stream_from_single_message(message, usage))
    }

    fn get_name(&self) -> &str {
        "fixture-mock"
    }
}

/// Ce que le provider du rejeu a reçu : le prompt système du tour rejoué, où
/// doivent apparaître le bloc mémoire et l'horloge servis par le journal.
struct SpyProvider {
    inner: ReplayProvider,
    prompts: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Provider for SpyProvider {
    async fn stream(
        &self,
        model_config: &ModelConfig,
        system: &str,
        messages: &[Message],
        tools: &[Tool],
    ) -> Result<MessageStream, ProviderError> {
        self.prompts.lock().unwrap().push(system.to_string());
        self.inner
            .stream(model_config, system, messages, tools)
            .await
    }

    fn get_name(&self) -> &str {
        "replay-spy"
    }
}

/// La session enregistrée et de quoi la rejouer à l'identique. L'agent
/// d'enregistrement survit au tour : c'est lui qui journalise une approbation
/// par son vrai chemin (`handle_confirmation`).
struct Fixture {
    agent: Agent,
    data_dir: TempDir,
    working_dir: PathBuf,
    session_manager: Arc<SessionManager>,
    session_id: String,
    events: Vec<SessionEvent>,
}

impl Fixture {
    /// Connexion directe au SQLite de la fixture : les tests amputent ou
    /// falsifient le journal, ce que l'API publique n'expose pas.
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

    fn first_turn(&self) -> i64 {
        self.events
            .iter()
            .find(|event| event.kind == "llm_request")
            .expect("the fixture logged an llm call")
            .turn_seq
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

fn new_agent(session_manager: &Arc<SessionManager>, data_dir: &TempDir, mode: KajiMode) -> Agent {
    Agent::with_config(AgentConfig::new(
        Arc::clone(session_manager),
        Arc::new(PermissionManager::new(data_dir.path().join("config"))),
        None,
        mode,
        true,
        KajiPlatform::KajiCli,
    ))
}

/// Un tour enregistré par le vrai pipeline : l'outil instrumenté est appelé une
/// fois, la mémoire est amorcée pour qu'un `memory_block` soit journalisé.
/// L'appelant détient déjà le verrou d'environnement.
async fn record_fixture(probe: &ProbeFixture) -> Result<Fixture> {
    let mode = KajiMode::Auto;
    let mut seed = SessionMemory::load("seeding-session");
    seed.remember(MEMORY_FACT, &["dashboard"], None);
    drop(seed);

    let data_dir = tempfile::tempdir()?;
    let working_dir = data_dir.path().join("workspace");
    std::fs::create_dir_all(&working_dir)?;

    let session_manager = Arc::new(SessionManager::new(data_dir.path().join("data")));
    let agent = new_agent(&session_manager, &data_dir, mode);
    let session = session_manager
        .create_session(
            working_dir.clone(),
            "replay-intercept-test".to_string(),
            SessionType::Hidden,
            mode,
        )
        .await?;

    agent
        .update_provider(
            Arc::new(FixtureProvider),
            ModelConfig::new("mock-model"),
            &session.id,
        )
        .await?;
    agent
        .add_extension(
            ExtensionConfig::streamable_http("probe", &probe.url, "instrumented probe", 30_u64),
            &session.id,
        )
        .await?;

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

    let events = session_manager.session_events(&session.id).await?;
    Ok(Fixture {
        agent,
        data_dir,
        working_dir,
        session_manager,
        session_id: session.id,
        events,
    })
}

/// L'agent de rejeu que le CLI (Task 11) montera : session dérivée, provider de
/// rejeu, `IdGen` redérivé de la graine du journal, curseur et mode branchés.
async fn replay_agent(
    fixture: &Fixture,
    probe: &ProbeFixture,
    cursor: Arc<EventCursor>,
    lenient: bool,
    mode: KajiMode,
) -> Result<(Agent, String, Arc<ReplayPosition>, Arc<Mutex<Vec<String>>>)> {
    let session = fixture
        .session_manager
        .create_session(
            fixture.working_dir.clone(),
            format!("replay-of-{}", fixture.session_id),
            SessionType::Hidden,
            mode,
        )
        .await?;

    let provider = ReplayProvider::new(Arc::clone(&cursor), lenient);
    let position = provider.position();
    let prompts = Arc::new(Mutex::new(Vec::new()));
    let spy = Arc::new(SpyProvider {
        inner: provider,
        prompts: Arc::clone(&prompts),
    });

    let mut agent = new_agent(&fixture.session_manager, &fixture.data_dir, mode);
    agent.set_idgen(Arc::new(SessionIdGen::new(&cursor.log_meta.idgen_seed)));
    agent.set_replay_mode(ReplayMode::new(fixture.session_id.clone()));
    agent.set_replay_source(ReplaySource::new(cursor, Arc::clone(&position)));
    agent
        .update_provider(spy, ModelConfig::new("mock-model"), &session.id)
        .await?;
    agent
        .add_extension(
            ExtensionConfig::streamable_http("probe", &probe.url, "instrumented probe", 30_u64),
            &session.id,
        )
        .await?;

    Ok((agent, session.id, position, prompts))
}

/// Le tour rejoué, joué jusqu'au bout, puis la conversation qu'il a produite.
async fn replay_turn(agent: &Agent, session_id: &str, fixture: &Fixture) -> Result<String> {
    let before = TOOL_CALLS.load(Ordering::SeqCst);
    REPLAYING.store(true, Ordering::SeqCst);
    let replayed = drain(
        agent
            .reply(
                Message::user().with_text(RECORDED_QUERY),
                session_config(session_id),
                None,
            )
            .await?,
    )
    .await;
    REPLAYING.store(false, Ordering::SeqCst);
    replayed?;
    assert_eq!(
        TOOL_CALLS.load(Ordering::SeqCst),
        before,
        "the replay never executes the tool"
    );

    let session = fixture
        .session_manager
        .get_session(session_id, true)
        .await?;
    let conversation = session.conversation.expect("the replayed session has one");
    Ok(conversation
        .messages()
        .iter()
        .map(|message| format!("{message:?}"))
        .collect::<Vec<_>>()
        .join("\n"))
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

/// Le résultat d'outil du rejeu vient du journal, l'outil ne tourne pas.
async fn assert_tool_result_is_served_from_the_log(state_machine: Option<&str>) -> Result<()> {
    let label = format!("KAJI_STATE_MACHINE={state_machine:?}");
    let memory_dir = tempfile::tempdir()?;
    let _guard = env(state_machine, &memory_dir);

    let probe = ProbeFixture::new().await;
    let fixture = record_fixture(&probe).await?;
    let cursor = Arc::new(EventCursor::load(&fixture.session_manager, &fixture.session_id).await?);
    assert!(
        cursor.tool_results.contains_key(TOOL_REQUEST_ID),
        "{label}: the recorded turn logged its tool result"
    );

    let (agent, session_id, position, _) =
        replay_agent(&fixture, &probe, cursor, LENIENT, KajiMode::Auto).await?;
    position.begin_turn(fixture.first_turn());
    let replayed = replay_turn(&agent, &session_id, &fixture).await?;

    assert!(
        replayed.contains(PROBE_PAYLOAD),
        "{label}: the logged tool result is served to the model: {replayed}"
    );
    assert!(
        replayed.contains("done"),
        "{label}: the replayed turn ran to its recorded end: {replayed}"
    );
    Ok(())
}

/// Un `tool_result` absent du journal est une erreur nommée, jamais une
/// exécution de rattrapage.
async fn assert_missing_tool_result_is_refused(state_machine: Option<&str>) -> Result<()> {
    let label = format!("KAJI_STATE_MACHINE={state_machine:?}");
    let memory_dir = tempfile::tempdir()?;
    let _guard = env(state_machine, &memory_dir);

    let probe = ProbeFixture::new().await;
    let fixture = record_fixture(&probe).await?;

    let pool = fixture.raw_pool().await?;
    sqlx::query("DELETE FROM session_events WHERE session_id = ? AND kind = 'tool_result'")
        .bind(&fixture.session_id)
        .execute(&pool)
        .await?;
    pool.close().await;

    let cursor = Arc::new(EventCursor::load(&fixture.session_manager, &fixture.session_id).await?);
    assert!(
        cursor.tool_results.is_empty(),
        "{label}: the amputated log has no tool result left"
    );

    let (agent, session_id, position, _) =
        replay_agent(&fixture, &probe, cursor, LENIENT, KajiMode::Auto).await?;
    position.begin_turn(fixture.first_turn());
    let replayed = replay_turn(&agent, &session_id, &fixture).await?;

    assert!(
        replayed.contains(TOOL_REQUEST_ID) && replayed.contains("tool_result absent"),
        "{label}: the missing key is named, not worked around: {replayed}"
    );
    assert!(
        !replayed.contains(PROBE_PAYLOAD),
        "{label}: nothing ran to produce a result: {replayed}"
    );
    Ok(())
}

/// Bloc mémoire et horloge du prompt rejoué viennent du journal : le test les y
/// falsifie, donc seul un prompt qui les lit peut les porter.
async fn assert_memory_and_clock_come_from_the_log(state_machine: Option<&str>) -> Result<()> {
    let label = format!("KAJI_STATE_MACHINE={state_machine:?}");
    let memory_dir = tempfile::tempdir()?;
    let _guard = env(state_machine, &memory_dir);

    let probe = ProbeFixture::new().await;
    let fixture = record_fixture(&probe).await?;
    let turn = fixture.first_turn();

    let pool = fixture.raw_pool().await?;
    sqlx::query(
        "UPDATE session_events SET payload_json = ? WHERE session_id = ? AND kind = 'memory_block'",
    )
    .bind(json!({ "turn_seq": turn, "block": REPLAYED_BLOCK }).to_string())
    .bind(&fixture.session_id)
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE session_events SET payload_json = ? WHERE session_id = ? AND kind = 'clock_reads'",
    )
    .bind(json!({ "turn_seq": turn, "reads": [REPLAYED_CLOCK] }).to_string())
    .bind(&fixture.session_id)
    .execute(&pool)
    .await?;
    pool.close().await;

    let cursor = Arc::new(EventCursor::load(&fixture.session_manager, &fixture.session_id).await?);

    // Journal falsifié ⇒ les hashes de requête ne correspondent plus : le rejeu
    // lenient sert quand même, ce que ce test veut observer.
    let (agent, session_id, position, prompts) =
        replay_agent(&fixture, &probe, cursor, LENIENT, KajiMode::Auto).await?;
    // Le prompt système par défaut ne rend pas `{{current_date_time}}` : sans
    // override, l'estampille servie ne serait observable nulle part.
    agent
        .override_system_prompt(format!("<replay-clock>{CLOCK_PLACEHOLDER}</replay-clock>"))
        .await;
    position.begin_turn(turn);
    replay_turn(&agent, &session_id, &fixture).await?;

    let prompts = prompts.lock().unwrap().clone();
    let first = prompts
        .first()
        .unwrap_or_else(|| panic!("{label}: the replay called the provider"));
    assert!(
        first.contains(REPLAYED_BLOCK),
        "{label}: the memory block comes from the log, not from a fresh recall: {first}"
    );
    assert!(
        first.contains(REPLAYED_CLOCK),
        "{label}: the prompt carries the recorded clock read: {first}"
    );
    assert!(
        !first.contains(MEMORY_FACT),
        "{label}: the live memory store is not consulted at all: {first}"
    );
    Ok(())
}

/// Une approbation enregistrée est rejouée depuis le journal : le rejeu tourne
/// en mode `Approve`, personne ne répond à sa demande de confirmation, et le
/// tour va quand même jusqu'au résultat d'outil enregistré.
async fn assert_approval_is_replayed_from_the_log(state_machine: Option<&str>) -> Result<()> {
    let label = format!("KAJI_STATE_MACHINE={state_machine:?}");
    let memory_dir = tempfile::tempdir()?;
    let _guard = env(state_machine, &memory_dir);

    let probe = ProbeFixture::new().await;
    let fixture = record_fixture(&probe).await?;
    let turn = fixture.first_turn();
    // La row `approval` par son vrai chemin d'écriture : `handle_confirmation`
    // la journalise pour le tour que l'agent vient de jouer.
    fixture
        .agent
        .handle_confirmation(
            TOOL_REQUEST_ID.to_string(),
            PermissionConfirmation {
                principal_type: PrincipalType::Tool,
                permission: Permission::AllowOnce,
            },
        )
        .await;

    let cursor = Arc::new(EventCursor::load(&fixture.session_manager, &fixture.session_id).await?);
    assert_eq!(
        cursor.approvals.get(&(turn, TOOL_REQUEST_ID.to_string())),
        Some(&true),
        "{label}: the recorded approval is indexed by turn and request: {:?}",
        cursor.approvals
    );

    let (agent, session_id, position, _) =
        replay_agent(&fixture, &probe, cursor, LENIENT, KajiMode::Approve).await?;
    position.begin_turn(turn);
    let replayed = replay_turn(&agent, &session_id, &fixture).await?;

    assert!(
        replayed.contains(PROBE_PAYLOAD),
        "{label}: the approved tool's logged result is served: {replayed}"
    );
    Ok(())
}

#[tokio::test]
async fn tool_result_is_served_from_the_log_on_the_legacy_loop() -> Result<()> {
    assert_tool_result_is_served_from_the_log(None).await
}

#[tokio::test]
async fn tool_result_is_served_from_the_log_on_the_state_machine_loop() -> Result<()> {
    assert_tool_result_is_served_from_the_log(Some("1")).await
}

#[tokio::test]
async fn missing_tool_result_is_refused_on_the_legacy_loop() -> Result<()> {
    assert_missing_tool_result_is_refused(None).await
}

#[tokio::test]
async fn missing_tool_result_is_refused_on_the_state_machine_loop() -> Result<()> {
    assert_missing_tool_result_is_refused(Some("1")).await
}

#[tokio::test]
async fn memory_and_clock_come_from_the_log_on_the_legacy_loop() -> Result<()> {
    assert_memory_and_clock_come_from_the_log(None).await
}

#[tokio::test]
async fn memory_and_clock_come_from_the_log_on_the_state_machine_loop() -> Result<()> {
    assert_memory_and_clock_come_from_the_log(Some("1")).await
}

#[tokio::test]
async fn approval_is_replayed_from_the_log_on_the_legacy_loop() -> Result<()> {
    assert_approval_is_replayed_from_the_log(None).await
}

#[tokio::test]
async fn approval_is_replayed_from_the_log_on_the_state_machine_loop() -> Result<()> {
    assert_approval_is_replayed_from_the_log(Some("1")).await
}
