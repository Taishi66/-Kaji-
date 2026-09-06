//! Le doré du rejeu d'orchestration : un DAG enregistré par le vrai
//! exécuteur, avec les trois façons dont une gate se règle, puis rejoué
//! **deux fois** en strict — même topologie que l'enregistrement, et même
//! topologie entre les deux rejeux.
//!
//! Trois sorties de gate sur une seule exécution, parce qu'elles ne se
//! ressemblent pas au journal :
//! - **approuvée** — `gate_decision(Approve)` écrite, servie telle quelle ;
//! - **refusée** — `gate_decision(Deny)` écrite, le stage tombe et sa
//!   descendance avec lui ;
//! - **annulée à la gate** — *rien* n'est écrit, parce qu'il n'y a pas eu de
//!   décision. C'est le kind `workflow_cancelled`, daté par sa place au
//!   journal, qui l'explique, et c'est ce que le rejeu doit savoir lire : sans
//!   ça il réclame une approbation que personne n'a donnée et diverge.
//!
//! Le rejeu tourne sur `ReplayGates` + `ReplayRunner` : aucun sous-agent ne
//! part, aucune décision vivante n'est acceptée. Un témoin branché **sur** le
//! runner du rejeu le prouve — un témoin gardé à côté ne prouverait rien.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kaji::config::KajiMode;
use kaji::replay::cursor::EventCursor;
use kaji::session::session_manager::SessionType;
use kaji::session::SessionManager;
use kaji::workflow::events::WorkflowRecorder;
use kaji::workflow::{
    AgentRunRequest, AgentRunner, GateDecision, GateVerdict, RecordedOutcome, ReplayGates,
    ReplayRunner, StageState, WorkflowExecutor, WorkflowOutcome, WorkflowState,
};
use kaji_core::workflow::WorkflowSpec;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

/// Un DAG qui porte les trois issues de gate en une exécution : `publie` est
/// approuvée, `purge` refusée — `archive` tombe avec elle —, et `annonce`
/// attend encore quand l'annulation arrive.
const SPEC: &str = r#"
name: dore
stages:
  - name: recolte
    agents:
      - name: scan
        prompt: scanne
      - name: lint
        prompt: linte
  - name: publie
    depends_on: [recolte]
    gate: approve
    agents:
      - name: pousse
        prompt: pousse
        inputs:
          scan: "{{recolte.scan.output}}"
  - name: purge
    depends_on: [recolte]
    gate: approve
    agents:
      - name: efface
        prompt: efface
  - name: archive
    depends_on: [purge]
    agents:
      - name: range
        prompt: range
  - name: annonce
    depends_on: [publie]
    gate: approve
    agents:
      - name: crie
        prompt: crie
"#;

const DEADLINE: Duration = Duration::from_secs(5);

#[derive(Default)]
struct EchoRunner {
    started: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl AgentRunner for EchoRunner {
    async fn prepare(&self, request: &AgentRunRequest) -> Result<String, String> {
        Ok(format!("session_{}", request.label().replace('.', "_")))
    }

    async fn run(
        &self,
        request: AgentRunRequest,
        _session_id: &str,
        _cancel: CancellationToken,
    ) -> Result<String, String> {
        self.started.lock().unwrap().push(request.label());
        Ok(format!("[{}]", request.agent))
    }
}

/// Le runner du rejeu, doublé d'un témoin de lancement : tout passage par
/// `run` serait un sous-agent réel parti pendant un rejeu.
struct WitnessedReplayRunner {
    inner: ReplayRunner,
    launched: Mutex<Vec<String>>,
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

    async fn recorded_outcome(&self, request: &AgentRunRequest) -> Option<RecordedOutcome> {
        self.inner.recorded_outcome(request).await
    }
}

struct Harness {
    _data_dir: TempDir,
    session_manager: Arc<SessionManager>,
    session_id: String,
    working_dir: PathBuf,
}

impl Harness {
    async fn new() -> Self {
        let data_dir = tempfile::tempdir().unwrap();
        let session_manager = Arc::new(SessionManager::new(data_dir.path().join("data")));
        let working_dir = data_dir.path().join("workspace");
        let session = session_manager
            .create_session(
                working_dir.clone(),
                "dore".to_string(),
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
        WorkflowRecorder::open(
            Arc::clone(&self.session_manager),
            self.session_id.clone(),
            "dore",
        )
        .await
        .unwrap()
    }
}

/// Attend qu'un stage atteigne l'état voulu plutôt que de dormir un temps
/// choisi au doigt mouillé : c'est ce qui rend l'ordre des décisions
/// reproductible d'une machine à l'autre.
async fn wait_for_stage(
    handle: &kaji::workflow::WorkflowHandle,
    stage: &str,
    wanted: StageState,
) -> StageState {
    let poll = async {
        loop {
            let state = handle.snapshot().stage(stage).map(|s| s.state.clone());
            if state.as_ref() == Some(&wanted) {
                return wanted.clone();
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    tokio::time::timeout(DEADLINE, poll)
        .await
        .unwrap_or_else(|_| {
            panic!(
                "stage « {stage} » jamais {wanted:?} — vu {:?}",
                handle.snapshot().stage(stage).map(|s| s.state.clone())
            )
        })
}

fn stage_states(state: &WorkflowState) -> Vec<(String, StageState)> {
    state
        .stages
        .iter()
        .map(|stage| (stage.name.clone(), stage.state.clone()))
        .collect()
}

/// Rejoue le journal une fois et rend la topologie obtenue, après avoir
/// vérifié qu'aucun sous-agent n'est parti et qu'aucune décision vivante n'est
/// acceptée.
async fn replay_once(cursor: &EventCursor, spec: &WorkflowSpec) -> WorkflowState {
    let replayed = Harness::new().await;
    let witness = Arc::new(WitnessedReplayRunner {
        inner: ReplayRunner::from_cursor(cursor),
        launched: Mutex::new(Vec::new()),
    });
    let executor = WorkflowExecutor::replaying(
        spec.clone(),
        Arc::clone(&witness) as Arc<dyn AgentRunner>,
        Arc::new(ReplayGates::from_cursor(cursor)),
        cursor.workflow_recipes.clone(),
        replayed.recorder().await,
        replayed.session_id.clone(),
        replayed.working_dir.clone(),
    )
    .unwrap();
    for stage in ["publie", "purge", "annonce"] {
        assert_eq!(
            executor.handle().approve(stage),
            GateVerdict::Settled,
            "un rejeu n'accepte aucune décision vivante, pas même sur « {stage} »"
        );
    }

    let state = tokio::time::timeout(DEADLINE, executor.run())
        .await
        .expect("le rejeu n'attend aucune approbation — il les lit")
        .unwrap();

    assert!(
        witness.launched.lock().unwrap().is_empty(),
        "un sous-agent réel est parti pendant le rejeu : {:?}",
        witness.launched.lock().unwrap()
    );
    state
}

#[tokio::test]
async fn a_dag_with_an_approved_a_denied_and_a_cancelled_gate_replays_without_divergence() {
    let spec = WorkflowSpec::from_yaml(SPEC).unwrap();

    let recorded = Harness::new().await;
    let runner = Arc::new(EchoRunner::default());
    let executor = WorkflowExecutor::new(
        spec.clone(),
        Arc::clone(&runner) as Arc<dyn AgentRunner>,
        recorded.recorder().await,
        recorded.session_id.clone(),
        recorded.working_dir.clone(),
    )
    .unwrap();
    let handle = executor.handle();
    let run = tokio::spawn(executor.run());

    wait_for_stage(&handle, "publie", StageState::Waiting).await;
    wait_for_stage(&handle, "purge", StageState::Waiting).await;
    assert_eq!(handle.approve("publie"), GateVerdict::Applied);
    assert_eq!(handle.deny("purge"), GateVerdict::Applied);

    // L'annulation ne tombe qu'une fois `annonce` réellement suspendue à sa
    // gate : c'est le cas que le rejeu doit savoir relire, et le provoquer
    // plus tôt le remplacerait par une annulation avant démarrage.
    wait_for_stage(&handle, "annonce", StageState::Waiting).await;
    wait_for_stage(&handle, "archive", StageState::Cancelled).await;
    handle.cancel().await;

    let recorded_state = tokio::time::timeout(DEADLINE, run)
        .await
        .expect("cancel() réveille l'attente de gate")
        .unwrap()
        .unwrap();

    assert_eq!(recorded_state.outcome(), WorkflowOutcome::Cancelled);
    assert_eq!(
        stage_states(&recorded_state),
        vec![
            ("recolte".to_string(), StageState::Done),
            ("publie".to_string(), StageState::Done),
            ("purge".to_string(), StageState::Cancelled),
            ("archive".to_string(), StageState::Cancelled),
            ("annonce".to_string(), StageState::Cancelled),
        ]
    );
    assert_eq!(
        *runner.started.lock().unwrap(),
        vec![
            "recolte.scan".to_string(),
            "recolte.lint".to_string(),
            "publie.pousse".to_string()
        ],
        "aucun agent ne part derrière une gate refusée ou annulée"
    );

    let cursor = EventCursor::load(&recorded.session_manager, &recorded.session_id)
        .await
        .unwrap();
    assert_eq!(
        cursor.gate_decisions.get("publie"),
        Some(&GateDecision::Approve)
    );
    assert_eq!(
        cursor.gate_decisions.get("purge"),
        Some(&GateDecision::Deny)
    );
    assert_eq!(
        cursor.gate_decisions.get("annonce"),
        None,
        "une gate annulée ne laisse aucune décision : c'est tout l'écart que le rejeu doit combler"
    );
    assert!(
        cursor.cancelled,
        "le kind workflow_cancelled est la seule chose qui explique la gate manquante — \
         l'issue figée par workflow_done dirait « annulé » d'un simple refus de gate"
    );

    let first = replay_once(&cursor, &spec).await;
    let second = replay_once(&cursor, &spec).await;

    assert_eq!(
        first.topology(),
        recorded_state.topology(),
        "premier rejeu divergent"
    );
    assert_eq!(
        second.topology(),
        recorded_state.topology(),
        "second rejeu divergent"
    );
    assert_eq!(first.outcome(), WorkflowOutcome::Cancelled);

    assert_eq!(
        first
            .stage("publie")
            .unwrap()
            .agents
            .first()
            .and_then(|agent| agent.session_id.clone()),
        Some("session_publie_pousse".to_string()),
        "la session enfant enregistrée est resservie, pas recréée"
    );
}

/// Le pendant du doré : le même journal, privé de son issue annulée, redevient
/// une divergence. Sans ce test, « le rejeu rejoue l'annulation » pourrait
/// aussi bien vouloir dire « le rejeu tolère toute gate manquante ».
#[tokio::test]
async fn the_same_missing_gate_stays_a_divergence_outside_a_cancelled_run() {
    let spec = WorkflowSpec::from_yaml(SPEC).unwrap();
    let harness = Harness::new().await;
    let executor = WorkflowExecutor::replaying(
        spec,
        Arc::new(EchoRunner::default()) as Arc<dyn AgentRunner>,
        Arc::new(ReplayGates::new(
            BTreeMap::from([("publie".to_string(), GateDecision::Approve)])
                .into_iter()
                .collect(),
        )),
        Default::default(),
        harness.recorder().await,
        harness.session_id.clone(),
        harness.working_dir.clone(),
    )
    .unwrap();

    let state = tokio::time::timeout(DEADLINE, executor.run())
        .await
        .expect("un rejeu strict s'arrête, il n'attend pas")
        .unwrap();

    assert!(
        matches!(
            state.stage("purge").unwrap().state,
            StageState::Failed(kaji::workflow::FailureCause::Error(_))
        ),
        "hors annulation, la gate absente reste une divergence nommée : {:?}",
        state.stage("purge").unwrap().state
    );
    assert_ne!(state.outcome(), WorkflowOutcome::Cancelled);
}
