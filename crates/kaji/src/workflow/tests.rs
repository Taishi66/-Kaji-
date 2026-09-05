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

use kaji_core::workflow::{AgentSource, AgentSpec, Budgets, Gate, Stage, WorkflowSpec};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

use crate::config::KajiMode;
use crate::replay::cursor::EventCursor;
use crate::session::session_manager::{SessionType, TurnClaim};
use crate::session::SessionManager;
use crate::workflow::events::{
    WorkflowRecorder, AGENT_DONE, AGENT_STARTED, STAGE_STARTED, WORKFLOW_KINDS, WORKFLOW_STARTED,
};
use crate::workflow::executor::WorkflowExecutor;
use crate::workflow::gate::{GateDecision, GateVerdict, ReplayGates};
use crate::workflow::runner::{AgentRunRequest, AgentRunner, ReplayRunner};
use crate::workflow::state::{
    AgentState, BudgetLimit, FailureCause, StageState, WorkflowOutcome, WorkflowState,
};

/// Ce qu'un agent fixture fait de son tour.
#[derive(Clone)]
enum Script {
    /// Rend la concaténation de ses entrées, préfixée du nom de l'agent.
    Echo,
    Fail(String),
    /// Ne rend jamais rien tant que son jeton d'annulation n'est pas tiré :
    /// c'est la cible des tests de budget.
    Hang,
    /// Déclare une consommation fixe sur sa session enfant, puis attend son
    /// annulation. La valeur ne bouge pas d'une relecture à l'autre : un
    /// compteur d'usage est une lecture pure, et un test de budget doit
    /// dépendre de la valeur lue, jamais du nombre de lectures.
    Burn {
        tokens: i64,
    },
    /// Ne rend sa sortie qu'une fois l'autre branche partie : un stage qui
    /// attendrait son voisin au lieu de tourner avec lui bloque le test.
    Rendezvous(Arc<tokio::sync::Barrier>),
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
            Script::Burn { tokens } => {
                self.journal
                    .lock()
                    .unwrap()
                    .tokens
                    .insert(session_id.to_string(), tokens);
                cancel.cancelled().await;
                Err("annulé".to_string())
            }
            Script::Rendezvous(barrier) => {
                barrier.wait().await;
                Ok(format!("[{}]", request.agent))
            }
        }
    }

    async fn tokens_used(&self, session_id: &str) -> i64 {
        self.journal
            .lock()
            .unwrap()
            .tokens
            .get(session_id)
            .copied()
            .unwrap_or(0)
    }
}

/// Le runner de rejeu, doublé d'un témoin de lancement. Un témoin qui n'est
/// pas **branché** sur l'exécuteur ne peut rien prouver : celui-ci sert le
/// journal comme `ReplayRunner` et note tout appel à `run`, donc l'assertion
/// « aucun sous-agent réel » tombe si l'exécuteur en lance un.
struct WitnessedReplayRunner {
    inner: ReplayRunner,
    launched: Mutex<Vec<String>>,
}

impl WitnessedReplayRunner {
    fn new(inner: ReplayRunner) -> Self {
        Self {
            inner,
            launched: Mutex::new(Vec::new()),
        }
    }

    fn launched(&self) -> Vec<String> {
        self.launched.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl AgentRunner for WitnessedReplayRunner {
    async fn prepare(&self, request: &AgentRunRequest) -> Result<String, String> {
        self.inner.prepare(request).await
    }

    async fn run(
        &self,
        request: AgentRunRequest,
        session_id: &str,
        cancel: CancellationToken,
    ) -> Result<String, String> {
        self.launched.lock().unwrap().push(request.label());
        self.inner.run(request, session_id, cancel).await
    }

    async fn tokens_used(&self, session_id: &str) -> i64 {
        self.inner.tokens_used(session_id).await
    }

    async fn recorded_outcome(
        &self,
        request: &AgentRunRequest,
    ) -> Option<crate::workflow::runner::RecordedOutcome> {
        self.inner.recorded_outcome(request).await
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
        self.open_recorder("test").await.unwrap()
    }

    async fn open_recorder(&self, workflow: &str) -> anyhow::Result<WorkflowRecorder> {
        WorkflowRecorder::open(
            Arc::clone(&self.session_manager),
            self.session_id.clone(),
            workflow,
        )
        .await
    }

    async fn executor(&self, spec: WorkflowSpec, runner: Arc<FixtureRunner>) -> WorkflowExecutor {
        let recorder = self.open_recorder(&spec.name).await.unwrap();
        WorkflowExecutor::new(
            spec,
            runner,
            recorder,
            self.session_id.clone(),
            self.working_dir.clone(),
        )
        .unwrap()
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

    assert_eq!(state.outcome(), WorkflowOutcome::Done);
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

    assert_eq!(handle.approve("deploie"), GateVerdict::Applied);
    let state = run.await.unwrap().unwrap();

    assert_eq!(state.outcome(), WorkflowOutcome::Done);
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
    assert_eq!(handle.deny("deploie"), GateVerdict::Applied);
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
    let runner = Arc::new(FixtureRunner::new().with("cher.gourmand", Script::Burn { tokens: 150 }));
    let executor = fixture.executor(spec(yaml), Arc::clone(&runner)).await;

    let state = executor.run().await.unwrap();

    assert_eq!(
        state.stage("cher").unwrap().agents[0].state,
        AgentState::Failed(FailureCause::Budget(BudgetLimit::Tokens))
    );
}

/// C-1 : `max_tokens` est porté par le **stage**, donc partagé par ses agents.
/// Deux agents sous le budget chacun mais au-dessus à eux deux doivent être
/// coupés tous les deux — sinon un fan-out de N agents dépense N × le budget.
#[tokio::test]
async fn a_token_budget_is_shared_across_the_agents_of_its_stage() {
    let yaml = r#"
name: borne
stages:
  - name: cher
    budgets:
      max_tokens: 100
    agents:
      - name: gauche
        prompt: consomme
      - name: droite
        prompt: consomme
"#;
    let fixture = Fixture::new().await;
    let runner = Arc::new(
        FixtureRunner::new()
            .with("cher.gauche", Script::Burn { tokens: 60 })
            .with("cher.droite", Script::Burn { tokens: 60 }),
    );
    let executor = fixture.executor(spec(yaml), Arc::clone(&runner)).await;

    let state = tokio::time::timeout(Duration::from_secs(5), executor.run())
        .await
        .expect("le budget du stage est partagé : 60 + 60 > 100 coupe le fan-out")
        .unwrap();

    let stage = state.stage("cher").unwrap();
    for agent in &stage.agents {
        assert_eq!(
            agent.state,
            AgentState::Failed(FailureCause::Budget(BudgetLimit::Tokens)),
            "l'agent « {} » devait être coupé par le budget du stage",
            agent.name
        );
    }
    assert_eq!(
        stage.state,
        StageState::Failed(FailureCause::Budget(BudgetLimit::Tokens))
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
            "turn_start",
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
            "turn_end",
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

/// M-1 : `WORKFLOW_KINDS` prétend énumérer les kinds de l'orchestration. Sans
/// lecteur, la liste dérive au premier kind ajouté ailleurs.
#[tokio::test]
async fn the_recorder_writes_no_kind_outside_the_declared_family() {
    let fixture = Fixture::new().await;
    let runner = Arc::new(FixtureRunner::new());
    let executor = fixture
        .executor(spec(FAN_OUT_THEN_JOIN), Arc::clone(&runner))
        .await;
    executor.run().await.unwrap();

    let journal = fixture.kinds().await;
    let bornes = ["log_meta", "turn_start", "turn_end"];
    for kind in &journal {
        assert!(
            bornes.contains(&kind.as_str()) || WORKFLOW_KINDS.contains(&kind.as_str()),
            "kind « {kind} » écrit hors de WORKFLOW_KINDS"
        );
    }
    for kind in [WORKFLOW_STARTED, STAGE_STARTED, AGENT_STARTED, AGENT_DONE] {
        assert!(
            journal.iter().any(|written| written == kind),
            "le kind structurel « {kind} » n'a pas été écrit"
        );
    }
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
        cursor.workflow_recipes.clone(),
        replayed.recorder().await,
        replayed.session_id.clone(),
        replayed.working_dir.clone(),
    )
    .unwrap();
    let replay_handle = executor.handle();
    assert_eq!(
        replay_handle.approve("deploie"),
        GateVerdict::Settled,
        "un rejeu ne prend pas de décision vivante"
    );

    let state = tokio::time::timeout(Duration::from_secs(5), executor.run())
        .await
        .expect("le rejeu ne doit pas attendre d'approbation")
        .unwrap();

    assert_eq!(state.outcome(), WorkflowOutcome::Done);
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
        HashMap::new(),
        fixture.recorder().await,
        fixture.session_id.clone(),
        fixture.working_dir.clone(),
    )
    .unwrap();

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

/// I-1 : une spec relue d'un payload `workflow_started` n'est pas passée par
/// `from_yaml` — c'est l'exécuteur qui doit la valider, sinon un `depends_on`
/// pointant un stage disparu laisse tout `Pending` et sort vert.
#[tokio::test]
async fn an_executor_refuses_a_spec_that_never_passed_validation() {
    let fixture = Fixture::new().await;
    let broken = WorkflowSpec {
        name: "cassé".to_string(),
        stages: vec![Stage {
            name: "seul".to_string(),
            agents: vec![AgentSpec {
                name: "agent".to_string(),
                source: AgentSource::Prompt("fais".to_string()),
                model: None,
                inputs: BTreeMap::new(),
            }],
            depends_on: vec!["disparu".to_string()],
            gate: Gate::Auto,
            budgets: Budgets::default(),
        }],
    };

    let Err(error) = WorkflowExecutor::new(
        broken,
        Arc::new(FixtureRunner::new()),
        fixture.recorder().await,
        fixture.session_id.clone(),
        fixture.working_dir.clone(),
    ) else {
        panic!("une spec invalide ne construit pas d'exécuteur");
    };
    assert!(
        error.to_string().contains("disparu"),
        "l'erreur nomme la dépendance inconnue : {error}"
    );
}

/// I-1 (second volet) : même sur une spec valide, un état où un stage est
/// resté non terminal est un échec nommé — jamais un `Done` silencieux.
#[test]
fn an_outcome_names_the_stage_that_never_finished() {
    let mut state = WorkflowState::from_spec(&spec(GATED));
    state.stages[0].state = StageState::Done;

    let outcome = state.outcome();
    let WorkflowOutcome::Failed(FailureCause::Error(message)) = outcome else {
        panic!("un stage resté Pending doit rendre un échec nommé, pas {outcome:?}");
    };
    assert!(message.contains("deploie"), "{message}");
}

/// I-2 : `approve`/`deny` distinguent trois cas. `true` pour un stage mort ou
/// inexistant ferait afficher « approuvé » sur un stage que personne ne lira.
#[tokio::test]
async fn a_decision_on_a_dead_or_unknown_stage_is_refused() {
    let fixture = Fixture::new().await;
    let runner = Arc::new(FixtureRunner::new());
    let executor = fixture.executor(spec(GATED), Arc::clone(&runner)).await;
    let handle = executor.handle();
    let run = tokio::spawn(executor.run());

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(handle.approve("inconnu"), GateVerdict::UnknownStage);
    assert_eq!(handle.deny("deploie"), GateVerdict::Applied);
    run.await.unwrap().unwrap();

    assert_eq!(
        handle.approve("deploie"),
        GateVerdict::Settled,
        "un stage terminal ne prend plus de décision"
    );
    assert_eq!(
        handle.approve("annonce"),
        GateVerdict::Settled,
        "un stage emporté par la cascade non plus"
    );
    assert_eq!(
        fixture.payloads("gate_decision").await.len(),
        1,
        "seule la décision réellement consommée est journalisée"
    );
}

/// I-2 (résidu F-3) : un stage vivant **sans gate** ne prend pas de décision.
/// `Applied` y enregistrerait une approbation que `run_stage` ne consomme
/// jamais, et T6 afficherait « décision enregistrée » sur un stage qui n'a
/// rien à approuver.
#[tokio::test]
async fn a_decision_on_a_stage_without_a_gate_is_refused() {
    let fixture = Fixture::new().await;
    let runner = Arc::new(FixtureRunner::new().with("build.compile", Script::Hang));
    let executor = fixture.executor(spec(GATED), Arc::clone(&runner)).await;
    let handle = executor.handle();
    let run = tokio::spawn(executor.run());

    // `build` tourne encore : il n'est ni inconnu ni terminal, seule l'absence
    // de gate peut le refuser.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        handle.snapshot().stage("build").unwrap().state,
        StageState::Running
    );
    assert_eq!(handle.approve("build"), GateVerdict::NoGate);
    assert_eq!(handle.deny("build"), GateVerdict::NoGate);

    handle.cancel();
    tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .expect("cancel() coupe l'agent suspendu")
        .unwrap()
        .unwrap();

    assert!(
        fixture.payloads("gate_decision").await.is_empty(),
        "un stage sans gate n'enregistre aucune décision"
    );
}

/// I-3 : une gate sans approbateur attend sans borne — `cancel()` est la
/// sortie. Le stage gaté part en `Cancelled`, sa descendance avec, et le
/// journal se referme (`workflow_done` + `turn_end`).
#[tokio::test]
async fn cancelling_a_waiting_gate_ends_the_workflow_with_a_closed_journal() {
    let fixture = Fixture::new().await;
    let runner = Arc::new(FixtureRunner::new());
    let executor = fixture.executor(spec(GATED), Arc::clone(&runner)).await;
    let handle = executor.handle();
    let run = tokio::spawn(executor.run());

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        handle.snapshot().stage("deploie").unwrap().state,
        StageState::Waiting
    );
    handle.cancel();

    let state = tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .expect("cancel() réveille l'attente de gate")
        .unwrap()
        .unwrap();

    assert_eq!(state.outcome(), WorkflowOutcome::Cancelled);
    assert_eq!(state.stage("build").unwrap().state, StageState::Done);
    assert_eq!(state.stage("deploie").unwrap().state, StageState::Cancelled);
    assert_eq!(state.stage("annonce").unwrap().state, StageState::Cancelled);
    assert_eq!(
        state.stage("deploie").unwrap().agents[0].state,
        AgentState::Cancelled,
        "les agents d'un stage annulé ne restent pas Pending dans la vue"
    );
    assert_eq!(
        runner.started(),
        vec!["build.compile".to_string()],
        "aucun agent ne part au-delà de la gate annulée"
    );

    let kinds = fixture.kinds().await;
    assert!(
        kinds.contains(&"workflow_done".to_string())
            && kinds.last() == Some(&"turn_end".to_string()),
        "un workflow annulé referme son tour : {kinds:?}"
    );
    assert!(
        fixture.payloads("gate_decision").await.is_empty(),
        "une gate annulée n'a pris aucune décision"
    );
}

/// I-3 : annulation pendant qu'un agent tourne — il est coupé, l'état le dit,
/// et le stage suivant ne démarre pas.
#[tokio::test]
async fn cancelling_an_in_flight_agent_cuts_it_and_stops_the_dag() {
    let fixture = Fixture::new().await;
    let runner = Arc::new(FixtureRunner::new().with("build.compile", Script::Hang));
    let executor = fixture.executor(spec(GATED), Arc::clone(&runner)).await;
    let handle = executor.handle();
    let run = tokio::spawn(executor.run());

    tokio::time::sleep(Duration::from_millis(50)).await;
    handle.cancel();

    let state = tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .expect("cancel() coupe l'agent en vol")
        .unwrap()
        .unwrap();

    assert_eq!(
        state.stage("build").unwrap().agents[0].state,
        AgentState::Cancelled
    );
    assert_eq!(state.stage("build").unwrap().state, StageState::Cancelled);
    assert_eq!(state.stage("deploie").unwrap().state, StageState::Cancelled);
    assert_eq!(runner.started(), vec!["build.compile".to_string()]);
}

/// I-3 : annulation avant qu'un stage `Pending` démarre — le garde d'entrée de
/// `run_stage` refuse de le lancer.
#[tokio::test]
async fn a_stage_that_has_not_started_is_cancelled_at_its_gate_of_entry() {
    let fixture = Fixture::new().await;
    let runner = Arc::new(FixtureRunner::new());
    let executor = fixture.executor(spec(GATED), Arc::clone(&runner)).await;
    let handle = executor.handle();
    handle.cancel();

    let state = tokio::time::timeout(Duration::from_secs(5), executor.run())
        .await
        .expect("un workflow annulé avant de partir ne bloque pas")
        .unwrap();

    assert_eq!(state.outcome(), WorkflowOutcome::Cancelled);
    assert!(
        runner.started().is_empty(),
        "aucun agent ne part sur un workflow déjà annulé : {:?}",
        runner.started()
    );
}

/// I-4 : `Failed` l'emporte sur `Cancelled` quel que soit le rang des agents
/// dans le document. Le couple est **réel** : un agent qui échoue de sa propre
/// erreur, un autre coupé par `cancel()` — un stage dont les deux agents
/// tombent sous le même budget ne teste pas cette précédence, il ne produit
/// aucun `Cancelled`.
#[tokio::test]
async fn a_stage_state_is_aggregated_by_precedence_not_by_declaration_order() {
    async fn outcome_of(first: &str, second: &str) -> (StageState, Vec<AgentState>) {
        let yaml = format!(
            r#"
name: ordre
stages:
  - name: melange
    agents:
      - name: {first}
        prompt: fais
      - name: {second}
        prompt: fais
"#
        );
        let fixture = Fixture::new().await;
        let runner = Arc::new(
            FixtureRunner::new()
                .with("melange.suspendu", Script::Hang)
                .with("melange.casse", Script::Fail("disque plein".to_string())),
        );
        let executor = fixture.executor(spec(&yaml), Arc::clone(&runner)).await;
        let handle = executor.handle();
        let run = tokio::spawn(executor.run());

        // `casse` a déjà rendu son erreur, `suspendu` attend : l'annulation
        // n'arrive qu'ensuite, donc l'échec n'est pas imputable à la coupure.
        tokio::time::sleep(Duration::from_millis(50)).await;
        handle.cancel();

        let state = tokio::time::timeout(Duration::from_secs(5), run)
            .await
            .expect("cancel() coupe l'agent suspendu")
            .unwrap()
            .unwrap();
        let stage = state.stage("melange").unwrap();
        (
            stage.state.clone(),
            stage
                .agents
                .iter()
                .map(|agent| agent.state.clone())
                .collect(),
        )
    }

    let (failure_first, agents_first) = outcome_of("casse", "suspendu").await;
    let (failure_last, agents_last) = outcome_of("suspendu", "casse").await;

    let failed = AgentState::Failed(FailureCause::Error("disque plein".to_string()));
    assert!(
        agents_first.contains(&failed) && agents_first.contains(&AgentState::Cancelled),
        "le couple exercé doit être (Failed, Cancelled) : {agents_first:?}"
    );
    assert!(
        agents_last.contains(&failed) && agents_last.contains(&AgentState::Cancelled),
        "le couple exercé doit être (Failed, Cancelled) : {agents_last:?}"
    );
    assert_eq!(
        failure_first,
        StageState::Failed(FailureCause::Error("disque plein".to_string()))
    );
    assert_eq!(
        failure_last, failure_first,
        "Failed l'emporte sur Cancelled quel que soit l'ordre YAML"
    );
}

/// I-4 (second volet) : deux agents coupés par le **même** budget de stage
/// donnent le même échec nommé dans les deux ordres — la variante que C-1 a
/// rendue possible, et qui ne dit rien de la précédence `Failed > Cancelled`.
#[tokio::test]
async fn a_shared_budget_names_the_same_cause_in_both_declaration_orders() {
    async fn outcome_of(first: &str, second: &str) -> StageState {
        let yaml = format!(
            r#"
name: ordre
stages:
  - name: melange
    budgets:
      max_tokens: 100
    agents:
      - name: {first}
        prompt: fais
      - name: {second}
        prompt: fais
"#
        );
        let fixture = Fixture::new().await;
        let runner = Arc::new(
            FixtureRunner::new()
                .with("melange.sobre", Script::Burn { tokens: 60 })
                .with("melange.gourmand", Script::Burn { tokens: 150 }),
        );
        let executor = fixture.executor(spec(&yaml), Arc::clone(&runner)).await;
        let state = tokio::time::timeout(Duration::from_secs(5), executor.run())
            .await
            .expect("le budget partagé coupe le stage")
            .unwrap();
        state.stage("melange").unwrap().state.clone()
    }

    let budget_first = outcome_of("gourmand", "sobre").await;
    let budget_last = outcome_of("sobre", "gourmand").await;

    assert_eq!(
        budget_first,
        StageState::Failed(FailureCause::Budget(BudgetLimit::Tokens))
    );
    assert_eq!(budget_last, budget_first);
}

/// I-5 : deux stages sans lien partent **ensemble** — la propriété centrale de
/// l'ordonnanceur. Le losange `racine → {a, b} → jointure` la met à l'épreuve :
/// les deux branches doivent être démarrées avant qu'aucune ne finisse, et
/// leurs events s'entrelacent sans que le curseur, adressé par clé, s'en
/// trouve gêné.
#[tokio::test]
async fn two_unlinked_stages_run_concurrently_and_their_events_stay_addressable() {
    const DIAMOND: &str = r#"
name: losange
stages:
  - name: racine
    agents:
      - name: seed
        prompt: sème
  - name: gauche
    depends_on: [racine]
    agents:
      - name: a
        prompt: a
  - name: droite
    depends_on: [racine]
    agents:
      - name: b
        prompt: b
  - name: jointure
    depends_on: [gauche, droite]
    agents:
      - name: join
        prompt: joins
        inputs:
          gauche: "{{gauche.a.output}}"
          droite: "{{droite.b.output}}"
"#;

    let fixture = Fixture::new().await;
    // Les deux branches se rendez-vous : aucune ne peut finir avant que
    // l'autre soit partie. Le test échoue par timeout si elles sont
    // séquentielles.
    let both_started = Arc::new(tokio::sync::Barrier::new(2));
    let runner = Arc::new(
        FixtureRunner::new()
            .with("gauche.a", Script::Rendezvous(Arc::clone(&both_started)))
            .with("droite.b", Script::Rendezvous(Arc::clone(&both_started))),
    );
    let executor = fixture.executor(spec(DIAMOND), Arc::clone(&runner)).await;

    let state = tokio::time::timeout(Duration::from_secs(5), executor.run())
        .await
        .expect("les deux branches du losange tournent en parallèle")
        .unwrap();

    assert_eq!(state.outcome(), WorkflowOutcome::Done);
    let started = runner.started();
    assert_eq!(started.first().map(String::as_str), Some("racine.seed"));
    assert_eq!(started.last().map(String::as_str), Some("jointure.join"));

    // Les events des deux branches sont entrelacés dans le journal, mais le
    // curseur les rend par clé : l'ordre d'arrivée n'a aucun effet.
    let cursor = EventCursor::load(&fixture.session_manager, &fixture.session_id)
        .await
        .unwrap();
    assert_eq!(
        cursor
            .workflow_artifacts
            .get(&("gauche".to_string(), "a".to_string()))
            .map(String::as_str),
        Some("[a]")
    );
    assert_eq!(
        cursor
            .workflow_artifacts
            .get(&("droite".to_string(), "b".to_string()))
            .map(String::as_str),
        Some("[b]")
    );
    assert_eq!(
        runner.prompt_of("jointure.join").as_deref(),
        Some("droite=[b];gauche=[a]"),
        "la jointure reçoit les sorties des deux branches"
    );
}

/// Invariant des index : une session parente ne porte qu'un workflow. La clé
/// de gate est un nom de stage — un second run sur la même session écraserait
/// les décisions du premier.
#[tokio::test]
async fn a_session_refuses_a_second_workflow() {
    let fixture = Fixture::new().await;
    let first = fixture.open_recorder("premier").await.unwrap();

    let Err(error) = fixture.open_recorder("second").await else {
        panic!("une session ne porte qu'un workflow");
    };
    assert!(
        error.to_string().contains("premier"),
        "l'erreur nomme le workflow déjà attaché : {error}"
    );
    assert_eq!(first.turn_seq(), 2, "le premier garde son tour");
}

/// I-6 : le tour du workflow est **revendiqué** par un `turn_start`, sous
/// l'index unique d'allocation. La collision est ici réelle — le tour visé est
/// déjà pris par un tour d'agent — donc c'est bien l'index qui tranche, et le
/// workflow va chercher un tour libre au lieu d'écrire sous le même numéro.
#[tokio::test]
async fn a_turn_already_taken_is_refused_by_the_index_and_the_workflow_moves_on() {
    let fixture = Fixture::new().await;
    fixture
        .session_manager
        .append_event(
            &fixture.session_id,
            2,
            "turn_start",
            r#"{"query_preview":"agent"}"#,
        )
        .await
        .unwrap();

    let claim = fixture
        .session_manager
        .claim_exclusive_turn_start(
            &fixture.session_id,
            2,
            "workflow",
            r#"{"query_preview":"workflow","workflow":"après"}"#,
        )
        .await
        .unwrap();
    assert_eq!(
        claim,
        TurnClaim::TurnTaken,
        "le tour 2 appartient au tour d'agent : la revendication perd"
    );

    let turn_two: Vec<serde_json::Value> = fixture
        .session_manager
        .session_events(&fixture.session_id)
        .await
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == "turn_start" && event.turn_seq == 2)
        .filter_map(|event| serde_json::from_str(&event.payload_json).ok())
        .collect();
    assert_eq!(turn_two.len(), 1, "aucun second turn_start sous le tour 2");
    assert!(
        turn_two[0].get("workflow").is_none(),
        "le tour de l'agent n'a pas été écrasé : {}",
        turn_two[0]
    );

    let recorder = fixture.open_recorder("après").await.unwrap();
    assert!(
        recorder.turn_seq() > 2,
        "le workflow prend un tour libre, pas celui de l'agent : {}",
        recorder.turn_seq()
    );
}

/// F-4 : deux `open()` concurrents sur la même session. L'exclusivité et
/// l'écriture du `turn_start` tiennent dans une seule transaction : le perdant
/// est **refusé**, jamais poussé sur le tour suivant — sinon la session
/// porterait deux workflows, chacun avec son tour, alors que les gates y sont
/// adressées par nom de stage.
#[tokio::test]
async fn two_concurrent_opens_leave_exactly_one_workflow_on_the_session() {
    let fixture = Fixture::new().await;

    // Le cas exact du TOCTOU : le demandeur a lu « libre », a alloué un tour
    // encore vacant, et arrive à l'écriture après le workflow gagnant. Le
    // refus est dans la même transaction que l'INSERT, donc son tour libre ne
    // le sauve pas.
    let attached = Fixture::new().await;
    let _first = attached.open_recorder("premier").await.unwrap();
    let late = attached
        .session_manager
        .claim_exclusive_turn_start(
            &attached.session_id,
            3,
            "workflow",
            r#"{"query_preview":"workflow","workflow":"second"}"#,
        )
        .await
        .unwrap();
    assert_eq!(
        late,
        TurnClaim::AlreadyExclusive("premier".to_string()),
        "un tour libre ne rattrape pas une session déjà prise"
    );

    let (left, right) = tokio::join!(
        fixture.open_recorder("gauche"),
        fixture.open_recorder("droite")
    );

    let (winner, loser) = match (left, right) {
        (Ok(winner), Err(loser)) | (Err(loser), Ok(winner)) => (winner, loser),
        (Ok(left), Ok(right)) => panic!(
            "deux workflows attachés à la même session (tours {} et {})",
            left.turn_seq(),
            right.turn_seq()
        ),
        (Err(left), Err(right)) => panic!("aucun gagnant : {left} / {right}"),
    };
    assert!(
        loser.to_string().contains("porte déjà"),
        "le perdant est refusé par un nom, pas par un tour de plus : {loser}"
    );

    let workflow_turns = fixture
        .session_manager
        .session_events(&fixture.session_id)
        .await
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == "turn_start")
        .filter(|event| {
            crate::workflow::events::workflow_of_turn_start(&event.payload_json).is_some()
        })
        .count();
    assert_eq!(workflow_turns, 1, "un seul tour de workflow sur la session");
    assert_eq!(winner.turn_seq(), 2);
}

/// I-7 : le rejeu d'un parent sert les sorties du journal et ne lance **aucun**
/// sous-agent. Sans ce runner, un rejeu relancerait de vrais agents, produirait
/// d'autres sorties, et les entrées substituées des descendants divergeraient.
#[tokio::test]
async fn a_replayed_workflow_serves_its_agent_outputs_from_the_log() {
    let recorded = Fixture::new().await;
    let runner = Arc::new(
        FixtureRunner::new().with("collecte.lint", Script::Fail("disque plein".to_string())),
    );
    let executor = recorded
        .executor(spec(FAN_OUT_THEN_JOIN), Arc::clone(&runner))
        .await;
    let recorded_state = executor.run().await.unwrap();

    let cursor = EventCursor::load(&recorded.session_manager, &recorded.session_id)
        .await
        .unwrap();

    let replayed = Fixture::new().await;
    // Le runner de rejeu est branché derrière un témoin : tout appel à `run`
    // — le seul chemin qui lancerait un vrai sous-agent — y laisse une trace.
    let witness = Arc::new(WitnessedReplayRunner::new(ReplayRunner::from_cursor(
        &cursor,
    )));
    let executor = WorkflowExecutor::replaying(
        cursor.workflow.clone().unwrap().spec,
        Arc::clone(&witness) as Arc<dyn AgentRunner>,
        Arc::new(ReplayGates::from_cursor(&cursor)),
        cursor.workflow_recipes.clone(),
        replayed.recorder().await,
        replayed.session_id.clone(),
        replayed.working_dir.clone(),
    )
    .unwrap();

    let state = tokio::time::timeout(Duration::from_secs(5), executor.run())
        .await
        .expect("le rejeu ne dépend d'aucun agent vivant")
        .unwrap();

    assert!(
        witness.launched().is_empty(),
        "aucun agent réel ne doit partir au rejeu : {:?}",
        witness.launched()
    );
    assert_eq!(
        state.topology(),
        recorded_state.topology(),
        "le rejeu reproduit l'état enregistré, échec d'agent compris"
    );
    assert_eq!(
        state.stage("collecte").unwrap().state,
        StageState::Failed(FailureCause::Error("disque plein".to_string())),
        "un échec enregistré se rejoue en échec, pas en succès"
    );
}

/// I-7 (second volet) : une sortie purgée par la rétention ne se remplace pas
/// par du vide — le rejeu échoue avec une cause nommée plutôt que de
/// substituer une chaîne absente dans le prompt d'un descendant.
#[tokio::test]
async fn a_purged_artifact_fails_the_replay_instead_of_substituting_nothing() {
    let recorded = Fixture::new().await;
    let runner = Arc::new(FixtureRunner::new());
    let executor = recorded
        .executor(spec(FAN_OUT_THEN_JOIN), Arc::clone(&runner))
        .await;
    executor.run().await.unwrap();

    let mut cursor = EventCursor::load(&recorded.session_manager, &recorded.session_id)
        .await
        .unwrap();
    cursor.workflow_artifacts.clear();

    let replayed = Fixture::new().await;
    let executor = WorkflowExecutor::replaying(
        cursor.workflow.clone().unwrap().spec,
        Arc::new(ReplayRunner::from_cursor(&cursor)),
        Arc::new(ReplayGates::from_cursor(&cursor)),
        cursor.workflow_recipes.clone(),
        replayed.recorder().await,
        replayed.session_id.clone(),
        replayed.working_dir.clone(),
    )
    .unwrap();

    let state = tokio::time::timeout(Duration::from_secs(5), executor.run())
        .await
        .expect("un journal amputé ne fait pas attendre le rejeu")
        .unwrap();

    let StageState::Failed(FailureCause::Error(message)) =
        state.stage("collecte").unwrap().state.clone()
    else {
        panic!("une sortie purgée doit nommer sa cause");
    };
    assert!(message.contains("purgée"), "{message}");
    assert_eq!(
        state.stage("synthese").unwrap().state,
        StageState::Cancelled
    );
}

/// I-8 : le tour du workflow est visible au plan de rejeu. Sans lui, `kaji
/// replay` d'une session de workflow rendait un plan vide et sortait vert sans
/// avoir rien rejoué.
#[tokio::test]
async fn the_workflow_turn_is_visible_to_the_replay_plan() {
    let fixture = Fixture::new().await;
    let runner = Arc::new(FixtureRunner::new());
    let executor = fixture
        .executor(spec(FAN_OUT_THEN_JOIN), Arc::clone(&runner))
        .await;
    executor.run().await.unwrap();

    let events = fixture
        .session_manager
        .session_events(&fixture.session_id)
        .await
        .unwrap();
    let plan = crate::replay::plan::replay_plan(&events, None);

    assert_eq!(plan.len(), 1);
    let (turn_seq, planned) = &plan[0];
    assert_eq!(*turn_seq, 2);
    let crate::replay::plan::PlannedTurn::Workflow(name) = planned else {
        panic!("le tour de workflow doit être planifié comme tel, pas sauté");
    };
    assert_eq!(name, "revue");
}

/// I-8 (second volet) : un workflow tué laisse sa borne ouverte, donc le
/// curseur refuse le journal au lieu de le rejouer à moitié.
#[tokio::test]
async fn a_killed_workflow_is_detected_as_truncated() {
    let fixture = Fixture::new().await;
    let recorder = fixture.recorder().await;
    recorder.workflow_started(&spec(FAN_OUT_THEN_JOIN)).await;
    recorder.stage_started("collecte", Gate::Auto).await;
    // Pas de `workflow_done` : le processus est mort en plein stage.

    let Err(error) = EventCursor::load(&fixture.session_manager, &fixture.session_id).await else {
        panic!("un workflow tronqué ne se charge pas");
    };
    assert_eq!(
        error.downcast_ref::<crate::replay::cursor::ReplayUnavailable>(),
        Some(&crate::replay::cursor::ReplayUnavailable::TruncatedAt(2))
    );
}

/// I-9 : le contenu d'une recette entre dans le prompt de l'agent. Il est
/// journalisé à l'exécution, puis servi au rejeu — un fichier édité entre les
/// deux ne change pas ce que le rejeu voit.
#[tokio::test]
async fn a_recipe_is_journalled_then_served_from_the_log() {
    let fixture = Fixture::new().await;
    let recipe_path = fixture._data_dir.path().join("revue.yaml");
    std::fs::write(
        &recipe_path,
        "version: 1.0.0\ntitle: revue\ndescription: revue\nprompt: version enregistrée\n",
    )
    .unwrap();

    let yaml = format!(
        r#"
name: recette
stages:
  - name: revue
    agents:
      - name: lecteur
        recipe: {}
"#,
        recipe_path.display()
    );
    let runner = Arc::new(FixtureRunner::new());
    let executor = fixture.executor(spec(&yaml), Arc::clone(&runner)).await;
    executor.run().await.unwrap();

    let cursor = EventCursor::load(&fixture.session_manager, &fixture.session_id)
        .await
        .unwrap();
    let recorded = cursor
        .workflow_recipes
        .get(&recipe_path.to_string_lossy().to_string())
        .expect("le contenu de la recette est journalisé");
    assert!(
        recorded.content.contains("version enregistrée"),
        "{}",
        recorded.content
    );

    // Le fichier change ; le rejeu doit continuer à voir la version du journal.
    std::fs::write(
        &recipe_path,
        "version: 1.0.0\ntitle: revue\ndescription: revue\nprompt: version éditée\n",
    )
    .unwrap();

    let replayed = Fixture::new().await;
    let executor = WorkflowExecutor::replaying(
        cursor.workflow.clone().unwrap().spec,
        Arc::new(ReplayRunner::from_cursor(&cursor)),
        Arc::new(ReplayGates::from_cursor(&cursor)),
        cursor.workflow_recipes.clone(),
        replayed.recorder().await,
        replayed.session_id.clone(),
        replayed.working_dir.clone(),
    )
    .unwrap();
    executor.run().await.unwrap();

    let replayed_cursor = EventCursor::load(&replayed.session_manager, &replayed.session_id)
        .await
        .unwrap();
    assert!(
        replayed_cursor.workflow_recipes.is_empty(),
        "un rejeu ne relit pas le disque, donc ne rejournalise aucune recette"
    );
}

/// F-8 : le contenu d'une recette est volumineux — c'est la raison d'être de
/// son kind purgeable. Un stage de N agents sur la même recette n'en écrit
/// qu'une copie, et tous voient la **même** version, même si le fichier change
/// pendant le run.
#[tokio::test]
async fn a_recipe_shared_by_several_agents_is_journalled_once() {
    let fixture = Fixture::new().await;
    let recipe_path = fixture._data_dir.path().join("partagee.yaml");
    std::fs::write(
        &recipe_path,
        "version: 1.0.0\ntitle: partagée\ndescription: partagée\nprompt: version enregistrée\n",
    )
    .unwrap();

    let yaml = format!(
        r#"
name: recette
stages:
  - name: revue
    agents:
      - name: lecteur
        recipe: {path}
      - name: relecteur
        recipe: {path}
"#,
        path = recipe_path.display()
    );
    let runner = Arc::new(FixtureRunner::new());
    let executor = fixture.executor(spec(&yaml), Arc::clone(&runner)).await;
    executor.run().await.unwrap();

    assert_eq!(
        fixture.payloads("workflow_recipe").await.len(),
        1,
        "la recette n'est journalisée qu'une fois, pas une fois par agent"
    );
}
