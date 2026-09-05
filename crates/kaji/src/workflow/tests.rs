//! L'exécuteur DAG bout en bout, sur un runner fixture et un vrai
//! `SessionManager` : le journal écrit est le vrai journal v2, relu par
//! `EventCursor` comme le ferait `kaji replay`.
//!
//! Le fixture remplace le seul point qui aurait besoin d'un provider et d'une
//! boucle agent — le lancement d'un sous-agent. Tout le reste (ordonnancement,
//! artefacts, gates, budgets, journal) est le code de production.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kaji_core::workflow::WorkflowSpec;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

use crate::config::KajiMode;
use crate::replay::cursor::EventCursor;
use crate::session::session_manager::SessionType;
use crate::session::SessionManager;
use crate::workflow::events::WorkflowRecorder;
use crate::workflow::executor::WorkflowExecutor;
use crate::workflow::gate::{GateDecision, ReplayGates};
use crate::workflow::runner::{AgentRunRequest, AgentRunner};
use crate::workflow::state::{AgentState, BudgetLimit, FailureCause, StageState};

/// Ce qu'un agent fixture fait de son tour.
#[derive(Clone)]
enum Script {
    /// Rend la concaténation de ses entrées, préfixée du nom de l'agent.
    Echo,
    Fail(String),
    /// Ne rend jamais rien tant que son jeton d'annulation n'est pas tiré :
    /// c'est la cible des tests de budget.
    Hang,
    /// Consomme des tokens à chaque relecture d'usage.
    Burn {
        per_poll: i64,
    },
}

#[derive(Default)]
struct Journal {
    started: Vec<String>,
    prompts: Vec<(String, String)>,
    tokens: HashMap<String, i64>,
}

struct FixtureRunner {
    scripts: HashMap<String, Script>,
    default_script: Script,
    journal: Mutex<Journal>,
    next_session: Mutex<u32>,
}

impl FixtureRunner {
    fn new() -> Self {
        Self {
            scripts: HashMap::new(),
            default_script: Script::Echo,
            journal: Mutex::new(Journal::default()),
            next_session: Mutex::new(0),
        }
    }

    fn with(mut self, label: &str, script: Script) -> Self {
        self.scripts.insert(label.to_string(), script);
        self
    }

    fn script(&self, label: &str) -> Script {
        self.scripts
            .get(label)
            .cloned()
            .unwrap_or_else(|| self.default_script.clone())
    }

    fn started(&self) -> Vec<String> {
        self.journal.lock().unwrap().started.clone()
    }

    fn prompt_of(&self, label: &str) -> Option<String> {
        self.journal
            .lock()
            .unwrap()
            .prompts
            .iter()
            .find(|(candidate, _)| candidate == label)
            .map(|(_, prompt)| prompt.clone())
    }
}

#[async_trait::async_trait]
impl AgentRunner for FixtureRunner {
    async fn prepare(&self, request: &AgentRunRequest) -> Result<String, String> {
        let mut next = self.next_session.lock().unwrap();
        *next += 1;
        Ok(format!("fixture_{}_{}", request.label(), next))
    }

    async fn run(
        &self,
        request: AgentRunRequest,
        session_id: &str,
        cancel: CancellationToken,
    ) -> Result<String, String> {
        let label = request.label();
        {
            let mut journal = self.journal.lock().unwrap();
            journal.started.push(label.clone());
            journal
                .prompts
                .push((label.clone(), rendered_inputs(&request.inputs)));
        }

        match self.script(&label) {
            Script::Echo => {
                // Un point de suspension, pour que le fan-out d'un stage soit
                // réellement entrelacé plutôt que séquentiel par accident.
                tokio::task::yield_now().await;
                Ok(format!("[{}]", request.agent))
            }
            Script::Fail(error) => Err(error),
            Script::Hang => {
                cancel.cancelled().await;
                Err("annulé".to_string())
            }
            Script::Burn { per_poll } => {
                self.journal
                    .lock()
                    .unwrap()
                    .tokens
                    .insert(session_id.to_string(), per_poll);
                cancel.cancelled().await;
                Err("annulé".to_string())
            }
        }
    }

    async fn tokens_used(&self, session_id: &str) -> i64 {
        let mut journal = self.journal.lock().unwrap();
        let Some(tokens) = journal.tokens.get_mut(session_id) else {
            return 0;
        };
        let seen = *tokens;
        *tokens += seen;
        seen
    }
}

fn rendered_inputs(inputs: &BTreeMap<String, String>) -> String {
    inputs
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join(";")
}

struct Fixture {
    _data_dir: TempDir,
    session_manager: Arc<SessionManager>,
    session_id: String,
    working_dir: PathBuf,
}

impl Fixture {
    async fn new() -> Self {
        let data_dir = tempfile::tempdir().unwrap();
        let session_manager = Arc::new(SessionManager::new(data_dir.path().join("data")));
        let working_dir = data_dir.path().join("workspace");
        let session = session_manager
            .create_session(
                working_dir.clone(),
                "workflow".to_string(),
                SessionType::Hidden,
                KajiMode::Auto,
            )
            .await
            .unwrap();
        Self {
            _data_dir: data_dir,
            session_manager,
            session_id: session.id,
            working_dir,
        }
    }

    async fn recorder(&self) -> WorkflowRecorder {
        WorkflowRecorder::open(Arc::clone(&self.session_manager), self.session_id.clone())
            .await
            .unwrap()
    }

    async fn executor(&self, spec: WorkflowSpec, runner: Arc<FixtureRunner>) -> WorkflowExecutor {
        WorkflowExecutor::new(
            spec,
            runner,
            self.recorder().await,
            self.session_id.clone(),
            self.working_dir.clone(),
        )
    }

    async fn kinds(&self) -> Vec<String> {
        self.session_manager
            .session_events(&self.session_id)
            .await
            .unwrap()
            .into_iter()
            .map(|event| event.kind)
            .collect()
    }

    async fn payloads(&self, kind: &str) -> Vec<serde_json::Value> {
        self.session_manager
            .session_events(&self.session_id)
            .await
            .unwrap()
            .into_iter()
            .filter(|event| event.kind == kind)
            .filter_map(|event| serde_json::from_str(&event.payload_json).ok())
            .collect()
    }
}

/// `collecte` fan-out ×2 puis `synthese` qui consomme les deux sorties.
/// `synthese` est déclaré **avant** sa dépendance dans le document : c'est le
/// graphe qui ordonne, pas l'index.
const FAN_OUT_THEN_JOIN: &str = r#"
name: revue
stages:
  - name: synthese
    depends_on: [collecte]
    agents:
      - name: redacteur
        prompt: rédige
        inputs:
          scan: "vu {{collecte.scan.output}}"
          lint: "et {{collecte.lint.output}}"
  - name: collecte
    agents:
      - name: scan
        prompt: scanne
      - name: lint
        prompt: linte
"#;

const GATED: &str = r#"
name: livraison
stages:
  - name: build
    agents:
      - name: compile
        prompt: compile
  - name: deploie
    depends_on: [build]
    gate: approve
    agents:
      - name: pousse
        prompt: pousse
  - name: annonce
    depends_on: [deploie]
    agents:
      - name: publie
        prompt: publie
"#;

fn spec(yaml: &str) -> WorkflowSpec {
    WorkflowSpec::from_yaml(yaml).unwrap()
}

#[tokio::test]
async fn a_dag_runs_topologically_and_feeds_artifacts_to_its_descendants() {
    let fixture = Fixture::new().await;
    let runner = Arc::new(FixtureRunner::new());
    let executor = fixture
        .executor(spec(FAN_OUT_THEN_JOIN), Arc::clone(&runner))
        .await;

    let state = executor.run().await.unwrap();

    assert_eq!(state.outcome(), StageState::Done);
    let started = runner.started();
    assert_eq!(
        started.last().map(String::as_str),
        Some("synthese.redacteur"),
        "le stage dépendant part après ses dépendances, quel que soit son rang dans le document"
    );
    assert_eq!(started.len(), 3);
    assert!(
        started[..2].contains(&"collecte.scan".to_string())
            && started[..2].contains(&"collecte.lint".to_string()),
        "le fan-out du premier stage part en parallèle : {started:?}"
    );
    assert_eq!(
        runner.prompt_of("synthese.redacteur").as_deref(),
        Some("lint=et [lint];scan=vu [scan]"),
        "les sorties des ancêtres sont substituées dans les entrées du descendant"
    );
}

#[tokio::test]
async fn a_gate_holds_the_stage_until_it_is_approved() {
    let fixture = Fixture::new().await;
    let runner = Arc::new(FixtureRunner::new());
    let executor = fixture.executor(spec(GATED), Arc::clone(&runner)).await;
    let handle = executor.handle();
    let run = tokio::spawn(executor.run());

    // Le stage gaté attend : rien ne part au-delà du build tant que personne
    // n'a décidé.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(runner.started(), vec!["build.compile".to_string()]);
    assert_eq!(
        handle.snapshot().stage("deploie").unwrap().state,
        StageState::Waiting
    );

    assert!(handle.approve("deploie"));
    let state = run.await.unwrap().unwrap();

    assert_eq!(state.outcome(), StageState::Done);
    assert_eq!(
        runner.started(),
        vec![
            "build.compile".to_string(),
            "deploie.pousse".to_string(),
            "annonce.publie".to_string()
        ]
    );
}

#[tokio::test]
async fn a_denied_gate_cancels_the_stage_and_everything_downstream() {
    let fixture = Fixture::new().await;
    let runner = Arc::new(FixtureRunner::new());
    let executor = fixture.executor(spec(GATED), Arc::clone(&runner)).await;
    let handle = executor.handle();
    let run = tokio::spawn(executor.run());

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(handle.deny("deploie"));
    let state = run.await.unwrap().unwrap();

    assert_eq!(state.stage("build").unwrap().state, StageState::Done);
    assert_eq!(state.stage("deploie").unwrap().state, StageState::Cancelled);
    assert_eq!(state.stage("annonce").unwrap().state, StageState::Cancelled);
    assert_eq!(
        runner.started(),
        vec!["build.compile".to_string()],
        "aucun agent d'un stage refusé ou de sa descendance ne tourne"
    );
}

#[tokio::test]
async fn a_duration_budget_cuts_the_agent_and_names_the_budget() {
    let yaml = r#"
name: borne
stages:
  - name: long
    budgets:
      max_duration_s: 1
    agents:
      - name: interminable
        prompt: attends
"#;
    let fixture = Fixture::new().await;
    let runner = Arc::new(FixtureRunner::new().with("long.interminable", Script::Hang));
    let executor = fixture.executor(spec(yaml), Arc::clone(&runner)).await;

    let state = executor.run().await.unwrap();

    assert_eq!(
        state.stage("long").unwrap().agents[0].state,
        AgentState::Failed(FailureCause::Budget(BudgetLimit::Duration))
    );
    assert_eq!(
        state.stage("long").unwrap().state,
        StageState::Failed(FailureCause::Budget(BudgetLimit::Duration))
    );

    let done = fixture.payloads("agent_done").await;
    assert_eq!(done.len(), 1);
    assert_eq!(done[0]["state"]["failed"]["budget"], "duration");
}

#[tokio::test]
async fn a_token_budget_cuts_the_agent_on_the_child_session_usage() {
    let yaml = r#"
name: borne
stages:
  - name: cher
    budgets:
      max_tokens: 100
    agents:
      - name: gourmand
        prompt: consomme
"#;
    let fixture = Fixture::new().await;
    let runner =
        Arc::new(FixtureRunner::new().with("cher.gourmand", Script::Burn { per_poll: 80 }));
    let executor = fixture.executor(spec(yaml), Arc::clone(&runner)).await;

    let state = executor.run().await.unwrap();

    assert_eq!(
        state.stage("cher").unwrap().agents[0].state,
        AgentState::Failed(FailureCause::Budget(BudgetLimit::Tokens))
    );
}

#[tokio::test]
async fn a_failed_agent_fails_its_stage_and_cancels_the_descendants() {
    let fixture = Fixture::new().await;
    let runner = Arc::new(
        FixtureRunner::new().with("collecte.scan", Script::Fail("disque plein".to_string())),
    );
    let executor = fixture
        .executor(spec(FAN_OUT_THEN_JOIN), Arc::clone(&runner))
        .await;

    let state = executor.run().await.unwrap();

    assert_eq!(
        state.stage("collecte").unwrap().state,
        StageState::Failed(FailureCause::Error("disque plein".to_string()))
    );
    assert_eq!(
        state.stage("synthese").unwrap().state,
        StageState::Cancelled
    );
}

#[tokio::test]
async fn the_v2_kinds_are_journalled_in_execution_order() {
    let fixture = Fixture::new().await;
    let runner = Arc::new(FixtureRunner::new());
    let executor = fixture.executor(spec(GATED), Arc::clone(&runner)).await;
    let handle = executor.handle();
    let run = tokio::spawn(executor.run());
    tokio::time::sleep(Duration::from_millis(50)).await;
    handle.approve("deploie");
    run.await.unwrap().unwrap();

    let kinds = fixture.kinds().await;
    assert_eq!(
        kinds,
        vec![
            "log_meta",
            "workflow_started",
            "stage_started",
            "agent_started",
            "workflow_artifact",
            "agent_done",
            "stage_started",
            "gate_decision",
            "agent_started",
            "workflow_artifact",
            "agent_done",
            "stage_started",
            "agent_started",
            "workflow_artifact",
            "agent_done",
            "workflow_done",
        ]
        .into_iter()
        .map(String::from)
        .collect::<Vec<_>>()
    );

    let started = fixture.payloads("agent_started").await;
    assert!(
        started.iter().all(|payload| payload["session_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty())),
        "chaque agent_started porte l'id de la session enfant"
    );
}

#[tokio::test]
async fn a_replayed_gate_comes_from_the_log_instead_of_a_live_decision() {
    let recorded = Fixture::new().await;
    let runner = Arc::new(FixtureRunner::new());
    let executor = recorded.executor(spec(GATED), Arc::clone(&runner)).await;
    let handle = executor.handle();
    let run = tokio::spawn(executor.run());
    tokio::time::sleep(Duration::from_millis(50)).await;
    handle.approve("deploie");
    run.await.unwrap().unwrap();

    let cursor = EventCursor::load(&recorded.session_manager, &recorded.session_id)
        .await
        .unwrap();
    assert_eq!(
        cursor.gate_decisions.get("deploie"),
        Some(&GateDecision::Approve)
    );

    // Le rejeu tourne sur une autre session parente et n'a **aucune** décision
    // vivante : il ne peut aboutir que si la gate est servie depuis le journal.
    let replayed = Fixture::new().await;
    let replay_runner = Arc::new(FixtureRunner::new());
    let executor = WorkflowExecutor::replaying(
        spec(GATED),
        Arc::clone(&replay_runner) as Arc<dyn AgentRunner>,
        Arc::new(ReplayGates::from_cursor(&cursor)),
        replayed.recorder().await,
        replayed.session_id.clone(),
        replayed.working_dir.clone(),
    );
    let replay_handle = executor.handle();
    assert!(
        !replay_handle.approve("deploie"),
        "un rejeu ne prend pas de décision vivante"
    );

    let state = tokio::time::timeout(Duration::from_secs(5), executor.run())
        .await
        .expect("le rejeu ne doit pas attendre d'approbation")
        .unwrap();

    assert_eq!(state.outcome(), StageState::Done);
    assert_eq!(replay_runner.started(), runner.started());
}

#[tokio::test]
async fn a_replay_without_the_recorded_gate_fails_the_stage_instead_of_waiting() {
    let fixture = Fixture::new().await;
    let runner = Arc::new(FixtureRunner::new());
    let executor = WorkflowExecutor::replaying(
        spec(GATED),
        Arc::clone(&runner) as Arc<dyn AgentRunner>,
        Arc::new(ReplayGates::new(HashMap::new())),
        fixture.recorder().await,
        fixture.session_id.clone(),
        fixture.working_dir.clone(),
    );

    let state = tokio::time::timeout(Duration::from_secs(5), executor.run())
        .await
        .expect("un rejeu strict s'arrête, il n'attend pas")
        .unwrap();

    assert!(matches!(
        state.stage("deploie").unwrap().state,
        StageState::Failed(FailureCause::Error(_))
    ));
    assert_eq!(state.stage("annonce").unwrap().state, StageState::Cancelled);
}
