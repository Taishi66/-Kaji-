//! L'exécuteur de workflow vu de l'extérieur du crate : l'API que T5 (CLI) et
//! T6 (mission-control) consommeront.
//!
//! Deux propriétés y sont clouées : une exécution enregistrée se rejoue avec
//! ses gates servies depuis le journal (aucune approbation redemandée), et le
//! runner de production rattache bien la session enfant à la session parente.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kaji::config::KajiMode;
use kaji::replay::cursor::EventCursor;
use kaji::session::session_manager::SessionType;
use kaji::session::SessionManager;
use kaji::workflow::events::WorkflowRecorder;
use kaji::workflow::{
    AgentRunRequest, AgentRunner, GateDecision, GateVerdict, ReplayGates, ReplayRunner,
    SubagentRunner, WorkflowExecutor, WorkflowOutcome,
};
use kaji_core::workflow::WorkflowSpec;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

const SPEC: &str = r#"
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
        inputs:
          artefact: "{{build.compile.output}}"
"#;

/// Rend le nom de l'agent et note ce qu'il a reçu : assez pour prouver l'ordre
/// et la substitution sans provider ni boucle agent.
#[derive(Default)]
struct EchoRunner {
    started: Mutex<Vec<String>>,
    inputs: Mutex<Vec<(String, BTreeMap<String, String>)>>,
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
        self.inputs
            .lock()
            .unwrap()
            .push((request.label(), request.inputs.clone()));
        Ok(format!("[{}]", request.agent))
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

    async fn recorder(&self, workflow: &str) -> WorkflowRecorder {
        WorkflowRecorder::open(
            Arc::clone(&self.session_manager),
            self.session_id.clone(),
            workflow,
        )
        .await
        .unwrap()
    }
}

#[tokio::test]
async fn a_recorded_workflow_replays_hermetically_from_its_log() {
    let spec = WorkflowSpec::from_yaml(SPEC).unwrap();

    let recorded = Harness::new().await;
    let runner = Arc::new(EchoRunner::default());
    let executor = WorkflowExecutor::new(
        spec.clone(),
        Arc::clone(&runner) as Arc<dyn AgentRunner>,
        recorded.recorder("livraison").await,
        recorded.session_id.clone(),
        recorded.working_dir.clone(),
    )
    .unwrap();
    let handle = executor.handle();
    let run = tokio::spawn(executor.run());
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(handle.approve("deploie"), GateVerdict::Applied);
    let recorded_state = run.await.unwrap().unwrap();
    assert_eq!(recorded_state.outcome(), WorkflowOutcome::Done);
    assert_eq!(
        runner.inputs.lock().unwrap()[1].1,
        HashMap::from([("artefact".to_string(), "[compile]".to_string())])
            .into_iter()
            .collect::<BTreeMap<_, _>>(),
        "la sortie de l'ancêtre est substituée dans l'entrée du descendant"
    );

    let cursor = EventCursor::load(&recorded.session_manager, &recorded.session_id)
        .await
        .unwrap();
    assert_eq!(
        cursor.gate_decisions.get("deploie"),
        Some(&GateDecision::Approve),
        "la décision de gate est capturée dans le journal de la session parente"
    );
    assert_eq!(
        cursor
            .workflow_artifacts
            .get(&("build".to_string(), "compile".to_string()))
            .map(String::as_str),
        Some("[compile]"),
        "la sortie de l'agent est journalisée sous son kind purgeable"
    );

    // Le rejeu tourne sur `ReplayRunner` : gates, sorties et issues viennent
    // du journal, aucun sous-agent ne part. Le runner vivant sert de témoin —
    // s'il est sollicité, le rejeu n'était pas hermétique.
    let replayed = Harness::new().await;
    let live = Arc::new(EchoRunner::default());
    let executor = WorkflowExecutor::replaying(
        cursor.workflow.clone().unwrap().spec,
        Arc::new(ReplayRunner::from_cursor(&cursor)),
        Arc::new(ReplayGates::from_cursor(&cursor)),
        cursor.workflow_recipes.clone(),
        replayed.recorder("livraison").await,
        replayed.session_id.clone(),
        replayed.working_dir.clone(),
    )
    .unwrap();
    assert_eq!(
        executor.handle().approve("deploie"),
        GateVerdict::Settled,
        "un rejeu n'accepte pas de décision vivante"
    );

    let state = tokio::time::timeout(Duration::from_secs(5), executor.run())
        .await
        .expect("le rejeu n'attend aucune approbation")
        .unwrap();

    // `duration_ms` est une horloge murale, pas un invariant de rejeu : la
    // comparaison porte sur la topologie et les états.
    assert_eq!(state.topology(), recorded_state.topology());
    assert!(
        live.started.lock().unwrap().is_empty(),
        "aucun sous-agent réel ne part au rejeu : {:?}",
        live.started.lock().unwrap()
    );
    assert_eq!(
        state
            .stage("deploie")
            .unwrap()
            .agents
            .first()
            .and_then(|agent| agent.session_id.clone()),
        Some("session_deploie_pousse".to_string()),
        "la session enfant enregistrée est resservie, pas recréée"
    );
}

#[tokio::test]
async fn the_production_runner_links_the_child_session_to_the_workflow_session() {
    let harness = Harness::new().await;
    let runner = SubagentRunner::new(Arc::clone(&harness.session_manager));

    let child = runner
        .prepare(&AgentRunRequest {
            stage: "build".to_string(),
            agent: "compile".to_string(),
            source: kaji_core::workflow::AgentSource::Prompt("compile".to_string()),
            model: None,
            inputs: BTreeMap::new(),
            recipe: None,
            parent_session_id: harness.session_id.clone(),
            working_dir: harness.working_dir.clone(),
        })
        .await
        .unwrap();

    let session = harness
        .session_manager
        .get_session(&child, false)
        .await
        .unwrap();
    assert_eq!(
        session.parent_session_id.as_deref(),
        Some(harness.session_id.as_str())
    );
    assert_eq!(session.session_type, SessionType::SubAgent);
    assert_eq!(session.name, "build.compile");
}
