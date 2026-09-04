//! Le test doré du rejeu : une session de trois tours est enregistrée par le
//! vrai pipeline, puis rejouée deux fois par l'API — les deux rejeux doivent
//! rendre la même transcription structurée, jusqu'aux ids de messages.
//!
//! Trois propriétés y sont clouées, chacune sous les deux boucles agent :
//! le déterminisme du rejeu (§ doré), la parité d'enregistrement entre les
//! deux boucles (§ parité), et l'hermétisme du bloc mémoire — un fait ajouté
//! après l'enregistrement ne déplace pas le rejeu, parce que le bloc vient du
//! journal et non d'un rappel frais.
//!
//! Le rejeu tourne en **strict** : toute divergence de hash de requête
//! arrêterait le tour. C'est le point du test doré — il vérifie de bout en
//! bout, à travers la boucle réelle, ce que `replay_provider_test` ne vérifie
//! qu'au niveau du provider.

use anyhow::Result;
use async_trait::async_trait;
use futures::StreamExt;
use kaji::agents::{Agent, AgentConfig, AgentEvent, ExtensionConfig, KajiPlatform, SessionConfig};
use kaji::config::permission::PermissionManager;
use kaji::config::KajiMode;
use kaji::conversation::message::{Message, MessageContent};
use kaji::kaji::SessionMemory;
use kaji::providers::base::{stream_from_single_message, MessageStream, Provider};
use kaji::replay::cursor::EventCursor;
use kaji::replay::idgen::SessionIdGen;
use kaji::replay::mode::ReplayMode;
use kaji::replay::provider::ReplayProvider;
use kaji::replay::source::ReplaySource;
use kaji::session::session_manager::{SessionEvent, SessionType, DB_NAME, SESSIONS_FOLDER};
use kaji::session::SessionManager;
use kaji_providers::conversation::token_usage::{ProviderUsage, Usage};
use kaji_providers::errors::ProviderError;
use kaji_providers::model::ModelConfig;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ContentBlock, Implementation, InitializeResult,
    ProtocolVersion, Role, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
};
use rmcp::{tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler};
use serde_json::Value;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tempfile::TempDir;
use tokio::task::JoinHandle;

const TOOL: &str = "probe__get_code";
const TOOL_REQUEST_ID: &str = "golden-probe-1";
/// Deux appels d'outil dans une même réponse : le second n'est pas porté par le
/// message de la réponse, et la boucle legacy lui fabrique son propre message.
/// Un seul appel par tour ne passerait jamais par ce chemin-là.
const TOOL_REQUEST_ID_2: &str = "golden-probe-2";
const PROBE_PAYLOAD: &str = "golden-payload-42";

/// Les instructions d'une extension frontend. Elles entrent dans le prompt
/// système à l'enregistrement ; `kaji replay` ne charge aucune extension, donc
/// seul le journal peut les rendre au rejeu.
const FRONTEND_INSTRUCTIONS: &str = "golden frontend instructions marker";

/// Le fichier de hints du working dir, écrit avant l'enregistrement puis
/// réécrit après pour prouver que le rejeu ne relit pas le disque.
const AGENTS_MD: &str = "AGENTS.md";
const RECORDED_HINT: &str = "golden hint as recorded";
const EDITED_HINT: &str = "golden hint edited after the recording";

/// Les faits mémoire de l'enregistrement, et celui qu'on ajoute *après* pour
/// prouver que le rejeu ne consulte pas le magasin vivant.
const SEEDED_FACTS: [(&str, &str); 2] = [
    ("Onboarding lives on the PO dashboard", "dashboard"),
    ("The dashboard release train leaves on Thursday", "release"),
];
/// Recouvre volontairement les termes des requêtes enregistrées : un rappel
/// frais le ferait remonter dans le top-k, donc dans le prompt système.
const LATE_FACT: &str = "po dashboard onboarding moved to the ops console after the recording";

const QUERIES: [&str; 3] = [
    "po dashboard onboarding",
    "what else does the dashboard carry",
    "wrap up the dashboard review",
];

/// Au-dessus du seuil `MIN_CONTEXT_FOR_MOIM` (32 k, `agents/moim.rs`), donc
/// chaque tour porte son bloc `turn-context`. Ce bloc entre dans la requête
/// hachée alors qu'il est fait d'état vivant — horloge à la minute, usage
/// cumulé de la session, budget de tours : le rejeu ne peut le retrouver que
/// s'il est journalisé. C'est la limite d'un modèle réel, et le doré la prend.
const CONTEXT_LIMIT: usize = 120_000;

/// Le rejeu doré est strict : une requête reconstruite qui ne retrouve pas son
/// hash enregistré arrête le tour.
const LENIENT: bool = false;

/// Exécutions réelles de l'outil, tous tours confondus.
static TOOL_CALLS: AtomicUsize = AtomicUsize::new(0);
/// Armé pour la durée d'un rejeu : toute exécution réelle devient une panique.
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

/// Le premier tour demande l'outil puis conclut ; les suivants concluent
/// directement, la réponse d'outil du premier tour restant dans l'historique.
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
                .with_tool_request(TOOL_REQUEST_ID_2, Ok(CallToolRequestParams::new(TOOL)))
        };
        Ok(stream_from_single_message(message, usage))
    }

    fn get_name(&self) -> &str {
        "fixture-mock"
    }
}

/// Ce que le provider du rejeu a reçu, pour observer le prompt système servi.
struct SpyProvider {
    inner: ReplayProvider,
    prompts: Arc<Mutex<Vec<String>>>,
    messages: Arc<Mutex<Vec<String>>>,
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
        self.messages
            .lock()
            .unwrap()
            .push(messages.iter().map(Message::as_concat_text).collect());
        self.inner
            .stream(model_config, system, messages, tools)
            .await
    }

    fn get_name(&self) -> &str {
        "replay-spy"
    }
}

struct Fixture {
    data_dir: TempDir,
    working_dir: PathBuf,
    session_manager: Arc<SessionManager>,
    session_id: String,
    events: Vec<SessionEvent>,
    /// Ce que l'enregistrement lui-même a rendu, tour par tour : la référence
    /// contre laquelle le rejeu se compare (spec S1 — les ids du journal sont
    /// ceux que le rejeu redérive, pas seulement ceux qu'il reproduit d'un
    /// rejeu à l'autre).
    transcript: Transcript,
}

/// Une ligne de transcription : le tour du journal, la nature du contenu, la
/// clé qui l'adresse (id du message porteur, ou id de corrélation d'outil) et
/// le contenu sérialisé. Deux rejeux du même journal doivent en produire la
/// même suite, à l'octet près.
#[derive(Debug, PartialEq, Eq)]
struct Line {
    turn: i64,
    kind: &'static str,
    key: String,
    content: String,
}

#[derive(Debug, Default)]
struct Transcript {
    lines: Vec<Line>,
    message_ids: Vec<String>,
}

impl Transcript {
    fn push(&mut self, turn: i64, message: &Message) {
        let message_id = message.id.clone().unwrap_or_default();
        self.message_ids.push(message_id.clone());
        for block in &message.content {
            let (kind, key) = match block {
                MessageContent::Text(_) => ("text", message_id.clone()),
                MessageContent::ToolRequest(request) => ("tool_request", request.id.clone()),
                MessageContent::ToolResponse(response) => ("tool_response", response.id.clone()),
                _ => ("other", message_id.clone()),
            };
            self.lines.push(Line {
                turn,
                kind,
                key,
                content: serde_json::to_string(block).unwrap_or_default(),
            });
        }
    }

    fn rendered(&self) -> String {
        self.lines
            .iter()
            .map(|line| format!("{}|{}|{}|{}", line.turn, line.kind, line.key, line.content))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Arme la sécurité « l'outil ne doit pas tourner » et la désarme même quand
/// le rejeu échoue en cours de route.
struct ReplayingGuard;

impl ReplayingGuard {
    fn arm() -> Self {
        REPLAYING.store(true, Ordering::SeqCst);
        Self
    }
}

impl Drop for ReplayingGuard {
    fn drop(&mut self) {
        REPLAYING.store(false, Ordering::SeqCst);
    }
}

fn model_config() -> ModelConfig {
    ModelConfig::new("mock-model").with_context_limit(Some(CONTEXT_LIMIT))
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

fn env<'a>(state_machine: Option<&'a str>, memory_dir: &'a TempDir) -> env_lock::EnvGuard<'a> {
    env_lock::lock_env([
        ("KAJI_STATE_MACHINE", state_machine),
        (
            "KAJI_MEMORY_DIR",
            Some(memory_dir.path().to_str().expect("utf8 temp path")),
        ),
    ])
}

fn seed_memory() {
    let mut memory = SessionMemory::load("golden-seeding-session");
    for (fact, entity) in SEEDED_FACTS {
        memory.remember(fact, &[entity], None);
    }
}

async fn drain(agent: &Agent, session_id: &str, message: Message) -> Result<Vec<Message>> {
    let stream = agent
        .reply(message, session_config(session_id), None)
        .await?;
    tokio::pin!(stream);
    let mut messages = Vec::new();
    while let Some(event) = stream.next().await {
        if let AgentEvent::Message(message) = event? {
            messages.push(message);
        }
    }
    Ok(messages)
}

/// Enregistre la session synthétique : trois tours, un appel d'outil au
/// premier, des faits mémoire dans le prompt de chacun. L'appelant détient
/// déjà le verrou d'environnement.
async fn record_fixture(probe: &ProbeFixture) -> Result<Fixture> {
    seed_memory();

    let data_dir = tempfile::tempdir()?;
    let working_dir = data_dir.path().join("workspace");
    std::fs::create_dir_all(&working_dir)?;
    std::fs::write(working_dir.join(AGENTS_MD), RECORDED_HINT)?;

    let session_manager = Arc::new(SessionManager::new(data_dir.path().join("data")));
    let agent = new_agent(&session_manager, &data_dir);
    let session = session_manager
        .create_session(
            working_dir.clone(),
            "replay-golden-test".to_string(),
            SessionType::Hidden,
            KajiMode::Auto,
        )
        .await?;

    agent
        .update_provider(Arc::new(FixtureProvider), model_config(), &session.id)
        .await?;
    agent
        .add_extension(
            ExtensionConfig::streamable_http("probe", &probe.url, "instrumented probe", 30_u64),
            &session.id,
        )
        .await?;
    agent
        .add_extension(
            ExtensionConfig::Frontend {
                name: "golden-frontend".to_string(),
                description: "frontend fixture".to_string(),
                tools: Vec::new(),
                instructions: Some(FRONTEND_INSTRUCTIONS.to_string()),
                bundled: None,
                available_tools: Vec::new(),
            },
            &session.id,
        )
        .await?;

    let mut transcript = Transcript::default();
    for (index, query) in QUERIES.iter().enumerate() {
        let turn_seq = index as i64 + 1;
        for message in drain(&agent, &session.id, Message::user().with_text(*query)).await? {
            transcript.push(turn_seq, &message);
        }
    }

    let events = session_manager.session_events(&session.id).await?;
    Ok(Fixture {
        data_dir,
        working_dir,
        session_manager,
        session_id: session.id,
        events,
        transcript,
    })
}

/// Les blocs `turn-context` que l'enregistrement a posés dans la conversation.
async fn recorded_turn_context_blocks(fixture: &Fixture) -> Result<Vec<String>> {
    let session = fixture
        .session_manager
        .get_session(&fixture.session_id, true)
        .await?;
    Ok(session
        .conversation
        .map(|conversation| {
            conversation
                .messages()
                .iter()
                .filter(|message| message.is_turn_context())
                .map(Message::as_concat_text)
                .collect()
        })
        .unwrap_or_default())
}

/// Le message qui a ouvert chaque tour, tel qu'enregistré — même extraction
/// que celle du CLI `kaji replay` (`commands/replay.rs`, `user_turns`).
fn recorded_turns(events: &[SessionEvent]) -> Vec<(i64, Message)> {
    let mut opened: HashSet<i64> = HashSet::new();
    let mut turns = Vec::new();
    for event in events {
        if event.kind != "message" {
            continue;
        }
        let Ok(message) = serde_json::from_str::<Message>(&event.payload_json) else {
            continue;
        };
        if !matches!(message.role, Role::User) || !opened.insert(event.turn_seq) {
            continue;
        }
        turns.push((event.turn_seq, message));
    }
    turns
}

/// La forme du journal v2 : chaque kind rejouable avec la clé sous laquelle le
/// rejeu l'interrogera. Le `tool_call_id` est épinglé par la fixture, donc la
/// comparaison entre boucles porte sur la clé exacte plutôt que sur sa seule
/// forme — strictement plus fort.
///
/// Triée, parce que l'ordre d'écriture dans le journal n'est pas une propriété
/// du format : la boucle legacy consomme le flux du provider paresseusement et
/// écrit son `llm_response` après le `tool_result` qu'il a déclenché, la
/// machine à états draine le flux d'abord. Le rejeu, lui, n'adresse le journal
/// que par clé (`cursor.rs`, S3) — jamais positionnellement. Ce qui doit être à
/// parité, ce sont donc les clés présentes, pas leur ordre d'arrivée.
fn v2_shape(events: &[SessionEvent]) -> Vec<String> {
    let mut shape: Vec<String> = events
        .iter()
        .filter_map(|event| {
            let payload: Value = serde_json::from_str(&event.payload_json).ok()?;
            let key = match event.kind.as_str() {
                "llm_request" | "llm_response" => format!("call_idx={}", payload["call_idx"]),
                "tool_result" => format!("tool_call_id={}", payload["tool_call_id"]),
                "memory_block" | "turn_context" | "clock_reads" | "condense_triggered"
                | "tool_manifest" | "condense_summary" => "-".to_string(),
                _ => return None,
            };
            Some(format!("turn={} {} {key}", event.turn_seq, event.kind))
        })
        .collect();
    shape.sort();
    // Les kinds d'assemblage (`memory_block`, `turn_context`) sont adressés par
    // appel : la machine à états réassemble avant chaque appel du tour quand la
    // boucle legacy n'assemble qu'une fois. La multiplicité n'est donc pas une
    // propriété du format — la présence l'est, et c'est ce qui doit être à
    // parité.
    shape.dedup();
    shape
}

/// Rejoue la session enregistrée de bout en bout, tour par tour, comme le fait
/// le CLI : session dérivée, `IdGen` redérivé de la graine du journal, curseur
/// et mode branchés — et **aucune extension rechargée**, exactement comme
/// `kaji replay`. La liste d'outils et les fragments de prompt qui en dérivent
/// viennent du journal (`tool_manifest`), pas d'un serveur MCP relancé.
/// Ce qu'un rejeu laisse derrière lui : la transcription qu'il a rendue, les
/// prompts et messages présentés au provider, et les ids de la conversation
/// persistée — c'est là qu'atterrissent les messages que la boucle n'émet pas
/// (le porteur des appels d'outil surnuméraires, par exemple).
struct Replayed {
    transcript: Transcript,
    prompts: Vec<String>,
    seen_messages: Vec<String>,
    conversation_ids: Vec<String>,
}

async fn replay_once(fixture: &Fixture, label: &str) -> Result<Replayed> {
    replay_once_with(fixture, label, LENIENT).await
}

async fn replay_once_with(fixture: &Fixture, label: &str, lenient: bool) -> Result<Replayed> {
    let cursor = Arc::new(EventCursor::load(&fixture.session_manager, &fixture.session_id).await?);
    let provider = ReplayProvider::new(Arc::clone(&cursor), lenient);
    let position = provider.position();
    let divergences = provider.divergences();
    let prompts = Arc::new(Mutex::new(Vec::new()));
    let seen_messages = Arc::new(Mutex::new(Vec::new()));
    let spy = Arc::new(SpyProvider {
        inner: provider,
        prompts: Arc::clone(&prompts),
        messages: Arc::clone(&seen_messages),
    });

    let derived = fixture
        .session_manager
        .create_session(
            fixture.working_dir.clone(),
            format!("replay-of-{}", fixture.session_id),
            SessionType::Hidden,
            KajiMode::Auto,
        )
        .await?;

    let mut agent = new_agent(&fixture.session_manager, &fixture.data_dir);
    agent.set_idgen(Arc::new(SessionIdGen::new(&cursor.log_meta.idgen_seed)));
    agent.set_replay_mode(ReplayMode::new(fixture.session_id.clone()));
    agent.set_replay_source(ReplaySource::new(
        Arc::clone(&cursor),
        Arc::clone(&position),
    ));
    agent
        .update_provider(spy, model_config(), &derived.id)
        .await?;

    let executions_before = TOOL_CALLS.load(Ordering::SeqCst);
    let mut transcript = Transcript::default();
    {
        let _armed = ReplayingGuard::arm();
        for (turn_seq, user_message) in recorded_turns(&fixture.events) {
            position.begin_turn(turn_seq);
            for message in drain(&agent, &derived.id, user_message).await? {
                transcript.push(turn_seq, &message);
            }
        }
    }

    assert_eq!(
        TOOL_CALLS.load(Ordering::SeqCst),
        executions_before,
        "{label}: le rejeu n'exécute jamais l'outil"
    );
    // Les deux boucles rattrapent les erreurs du provider et les rendent en
    // message : sans ce contrôle, un rejeu entièrement divergent produirait
    // deux transcriptions identiques et le doré ne verrait rien.
    assert!(
        lenient || !transcript.rendered().contains("replay:"),
        "{label}: aucun tour n'a échoué faute de journal : {}",
        transcript.rendered()
    );
    assert!(
        lenient || divergences.drain().is_empty(),
        "{label}: un rejeu strict ne tolère aucune divergence"
    );

    let prompts = prompts.lock().unwrap().clone();
    let seen_messages = seen_messages.lock().unwrap().clone();
    let conversation_ids = conversation_ids(&fixture.session_manager, &derived.id).await?;
    Ok(Replayed {
        transcript,
        prompts,
        seen_messages,
        conversation_ids,
    })
}

/// Les ids des messages de la conversation persistée, dans l'ordre.
async fn conversation_ids(
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
                .map(|message| message.id.clone().unwrap_or_default())
                .collect()
        })
        .unwrap_or_default())
}

/// Une session de trois tours, rejouée deux fois, rend deux fois la même
/// transcription — mêmes contenus, mêmes clés, mêmes ids de messages.
async fn assert_two_replays_agree(state_machine: Option<&str>) -> Result<()> {
    let label = format!("KAJI_STATE_MACHINE={state_machine:?}");
    let memory_dir = tempfile::tempdir()?;
    let _guard = env(state_machine, &memory_dir);

    let probe = ProbeFixture::new().await;
    let fixture = record_fixture(&probe).await?;

    let cursor = EventCursor::load(&fixture.session_manager, &fixture.session_id).await?;
    assert_eq!(
        recorded_turns(&fixture.events).len(),
        QUERIES.len(),
        "{label}: les trois tours sont enregistrés"
    );
    assert!(
        cursor.tool_results.contains_key(TOOL_REQUEST_ID)
            && cursor.tool_results.contains_key(TOOL_REQUEST_ID_2),
        "{label}: les deux appels d'outil du premier tour sont journalisés"
    );
    assert!(
        !cursor.memory_blocks.is_empty(),
        "{label}: les faits mémoire sont entrés dans le prompt et sont journalisés"
    );
    assert!(
        cursor
            .tool_manifests
            .values()
            .any(|manifest| manifest.tools.iter().any(|tool| tool.name == TOOL)),
        "{label}: l'outil de l'extension est journalisé — sans quoi le rejeu sans extension \
         ne prouverait rien"
    );

    let recorded_blocks = recorded_turn_context_blocks(&fixture).await?;
    assert!(
        !recorded_blocks.is_empty(),
        "{label}: au-dessus du seuil moim, chaque tour porte son bloc turn-context"
    );
    assert!(
        recorded_blocks.iter().all(|block| cursor
            .turn_contexts
            .values()
            .any(|journaled| journaled == block)),
        "{label}: chaque bloc turn-context de l'enregistrement est journalisé, \
         adressé par (tour, appel) : {recorded_blocks:?} vs {:?}",
        cursor.turn_contexts
    );

    let first = replay_once(&fixture, &label).await?;
    let second = replay_once(&fixture, &label).await?;

    assert!(
        !first.transcript.lines.is_empty(),
        "{label}: le rejeu a produit une transcription"
    );
    assert_eq!(
        first.transcript.rendered(),
        second.transcript.rendered(),
        "{label}: deux rejeux du même journal rendent la même transcription"
    );
    assert_eq!(
        first.transcript.message_ids, second.transcript.message_ids,
        "{label}: l'IdGen seedé redonne les mêmes ids de messages"
    );
    assert_eq!(
        fixture.transcript.message_ids, first.transcript.message_ids,
        "{label}: les ids du rejeu sont ceux de l'enregistrement — sinon une \
         transcription rejouée ne se recoupe pas avec le journal source (spec S1)"
    );
    assert_eq!(
        fixture.transcript.rendered(),
        first.transcript.rendered(),
        "{label}: le rejeu rend la transcription de l'enregistrement"
    );
    assert!(
        first.transcript.message_ids.iter().all(|id| !id.is_empty()),
        "{label}: chaque message rejoué porte un id : {:?}",
        first.transcript.message_ids
    );
    assert!(
        first.transcript.rendered().contains(PROBE_PAYLOAD),
        "{label}: le résultat d'outil servi par le journal est dans la transcription"
    );

    let recorded_conversation =
        conversation_ids(&fixture.session_manager, &fixture.session_id).await?;
    assert!(
        !recorded_conversation.is_empty(),
        "{label}: l'enregistrement a persisté une conversation"
    );
    assert_eq!(
        recorded_conversation, first.conversation_ids,
        "{label}: la conversation rejouée porte les ids de l'enregistrement — c'est là \
         qu'atterrissent les messages que la boucle n'émet pas, dont le porteur du second \
         appel d'outil"
    );
    assert_eq!(
        first.conversation_ids, second.conversation_ids,
        "{label}: deux rejeux nomment la conversation de la même façon"
    );

    Ok(())
}

/// Les deux boucles enregistrent la même chose : même séquence de kinds v2,
/// mêmes clés d'adressage. Un kind capturé dans une seule des deux boucles
/// rendrait la session rejouable d'un côté et divergente de l'autre.
#[tokio::test]
async fn both_loops_record_the_same_v2_shape() -> Result<()> {
    let legacy_shape = {
        let memory_dir = tempfile::tempdir()?;
        let _guard = env(None, &memory_dir);
        let probe = ProbeFixture::new().await;
        v2_shape(&record_fixture(&probe).await?.events)
    };

    let state_machine_shape = {
        let memory_dir = tempfile::tempdir()?;
        let _guard = env(Some("1"), &memory_dir);
        let probe = ProbeFixture::new().await;
        v2_shape(&record_fixture(&probe).await?.events)
    };

    assert!(
        !legacy_shape.is_empty(),
        "la boucle legacy a journalisé des kinds v2"
    );
    assert_eq!(
        legacy_shape, state_machine_shape,
        "les deux boucles journalisent les mêmes kinds v2 sous les mêmes clés"
    );

    Ok(())
}

/// Le fait mémoire ajouté *après* l'enregistrement : un rappel frais le
/// ramènerait dans le prompt système, changerait le hash de requête et
/// arrêterait le rejeu strict. Le bloc servi depuis le journal l'ignore, donc
/// la transcription ne bouge pas.
async fn assert_a_late_memory_fact_does_not_move_the_replay(
    state_machine: Option<&str>,
) -> Result<()> {
    let label = format!("KAJI_STATE_MACHINE={state_machine:?}");
    let memory_dir = tempfile::tempdir()?;
    let _guard = env(state_machine, &memory_dir);

    let probe = ProbeFixture::new().await;
    let fixture = record_fixture(&probe).await?;
    let before = replay_once(&fixture, &label).await?.transcript;

    let mut memory = SessionMemory::load("golden-late-session");
    memory.remember(LATE_FACT, &["po", "dashboard", "onboarding"], None);
    drop(memory);
    let fresh_recall = SessionMemory::load("golden-late-session");
    assert!(
        QUERIES.iter().any(|query| fresh_recall
            .recall_prompt(query, 3)
            .is_some_and(|block| block.contains(LATE_FACT))),
        "{label}: un rappel frais ramènerait bien le fait tardif — sinon le test ne prouve rien"
    );
    drop(fresh_recall);

    let replayed = replay_once(&fixture, &label).await?;
    let (after, prompts) = (replayed.transcript, replayed.prompts);

    assert_eq!(
        before.rendered(),
        after.rendered(),
        "{label}: le bloc mémoire vient du journal, pas du magasin vivant"
    );
    assert_eq!(
        before.message_ids, after.message_ids,
        "{label}: les ids de messages ne bougent pas non plus"
    );
    assert!(
        prompts.iter().all(|prompt| !prompt.contains(LATE_FACT)),
        "{label}: aucun prompt rejoué ne porte le fait ajouté après l'enregistrement"
    );

    Ok(())
}

/// Le bloc `turn-context` sert d'où il est lu : on réécrit celui du premier
/// appel dans le journal, et le rejeu doit présenter ce texte-là au provider.
/// Une recomposition rendrait le bloc vivant, sans la marque. Le rejeu tourne
/// en lenient : le journal a été volontairement désaccordé de son hash.
const REWRITTEN_BLOCK: &str = "<turn-context>bloc réécrit dans le journal</turn-context>";

async fn assert_the_turn_context_comes_from_the_log(state_machine: Option<&str>) -> Result<()> {
    let label = format!("KAJI_STATE_MACHINE={state_machine:?}");
    let memory_dir = tempfile::tempdir()?;
    let _guard = env(state_machine, &memory_dir);

    let probe = ProbeFixture::new().await;
    let fixture = record_fixture(&probe).await?;
    rewrite_first_turn_context(&fixture).await?;

    let seen_messages = replay_once_with(&fixture, &label, true)
        .await?
        .seen_messages;

    assert!(
        seen_messages
            .iter()
            .any(|messages| messages.contains(REWRITTEN_BLOCK)),
        "{label}: le rejeu a servi le bloc du journal, pas un bloc recomposé : {seen_messages:?}"
    );
    Ok(())
}

/// Remplace le bloc journalisé du tout premier appel par une marque
/// reconnaissable, sans passer par la boucle.
async fn rewrite_first_turn_context(fixture: &Fixture) -> Result<()> {
    let db_path = fixture
        .data_dir
        .path()
        .join("data")
        .join(SESSIONS_FOLDER)
        .join(DB_NAME);
    let pool = sqlx::SqlitePool::connect_with(
        sqlx::sqlite::SqliteConnectOptions::new()
            .filename(db_path)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal),
    )
    .await?;
    let payload = serde_json::json!({
        "turn_seq": 1,
        "call_idx": 0,
        "block": REWRITTEN_BLOCK,
    })
    .to_string();
    let updated = sqlx::query(
        "UPDATE session_events SET payload_json = ? \
         WHERE session_id = ? AND kind = 'turn_context' AND turn_seq = 1",
    )
    .bind(&payload)
    .bind(&fixture.session_id)
    .execute(&pool)
    .await?
    .rows_affected();
    pool.close().await;
    assert!(
        updated > 0,
        "le journal porte un bloc turn-context à réécrire — sinon le test ne prouve rien"
    );
    Ok(())
}

#[tokio::test]
async fn the_turn_context_comes_from_the_log_on_the_legacy_loop() -> Result<()> {
    assert_the_turn_context_comes_from_the_log(None).await
}

#[tokio::test]
async fn the_turn_context_comes_from_the_log_on_the_state_machine_loop() -> Result<()> {
    assert_the_turn_context_comes_from_the_log(Some("1")).await
}

/// Les hints du working dir (`AGENTS.md`, `.kajihints`) entrent dans le prompt
/// système, donc dans la requête hachée, et ils sont lus du disque à chaque
/// build. Le cas d'usage n°1 du replay — `kaji replay <session d'hier>` après un
/// `git pull` qui a touché `AGENTS.md` — les fait donc diverger au tour 1 s'ils
/// ne viennent pas du journal.
async fn assert_edited_hints_do_not_move_the_replay(state_machine: Option<&str>) -> Result<()> {
    let label = format!("KAJI_STATE_MACHINE={state_machine:?}");
    let memory_dir = tempfile::tempdir()?;
    let _guard = env(state_machine, &memory_dir);

    let probe = ProbeFixture::new().await;
    let fixture = record_fixture(&probe).await?;
    let before = replay_once(&fixture, &label).await?;
    assert!(
        before
            .prompts
            .iter()
            .all(|prompt| prompt.contains(RECORDED_HINT)),
        "{label}: le hint enregistré est bien dans le prompt, sinon le test ne prouve rien"
    );

    std::fs::write(fixture.working_dir.join(AGENTS_MD), EDITED_HINT)?;

    let after = replay_once(&fixture, &label).await?;
    assert_eq!(
        before.transcript.rendered(),
        after.transcript.rendered(),
        "{label}: un hint réécrit après l'enregistrement ne déplace pas le rejeu"
    );
    assert!(
        after
            .prompts
            .iter()
            .all(|prompt| prompt.contains(RECORDED_HINT) && !prompt.contains(EDITED_HINT)),
        "{label}: le rejeu sert le bloc de hints du journal, pas celui du disque"
    );
    Ok(())
}

#[tokio::test]
async fn edited_hints_do_not_move_the_replay_on_the_legacy_loop() -> Result<()> {
    assert_edited_hints_do_not_move_the_replay(None).await
}

#[tokio::test]
async fn edited_hints_do_not_move_the_replay_on_the_state_machine_loop() -> Result<()> {
    assert_edited_hints_do_not_move_the_replay(Some("1")).await
}

/// Les instructions frontend entrent dans le prompt système, donc dans la
/// requête hachée. `kaji replay` ne charge aucune extension : elles ne peuvent
/// venir que du journal. La boucle legacy les lisait vivantes au moment du
/// build, hors manifeste.
async fn assert_the_frontend_instructions_come_from_the_log(
    state_machine: Option<&str>,
) -> Result<()> {
    let label = format!("KAJI_STATE_MACHINE={state_machine:?}");
    let memory_dir = tempfile::tempdir()?;
    let _guard = env(state_machine, &memory_dir);

    let probe = ProbeFixture::new().await;
    let fixture = record_fixture(&probe).await?;

    let prompts = replay_once(&fixture, &label).await?.prompts;
    assert!(
        !prompts.is_empty(),
        "{label}: le rejeu a présenté des prompts au provider"
    );
    assert!(
        prompts
            .iter()
            .all(|prompt| prompt.contains(FRONTEND_INSTRUCTIONS)),
        "{label}: le rejeu, sans extension chargée, sert les instructions frontend du journal"
    );
    Ok(())
}

#[tokio::test]
async fn the_frontend_instructions_come_from_the_log_on_the_legacy_loop() -> Result<()> {
    assert_the_frontend_instructions_come_from_the_log(None).await
}

#[tokio::test]
async fn the_frontend_instructions_come_from_the_log_on_the_state_machine_loop() -> Result<()> {
    assert_the_frontend_instructions_come_from_the_log(Some("1")).await
}

#[tokio::test]
async fn two_replays_agree_on_the_legacy_loop() -> Result<()> {
    assert_two_replays_agree(None).await
}

#[tokio::test]
async fn two_replays_agree_on_the_state_machine_loop() -> Result<()> {
    assert_two_replays_agree(Some("1")).await
}

#[tokio::test]
async fn a_late_memory_fact_does_not_move_the_replay_on_the_legacy_loop() -> Result<()> {
    assert_a_late_memory_fact_does_not_move_the_replay(None).await
}

#[tokio::test]
async fn a_late_memory_fact_does_not_move_the_replay_on_the_state_machine_loop() -> Result<()> {
    assert_a_late_memory_fact_does_not_move_the_replay(Some("1")).await
}
