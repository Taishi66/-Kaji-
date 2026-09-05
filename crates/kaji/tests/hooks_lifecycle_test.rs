//! Les hooks de cycle de vie, par leurs trois usages réels (spec S6) : un
//! `session_start` qui injecte du contexte, un `user_prompt_submit` qui réécrit
//! le prompt, un `pre_tool_use` qui bloque un outil sur un motif. Plus les deux
//! règles qui les rendent sûrs : le timeout (fail-open partout sauf
//! `pre_tool_use`) et le rejeu, qui sert ce que les hooks ont produit sans
//! jamais relancer une commande.
//!
//! Chaque cas tourne sur les deux boucles — la sortie du hook entre dans le
//! prompt au point où elles convergent (`Agent::reply`), donc une parité
//! rompue se verrait ici.

use anyhow::Result;
use async_trait::async_trait;
use futures::StreamExt;
use kaji::agents::{Agent, AgentConfig, AgentEvent, ExtensionConfig, KajiPlatform, SessionConfig};
use kaji::config::permission::PermissionManager;
use kaji::config::KajiMode;
use kaji::conversation::message::{Message, MessageContent};
use kaji::hooks::config::HookEntry;
use kaji::hooks::{HookEvent, HookManager};
use kaji::providers::base::{stream_from_single_message, MessageStream, Provider};
use kaji::replay::cursor::EventCursor;
use kaji::replay::idgen::SessionIdGen;
use kaji::replay::mode::ReplayMode;
use kaji::replay::plan::{replay_plan, PlannedTurn};
use kaji::replay::provider::ReplayProvider;
use kaji::replay::source::ReplaySource;
use kaji::session::session_manager::{SessionEvent, SessionType};
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
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tempfile::TempDir;
use tokio::task::JoinHandle;

const TOOL: &str = "probe__get_code";
const TOOL_REQUEST_ID: &str = "probe-1";
const PROBE_PAYLOAD: &str = "probe-payload-42";
const PROMPT: &str = "quel est le code";
const TOOL_PROMPT: &str = "appelle la sonde";
const SESSION_CONTEXT: &str = "CONTEXTE-DE-SESSION-SHOSOIN";
const PROMPT_PREFIX: &str = "CONTRAT-ADHD-REECRIT";
const STOP_REASON: &str = "FINIS-LE-TEST-DABORD";

/// Exécutions réelles de l'outil : le rejeu ne doit en ajouter aucune.
static TOOL_CALLS: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Default)]
struct ProbeServer;

#[tool_router]
impl ProbeServer {
    #[tool(description = "Get the code", annotations(read_only_hint = true))]
    fn get_code(&self) -> Result<CallToolResult, McpError> {
        TOOL_CALLS.fetch_add(1, Ordering::SeqCst);
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

/// Répond `done` dès qu'un résultat d'outil est présent, demande l'outil quand
/// le prompt le réclame, répond `ok` sinon. Garde une copie des messages reçus :
/// c'est là qu'on lit ce que les hooks ont mis dans le prompt assemblé.
struct SpyProvider {
    seen: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Provider for SpyProvider {
    async fn stream(
        &self,
        _model_config: &ModelConfig,
        _system: &str,
        messages: &[Message],
        _tools: &[Tool],
    ) -> Result<MessageStream, ProviderError> {
        let rendered = messages
            .iter()
            .map(Message::as_concat_text)
            .collect::<Vec<_>>()
            .join("\n");
        self.seen.lock().unwrap().push(rendered.clone());

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
        } else if rendered.contains(TOOL_PROMPT) {
            Message::assistant()
                .with_tool_request(TOOL_REQUEST_ID, Ok(CallToolRequestParams::new(TOOL)))
        } else {
            Message::assistant().with_text("ok")
        };
        Ok(stream_from_single_message(message, usage))
    }

    fn get_name(&self) -> &str {
        "hooks-spy"
    }
}

/// Un script shell qui note son passage puis écrit sur stdout. Le fichier
/// témoin est ce qui prouve qu'un rejeu n'a rien lancé.
struct Script {
    dir: TempDir,
}

impl Script {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn witness(&self) -> PathBuf {
        self.dir.path().join("ran.log")
    }

    fn runs(&self) -> usize {
        std::fs::read_to_string(self.witness())
            .unwrap_or_default()
            .lines()
            .count()
    }

    /// Un hook qui réussit et écrit `output` sur stdout.
    fn emitting(&self, name: &str, output: &str) -> String {
        self.write(
            name,
            &format!(
                "#!/bin/sh\necho ran >> \"{witness}\"\nprintf '%s' '{output}'\nexit 0\n",
                witness = self.witness().display(),
            ),
        )
    }

    /// Un hook qui refuse, raison sur stderr, sortie non nulle mais pas 2 —
    /// c'est bien « exit ≠ 0 » que S6 demande de bloquer, pas le seul exit 2.
    fn denying(&self, name: &str, reason: &str) -> String {
        self.write(
            name,
            &format!(
                "#!/bin/sh\necho ran >> \"{witness}\"\necho '{reason}' >&2\nexit 7\n",
                witness = self.witness().display(),
            ),
        )
    }

    /// Un hook qui bloque ses `times` premières exécutions puis laisse passer —
    /// le `stop` réel, qui refuse une fin de tour tant qu'une condition n'est
    /// pas remplie. Exit 2 : le contrat historique des hooks bloquants.
    fn denying_first(&self, name: &str, reason: &str, times: usize) -> String {
        let counter = self.dir.path().join(format!("{name}.blocked"));
        self.write(
            name,
            &format!(
                "#!/bin/sh\necho ran >> \"{witness}\"\n\
                 blocked=$(cat \"{counter}\" 2>/dev/null || echo 0)\n\
                 if [ \"$blocked\" -ge {times} ]; then exit 0; fi\n\
                 echo $((blocked + 1)) > \"{counter}\"\n\
                 echo '{reason}' >&2\nexit 2\n",
                witness = self.witness().display(),
                counter = counter.display(),
            ),
        )
    }

    /// Un hook qui ne rend jamais la main à temps.
    fn hanging(&self, name: &str) -> String {
        self.write(
            name,
            &format!(
                "#!/bin/sh\necho ran >> \"{witness}\"\nsleep 30\n",
                witness = self.witness().display(),
            ),
        )
    }

    fn write(&self, name: &str, body: &str) -> String {
        let path = self.dir.path().join(name);
        std::fs::write(&path, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&path).unwrap().permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&path, permissions).unwrap();
        }
        format!("sh {}", path.display())
    }
}

fn entry(event: &str, command: String) -> HookEntry {
    HookEntry {
        event: event.to_string(),
        command,
        matcher: None,
        timeout_s: None,
    }
}

fn manager(entries: Vec<HookEntry>, root: &Path) -> HookManager {
    HookManager::from_entries(entries, root, "test")
}

struct Fixture {
    /// Gardé en vie le temps du test : c'est lui qui tient la session MCP.
    agent: Agent,
    data_dir: TempDir,
    working_dir: PathBuf,
    session_manager: Arc<SessionManager>,
    session_id: String,
    seen: Arc<Mutex<Vec<String>>>,
}

impl Fixture {
    /// Un tour de plus sur la même session. Les erreurs du flux remontent :
    /// un test qui joue une commande dont l'exécution peut échouer les ignore
    /// lui-même.
    async fn turn(&self, prompt: &str) -> Result<()> {
        self.turn_within(prompt, DEFAULT_MAX_TURNS).await
    }

    /// Un tour dont la borne est relevée : un `stop` qui refuse plusieurs fois
    /// consomme un tour par refus, et la borne par défaut les couperait.
    async fn turn_within(&self, prompt: &str, max_turns: u32) -> Result<()> {
        drain(
            self.agent
                .reply(
                    Message::user().with_text(prompt),
                    session_config_within(&self.session_id, max_turns),
                    None,
                )
                .await?,
        )
        .await?;
        Ok(())
    }

    async fn events(&self) -> Result<Vec<SessionEvent>> {
        self.session_manager.session_events(&self.session_id).await
    }

    /// Tout ce que le provider a lu, tous appels confondus — l'agent en passe
    /// d'autres par le même provider (nommage de session, rappel mémoire).
    fn prompts(&self) -> String {
        self.seen.lock().unwrap().join("\n---\n")
    }

    /// Le seul appel qui porte le message d'ouverture du tour.
    fn turn_prompt(&self) -> String {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .find(|prompt| prompt.contains(PROMPT) || prompt.contains(TOOL_PROMPT))
            .cloned()
            .unwrap_or_default()
    }

    /// La conversation persistée : c'est là qu'atterrit le refus rendu au
    /// modèle, même quand le tour s'arrête juste après.
    async fn conversation_text(&self) -> Result<String> {
        let session = self
            .session_manager
            .get_session(&self.session_id, true)
            .await?;
        Ok(session
            .conversation
            .expect("la session a une conversation")
            .messages()
            .iter()
            .map(|message| format!("{message:?}"))
            .collect::<Vec<_>>()
            .join("\n"))
    }

    async fn logged_messages(&self) -> Result<String> {
        Ok(self
            .session_manager
            .session_events(&self.session_id)
            .await?
            .into_iter()
            .filter(|event| event.kind == "message")
            .map(|event| event.payload_json)
            .collect::<Vec<_>>()
            .join("\n"))
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

async fn drain(stream: impl futures::Stream<Item = Result<AgentEvent>>) -> Result<Vec<AgentEvent>> {
    tokio::pin!(stream);
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event?);
    }
    Ok(events)
}

const DEFAULT_MAX_TURNS: u32 = 3;

fn session_config(id: &str) -> SessionConfig {
    session_config_within(id, DEFAULT_MAX_TURNS)
}

fn session_config_within(id: &str, max_turns: u32) -> SessionConfig {
    SessionConfig {
        id: id.to_string(),
        schedule_id: None,
        max_turns: Some(max_turns),
        retry_config: None,
    }
}

/// Une session enregistrée avec `hooks` montés, un tour joué sur `prompt`.
async fn record(hooks: HookManager, prompt: &str, probe: Option<&ProbeFixture>) -> Result<Fixture> {
    record_within(hooks, prompt, probe, DEFAULT_MAX_TURNS).await
}

async fn record_within(
    hooks: HookManager,
    prompt: &str,
    probe: Option<&ProbeFixture>,
    max_turns: u32,
) -> Result<Fixture> {
    let fixture = fixture(hooks, SessionType::User, probe).await?;
    fixture.turn_within(prompt, max_turns).await?;
    Ok(fixture)
}

/// La session et son agent, hooks montés, sans aucun tour joué.
async fn fixture(
    hooks: HookManager,
    session_type: SessionType,
    probe: Option<&ProbeFixture>,
) -> Result<Fixture> {
    let data_dir = tempfile::tempdir()?;
    let working_dir = data_dir.path().join("workspace");
    std::fs::create_dir_all(&working_dir)?;

    let session_manager = Arc::new(SessionManager::new(data_dir.path().join("data")));
    let mut agent = new_agent(&session_manager, &data_dir);
    agent.set_hook_manager(hooks);

    let session = session_manager
        .create_session(
            working_dir.clone(),
            "hooks-lifecycle-test".to_string(),
            session_type,
            KajiMode::Auto,
        )
        .await?;

    let seen = Arc::new(Mutex::new(Vec::new()));
    agent
        .update_provider(
            Arc::new(SpyProvider {
                seen: Arc::clone(&seen),
            }),
            ModelConfig::new("mock-model"),
            &session.id,
        )
        .await?;
    if let Some(probe) = probe {
        agent
            .add_extension(
                ExtensionConfig::streamable_http("probe", &probe.url, "instrumented probe", 30_u64),
                &session.id,
            )
            .await?;
    }

    Ok(Fixture {
        agent,
        data_dir,
        working_dir,
        session_manager,
        session_id: session.id,
        seen,
    })
}

/// Le rappel mémoire lit un dossier réel s'il n'est pas dérouté : chaque test
/// lui en donne un vide, sinon la mémoire de la machine entre dans le prompt.
fn env<'a>(state_machine: Option<&'a str>, memory_dir: &'a TempDir) -> env_lock::EnvGuard<'a> {
    env_lock::lock_env([
        ("KAJI_STATE_MACHINE", state_machine),
        (
            "KAJI_MEMORY_DIR",
            Some(memory_dir.path().to_str().expect("utf8 temp path")),
        ),
    ])
}

const LOOPS: [Option<&str>; 2] = [None, Some("1")];

// ---------------------------------------------------------------------------
// Les trois usages réels
// ---------------------------------------------------------------------------

#[tokio::test]
async fn session_start_injects_context_into_the_assembled_prompt() -> Result<()> {
    for state_machine in LOOPS {
        let label = format!("KAJI_STATE_MACHINE={state_machine:?}");
        let memory_dir = tempfile::tempdir()?;
        let _guard = env(state_machine, &memory_dir);

        let script = Script::new();
        let hooks = manager(
            vec![entry(
                "session_start",
                script.emitting("ctx.sh", SESSION_CONTEXT),
            )],
            script.dir.path(),
        );
        let fixture = record(hooks, PROMPT, None).await?;

        assert_eq!(script.runs(), 1, "{label}: le hook a tourné une fois");
        assert!(
            fixture.turn_prompt().contains(SESSION_CONTEXT),
            "{label}: le contexte est dans le prompt assemblé : {}",
            fixture.prompts()
        );
    }
    Ok(())
}

#[tokio::test]
async fn user_prompt_submit_rewrites_the_prompt_and_the_log_carries_it() -> Result<()> {
    for state_machine in LOOPS {
        let label = format!("KAJI_STATE_MACHINE={state_machine:?}");
        let memory_dir = tempfile::tempdir()?;
        let _guard = env(state_machine, &memory_dir);

        let script = Script::new();
        let hooks = manager(
            vec![entry(
                "user_prompt_submit",
                script.emitting("rewrite.sh", PROMPT_PREFIX),
            )],
            script.dir.path(),
        );
        let fixture = record(hooks, PROMPT, None).await?;

        let prompt = fixture.turn_prompt();
        assert!(
            prompt.contains(PROMPT_PREFIX) && prompt.contains(PROMPT),
            "{label}: le préfixe précède le prompt original : {prompt}"
        );
        assert!(
            prompt.find(PROMPT_PREFIX) < prompt.find(PROMPT),
            "{label}: c'est un préfixe, pas un suffixe : {prompt}"
        );

        let logged = fixture.logged_messages().await?;
        assert!(
            logged.contains(PROMPT_PREFIX),
            "{label}: le log `message` porte la version réécrite : {logged}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn pre_tool_use_blocks_the_matched_tool_and_lets_the_rest_through() -> Result<()> {
    for state_machine in LOOPS {
        let label = format!("KAJI_STATE_MACHINE={state_machine:?}");
        let memory_dir = tempfile::tempdir()?;
        let _guard = env(state_machine, &memory_dir);
        let probe = ProbeFixture::new().await;

        let blocking = Script::new();
        let mut blocked = entry(
            "pre_tool_use",
            blocking.denying("deny.sh", "sonde interdite ici"),
        );
        blocked.matcher = Some(TOOL.to_string());
        let before = TOOL_CALLS.load(Ordering::SeqCst);
        let fixture = record(
            manager(vec![blocked], blocking.dir.path()),
            TOOL_PROMPT,
            Some(&probe),
        )
        .await?;

        assert_eq!(blocking.runs(), 1, "{label}: le hook a examiné l'appel");
        assert_eq!(
            TOOL_CALLS.load(Ordering::SeqCst),
            before,
            "{label}: l'outil bloqué n'a pas tourné"
        );
        let conversation = fixture.conversation_text().await?;
        assert!(
            conversation.contains("sonde interdite ici"),
            "{label}: stderr est rendu au modèle : {conversation}"
        );

        let passing = Script::new();
        let mut other = entry("pre_tool_use", passing.denying("deny.sh", "un autre outil"));
        other.matcher = Some("un__autre_outil".to_string());
        let before = TOOL_CALLS.load(Ordering::SeqCst);
        let fixture = record(
            manager(vec![other], passing.dir.path()),
            TOOL_PROMPT,
            Some(&probe),
        )
        .await?;

        assert_eq!(
            passing.runs(),
            0,
            "{label}: un matcher qui ne colle pas ne lance rien"
        );
        assert_eq!(
            TOOL_CALLS.load(Ordering::SeqCst),
            before + 1,
            "{label}: le reste passe"
        );
        assert!(
            fixture.conversation_text().await?.contains(PROBE_PAYLOAD),
            "{label}: le résultat de l'outil est revenu"
        );
    }
    Ok(())
}

#[tokio::test]
async fn post_tool_use_feedback_reaches_the_model_with_the_tool_result() -> Result<()> {
    for state_machine in LOOPS {
        let label = format!("KAJI_STATE_MACHINE={state_machine:?}");
        let memory_dir = tempfile::tempdir()?;
        let _guard = env(state_machine, &memory_dir);
        let probe = ProbeFixture::new().await;

        let script = Script::new();
        let hooks = manager(
            vec![entry(
                "post_tool_use",
                script.emitting("feedback.sh", "RELIS-LE-DIFF"),
            )],
            script.dir.path(),
        );
        let fixture = record(hooks, TOOL_PROMPT, Some(&probe)).await?;

        assert_eq!(script.runs(), 1, "{label}: le hook a tourné après l'outil");
        let conversation = fixture.conversation_text().await?;
        assert!(
            conversation.contains("RELIS-LE-DIFF") && conversation.contains(PROBE_PAYLOAD),
            "{label}: le retour accompagne le résultat, il ne le remplace pas : {conversation}"
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Timeout : fail-open partout, fail-closed sur pre_tool_use
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_slow_context_hook_fails_open_and_a_slow_guard_fails_closed() -> Result<()> {
    for state_machine in LOOPS {
        let label = format!("KAJI_STATE_MACHINE={state_machine:?}");
        let memory_dir = tempfile::tempdir()?;
        let _guard = env(state_machine, &memory_dir);

        let slow = Script::new();
        let mut context = entry("session_start", slow.hanging("slow.sh"));
        context.timeout_s = Some(1);
        let fixture = record(manager(vec![context], slow.dir.path()), PROMPT, None).await?;
        assert!(
            fixture.turn_prompt().contains(PROMPT),
            "{label}: le tour est parti sans le contexte : {}",
            fixture.prompts()
        );

        let probe = ProbeFixture::new().await;
        let guard_script = Script::new();
        let mut guard = entry("pre_tool_use", guard_script.hanging("slow.sh"));
        guard.timeout_s = Some(1);
        guard.matcher = Some(TOOL.to_string());
        let before = TOOL_CALLS.load(Ordering::SeqCst);
        let fixture = record(
            manager(vec![guard], guard_script.dir.path()),
            TOOL_PROMPT,
            Some(&probe),
        )
        .await?;

        assert_eq!(
            TOOL_CALLS.load(Ordering::SeqCst),
            before,
            "{label}: un garde-fou muet bloque l'appel"
        );
        let conversation = fixture.conversation_text().await?;
        assert!(
            conversation.contains(kaji::hooks::TIMEOUT_DENIAL),
            "{label}: la raison du blocage est nommée : {conversation}"
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Rejeu : servi depuis le journal, jamais réexécuté
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_recorded_session_replays_on_a_machine_without_the_hooks() -> Result<()> {
    for state_machine in LOOPS {
        let label = format!("KAJI_STATE_MACHINE={state_machine:?}");
        let memory_dir = tempfile::tempdir()?;
        let _guard = env(state_machine, &memory_dir);

        let script = Script::new();
        let hooks = manager(
            vec![
                entry("session_start", script.emitting("ctx.sh", SESSION_CONTEXT)),
                entry(
                    "user_prompt_submit",
                    script.emitting("rewrite.sh", PROMPT_PREFIX),
                ),
            ],
            script.dir.path(),
        );
        let fixture = record(hooks, PROMPT, None).await?;
        let runs_at_record = script.runs();
        assert_eq!(runs_at_record, 2, "{label}: les deux hooks ont tourné");

        let cursor =
            Arc::new(EventCursor::load(&fixture.session_manager, &fixture.session_id).await?);
        assert!(
            !cursor.hook_outputs.is_empty(),
            "{label}: les sorties de hooks sont journalisées"
        );

        // La machine de rejeu n'a aucun hook monté — c'est le cas réel d'une
        // trace rejouée ailleurs.
        let replayed = fixture
            .session_manager
            .create_session(
                fixture.working_dir.clone(),
                format!("replay-of-{}", fixture.session_id),
                SessionType::Hidden,
                KajiMode::Auto,
            )
            .await?;

        let provider = ReplayProvider::new(Arc::clone(&cursor), true);
        let position = provider.position();
        let mut agent = new_agent(&fixture.session_manager, &fixture.data_dir);
        agent.set_idgen(Arc::new(SessionIdGen::new(&cursor.log_meta.idgen_seed)));
        agent.set_replay_mode(ReplayMode::new(fixture.session_id.clone(), KajiMode::Auto));
        agent.set_replay_source(ReplaySource::new(
            Arc::clone(&cursor),
            Arc::clone(&position),
        ));
        agent.set_hook_manager(HookManager::default());
        agent
            .update_provider(
                Arc::new(provider),
                ModelConfig::new("mock-model"),
                &replayed.id,
            )
            .await?;
        // Le rejeu passe par le plan de production (`kaji replay`,
        // `commands/replay.rs`) : il réinjecte le message du journal, déjà
        // réécrit par les hooks. Un harness qui repartirait du prompt brut
        // masquerait le double-préfixe que ce test existe pour interdire.
        let events = fixture.events().await?;
        let planned: Vec<(i64, Message)> = replay_plan(&events, None)
            .into_iter()
            .filter_map(|(turn_seq, planned)| match planned {
                PlannedTurn::Replay(message) => Some((turn_seq, message)),
                PlannedTurn::Skipped => None,
            })
            .collect();
        assert_eq!(planned.len(), 1, "{label}: un tour enregistré à rejouer");

        for (turn_seq, user_message) in planned {
            position.begin_turn(turn_seq);
            drain(
                agent
                    .reply(user_message, session_config(&replayed.id), None)
                    .await?,
            )
            .await?;
        }

        assert_eq!(
            script.runs(),
            runs_at_record,
            "{label}: le rejeu n'a lancé aucun hook"
        );

        let session = fixture
            .session_manager
            .get_session(&replayed.id, true)
            .await?;
        let rendered = session
            .conversation
            .expect("la session rejouée a une conversation")
            .messages()
            .iter()
            .map(Message::as_concat_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            rendered.contains(SESSION_CONTEXT) && rendered.contains(PROMPT_PREFIX),
            "{label}: le prompt rejoué porte ce que les hooks avaient produit : {rendered}"
        );
        assert_eq!(
            rendered.matches(SESSION_CONTEXT).count(),
            1,
            "{label}: le message journalisé est canonique — le prélude n'est pas réappliqué : {rendered}"
        );
        assert_eq!(
            rendered.matches(PROMPT_PREFIX).count(),
            1,
            "{label}: idem pour la réécriture de prompt : {rendered}"
        );

        drop(fixture);
    }
    Ok(())
}

/// Le hook `stop` qui a bloqué une fin de tour à l'enregistrement la bloque
/// aussi au rejeu : sa décision est journalisée sous `hook_output` et servie.
/// Sans ça le rejeu s'arrête un tour plus tôt, avec moins d'appels LLM et
/// aucune divergence signalée — la divergence silencieuse que la règle
/// « Replay v2 » interdit.
#[tokio::test]
async fn a_stop_hook_denial_is_journaled_and_served_at_replay() -> Result<()> {
    a_stop_hook_replays_its_denials(1).await
}

/// Deux refus puis un accord. Chaque décision est adressée par le nombre de
/// blocages qui la précèdent — `"0"`, `"1"`, puis `"2"` que le journal ne porte
/// pas. Un adressage cassé se verrait ici et nulle part ailleurs : servir la
/// même clé deux fois rendrait un refus de trop, la lire trop tôt en rendrait
/// un de moins, et dans les deux cas le compte d'appels LLM du rejeu diverge.
#[tokio::test]
async fn two_stop_hook_denials_replay_in_order_then_the_turn_ends() -> Result<()> {
    a_stop_hook_replays_its_denials(2).await
}

async fn a_stop_hook_replays_its_denials(denials: usize) -> Result<()> {
    for state_machine in LOOPS {
        let label = format!("KAJI_STATE_MACHINE={state_machine:?}, refus={denials}");
        let memory_dir = tempfile::tempdir()?;
        let _guard = env(state_machine, &memory_dir);

        let script = Script::new();
        let hooks = manager(
            vec![entry(
                "stop",
                script.denying_first("stop.sh", STOP_REASON, denials),
            )],
            script.dir.path(),
        );
        let max_turns = denials as u32 + 2;
        let fixture = record_within(hooks, PROMPT, None, max_turns).await?;
        let runs_at_record = script.runs();
        assert_eq!(
            runs_at_record,
            denials + 1,
            "{label}: un passage du hook par refus, puis celui qui laisse finir"
        );
        assert_eq!(
            fixture
                .conversation_text()
                .await?
                .matches(STOP_REASON)
                .count(),
            denials,
            "{label}: chaque refus est rendu au modèle à l'enregistrement"
        );

        let cursor =
            Arc::new(EventCursor::load(&fixture.session_manager, &fixture.session_id).await?);
        let replayed = fixture
            .session_manager
            .create_session(
                fixture.working_dir.clone(),
                format!("replay-of-{}", fixture.session_id),
                SessionType::User,
                KajiMode::Auto,
            )
            .await?;

        let provider = ReplayProvider::new(Arc::clone(&cursor), true);
        let position = provider.position();
        let mut agent = new_agent(&fixture.session_manager, &fixture.data_dir);
        agent.set_idgen(Arc::new(SessionIdGen::new(&cursor.log_meta.idgen_seed)));
        agent.set_replay_mode(ReplayMode::new(fixture.session_id.clone(), KajiMode::Auto));
        agent.set_replay_source(ReplaySource::new(
            Arc::clone(&cursor),
            Arc::clone(&position),
        ));
        agent.set_hook_manager(HookManager::default());
        agent
            .update_provider(
                Arc::new(provider),
                ModelConfig::new("mock-model"),
                &replayed.id,
            )
            .await?;

        for (turn_seq, planned) in replay_plan(&fixture.events().await?, None) {
            let PlannedTurn::Replay(user_message) = planned else {
                continue;
            };
            position.begin_turn(turn_seq);
            drain(
                agent
                    .reply(
                        user_message,
                        session_config_within(&replayed.id, max_turns),
                        None,
                    )
                    .await?,
            )
            .await?;
        }

        assert_eq!(
            script.runs(),
            runs_at_record,
            "{label}: le rejeu n'a lancé aucun hook"
        );
        let rendered = fixture
            .session_manager
            .get_session(&replayed.id, true)
            .await?
            .conversation
            .expect("la session rejouée a une conversation")
            .messages()
            .iter()
            .map(Message::as_concat_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            rendered.matches(STOP_REASON).count(),
            denials,
            "{label}: le rejeu rejoue chaque blocage, ni un de plus ni un de moins : {rendered}"
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Ce qui échappe aux hooks de prompt
// ---------------------------------------------------------------------------

/// Une commande connue n'est pas un prompt : `apply_prompt_hooks` la reconnaît
/// avant d'appeler quoi que ce soit, donc ni `session_start` ni
/// `user_prompt_submit` ne tournent — sinon le préfixe pousserait le `/` ou le
/// `!` hors de tête et la commande partirait au modèle comme de la prose.
/// `Please compact this conversation` est un des déclencheurs qui valent
/// `/compact` : il échappe aux hooks comme la commande qu'il devient.
#[tokio::test]
async fn a_known_command_and_a_bang_escape_the_prompt_hooks() -> Result<()> {
    for state_machine in LOOPS {
        let label = format!("KAJI_STATE_MACHINE={state_machine:?}");
        for command in ["/status", "!ls", "Please compact this conversation"] {
            let memory_dir = tempfile::tempdir()?;
            let _guard = env(state_machine, &memory_dir);

            let script = Script::new();
            let hooks = manager(
                vec![
                    entry("session_start", script.emitting("ctx.sh", SESSION_CONTEXT)),
                    entry(
                        "user_prompt_submit",
                        script.emitting("rewrite.sh", PROMPT_PREFIX),
                    ),
                ],
                script.dir.path(),
            );
            let fixture = fixture(hooks, SessionType::User, None).await?;
            // Le sort de la commande elle-même n'est pas le sujet : `!ls` peut
            // échouer faute d'outil shell monté, ce qui compte est qu'aucun
            // hook n'ait touché son texte.
            let _ = fixture.turn(command).await;

            assert_eq!(
                script.runs(),
                0,
                "{label}: `{command}` n'a lancé aucun hook de prompt"
            );
            let conversation = fixture.conversation_text().await?;
            assert!(
                !conversation.contains(SESSION_CONTEXT) && !conversation.contains(PROMPT_PREFIX),
                "{label}: `{command}` traverse intact : {conversation}"
            );
        }
    }
    Ok(())
}

/// La contrepartie : tout ce qui commence par `/` sans être une commande connue
/// part au modèle, donc doit voir les hooks. Un chemin absolu, un collage
/// multi-ligne, une commande qui n'existe pas : trois entrées que le préfixe
/// `starts_with('/')` capturait, et qui atteignaient le modèle sans le contrat
/// que `user_prompt_submit` est là pour injecter.
#[tokio::test]
async fn an_unknown_slash_and_a_paste_reach_the_prompt_hooks() -> Result<()> {
    for state_machine in LOOPS {
        let label = format!("KAJI_STATE_MACHINE={state_machine:?}");
        for prompt in [
            "/Users/moi/code/agent.rs regarde ça",
            "/commande-qui-nexiste-pas fais un truc",
            "/* bloc collé */\nligne deux du collage",
        ] {
            let memory_dir = tempfile::tempdir()?;
            let _guard = env(state_machine, &memory_dir);

            let script = Script::new();
            let hooks = manager(
                vec![entry(
                    "user_prompt_submit",
                    script.emitting("rewrite.sh", PROMPT_PREFIX),
                )],
                script.dir.path(),
            );
            let fixture = fixture(hooks, SessionType::User, None).await?;
            fixture.turn(prompt).await?;

            assert_eq!(
                script.runs(),
                1,
                "{label}: `{prompt}` est un prompt, le hook tourne"
            );
            assert!(
                fixture.prompts().contains(PROMPT_PREFIX),
                "{label}: `{prompt}` atteint le modèle avec le contrat injecté"
            );
        }
    }
    Ok(())
}

/// Les hooks de prompt appartiennent à la session que l'utilisateur ouvre. Un
/// sous-agent tourne dans une session neuve à chaque invocation : y tirer
/// `session_start` préfixerait le dump de contexte au prompt de tâche écrit par
/// l'orchestrateur, et `user_prompt_submit` réécrirait ce prompt à chaque
/// branche du fan-out.
#[tokio::test]
async fn prompt_hooks_stay_out_of_subagent_scheduled_and_hidden_sessions() -> Result<()> {
    for state_machine in LOOPS {
        let label = format!("KAJI_STATE_MACHINE={state_machine:?}");
        for session_type in [
            SessionType::SubAgent,
            SessionType::Scheduled,
            SessionType::Hidden,
        ] {
            let memory_dir = tempfile::tempdir()?;
            let _guard = env(state_machine, &memory_dir);

            let script = Script::new();
            let hooks = manager(
                vec![
                    entry("session_start", script.emitting("ctx.sh", SESSION_CONTEXT)),
                    entry(
                        "user_prompt_submit",
                        script.emitting("rewrite.sh", PROMPT_PREFIX),
                    ),
                ],
                script.dir.path(),
            );
            let fixture = fixture(hooks, session_type, None).await?;
            fixture.turn(PROMPT).await?;

            assert_eq!(
                script.runs(),
                0,
                "{label}, {session_type:?}: aucun hook de prompt hors des sessions user"
            );
            let seen = fixture.prompts();
            assert!(
                !seen.contains(SESSION_CONTEXT) && !seen.contains(PROMPT_PREFIX),
                "{label}, {session_type:?}: le prompt de tâche reste celui de l'appelant"
            );
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Portes fermées : env, machine de développement
// ---------------------------------------------------------------------------

/// Les hooks se déclarent par fichiers. `HOOKS=` en variable d'environnement
/// est la porte d'à côté du gate projet — un `.envrc`, un `docker-compose.yml`
/// ou un `Makefile` du dépôt suffirait à la poser.
#[test]
fn a_hooks_key_posted_in_the_environment_is_ignored() {
    let payload = r#"[{"event":"session_start","command":"touch /tmp/kaji-hook-should-not-run"}]"#;
    let _guard = env_lock::lock_env([("HOOKS", Some(payload))]);
    assert!(
        kaji::hooks::config::user_entries().is_empty(),
        "une clé HOOKS posée en env ne déclare aucun hook"
    );
}

/// `cargo test` ne monte pas les hooks de la machine : un `Agent` construit
/// dans une suite lit les plugins, jamais la config de hooks — sinon un
/// `post_tool_use` de l'utilisateur ferait tomber des assertions d'intégration
/// sans rapport.
#[test]
fn a_test_process_never_mounts_the_config_hooks() {
    let project = tempfile::tempdir().unwrap();
    let path = project.path().join(".kaji/hooks.yaml");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        &path,
        "hooks:\n  - event: after_file_edit\n    command: echo hi\n",
    )
    .unwrap();

    let _guard = env_lock::lock_env([("KAJI_PROJECT_HOOKS", Some("1"))]);
    let manager = HookManager::load(Some(project.path()), false);
    assert!(
        !manager.has_hooks(HookEvent::AfterFileEdit),
        "les hooks de config restent hors du processus de test"
    );
}
