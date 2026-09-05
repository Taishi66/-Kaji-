//! Ce qu'un journal rend d'un workflow après coup.
//!
//! `kaji workflow list` et `kaji workflow status` ne lisent pas l'exécuteur —
//! le run peut être fini, ou tourner dans un autre processus : ils relisent
//! les kinds v2 de la session parente. `workflow_done` fait autorité quand il
//! est là, parce qu'il porte les états de stage que le journal n'émet nulle
//! part ailleurs (une cascade d'annulation n'a pas d'événement propre) ;
//! sinon l'état est **reconstruit** événement par événement, ce qui est la
//! seule façon de voir un run encore en vol ou tué.

use std::collections::BTreeMap;

use anyhow::Result;
use kaji_core::workflow::{Gate, WorkflowSpec};

use crate::session::session_manager::SessionEvent;
use crate::session::SessionManager;
use crate::workflow::events::{
    AgentDone, AgentStarted, GateDecided, StageStarted, WorkflowDone, WorkflowStarted, AGENT_DONE,
    AGENT_STARTED, GATE_DECISION, STAGE_STARTED, WORKFLOW_DONE, WORKFLOW_STARTED,
};
use crate::workflow::gate::GateDecision;
use crate::workflow::state::{AgentState, StageState, WorkflowOutcome, WorkflowState};

/// Un workflow tel que son journal le raconte.
pub struct WorkflowRun {
    /// La session parente — celle que `kaji replay` rejoue.
    pub session_id: String,
    pub workflow: String,
    pub started_at_ms: i64,
    pub spec: WorkflowSpec,
    pub state: WorkflowState,
    pub gates: BTreeMap<String, GateDecision>,
    /// `workflow_done` est au journal. Faux pour un run en vol **comme** pour
    /// un run tué : les deux se distinguent par la fraîcheur de leurs
    /// événements, pas par leur état.
    pub finished: bool,
}

impl WorkflowRun {
    /// `None` quand la session ne porte pas de workflow : c'est ce qui
    /// distingue une session de workflow d'une session de conversation.
    pub fn from_events(session_id: &str, events: &[SessionEvent]) -> Option<Self> {
        let started = events.iter().find(|event| event.kind == WORKFLOW_STARTED)?;
        let payload: WorkflowStarted = serde_json::from_str(&started.payload_json).ok()?;

        let mut run = Self {
            session_id: session_id.to_string(),
            workflow: payload.workflow,
            started_at_ms: started.ts_ms,
            state: WorkflowState::from_spec(&payload.spec),
            spec: payload.spec,
            gates: BTreeMap::new(),
            finished: false,
        };
        for event in events {
            run.apply(event);
        }
        Some(run)
    }

    /// Les stages qui attendent une décision humaine — ce que `status`
    /// annonce et ce que le mission-control met en évidence.
    pub fn pending_gates(&self) -> Vec<&str> {
        self.state
            .stages
            .iter()
            .filter(|stage| stage.state == StageState::Waiting)
            .map(|stage| stage.name.as_str())
            .collect()
    }

    /// L'issue d'ensemble, seulement quand le workflow a conclu : sur un run
    /// inachevé, `WorkflowState::outcome` rendrait « échoué » d'un stage qui
    /// tourne encore.
    pub fn outcome(&self) -> Option<WorkflowOutcome> {
        self.finished.then(|| self.state.outcome())
    }

    fn apply(&mut self, event: &SessionEvent) {
        match event.kind.as_str() {
            STAGE_STARTED => {
                let Ok(payload) = serde_json::from_str::<StageStarted>(&event.payload_json) else {
                    return;
                };
                let opened = match payload.gate {
                    Gate::Approve => StageState::Waiting,
                    Gate::Auto => StageState::Running,
                };
                if let Some(stage) = self.stage_mut(&payload.stage) {
                    stage.state = opened;
                }
            }
            GATE_DECISION => {
                let Ok(payload) = serde_json::from_str::<GateDecided>(&event.payload_json) else {
                    return;
                };
                self.gates.insert(payload.stage.clone(), payload.decision);
                let approved = payload.decision.approved();
                if let Some(stage) = self.stage_mut(&payload.stage) {
                    if approved {
                        stage.state = StageState::Running;
                    } else {
                        stage.state = StageState::Cancelled;
                        for agent in &mut stage.agents {
                            if !agent.state.is_terminal() {
                                agent.state = AgentState::Cancelled;
                            }
                        }
                    }
                }
            }
            AGENT_STARTED => {
                let Ok(payload) = serde_json::from_str::<AgentStarted>(&event.payload_json) else {
                    return;
                };
                if let Some(stage) = self.stage_mut(&payload.stage) {
                    if let Some(agent) = stage
                        .agents
                        .iter_mut()
                        .find(|agent| agent.name == payload.agent)
                    {
                        agent.state = AgentState::Running;
                        agent.session_id = Some(payload.session_id);
                    }
                }
            }
            AGENT_DONE => {
                let Ok(payload) = serde_json::from_str::<AgentDone>(&event.payload_json) else {
                    return;
                };
                let Some(stage) = self.stage_mut(&payload.stage) else {
                    return;
                };
                if let Some(agent) = stage
                    .agents
                    .iter_mut()
                    .find(|agent| agent.name == payload.agent)
                {
                    agent.state = payload.state;
                    agent.tokens = payload.tokens;
                    agent.duration_ms = payload.duration_ms;
                    if payload.session_id.is_some() {
                        agent.session_id = payload.session_id;
                    }
                }
                let agents: Vec<AgentState> = stage
                    .agents
                    .iter()
                    .map(|agent| agent.state.clone())
                    .collect();
                if let Some(derived) = StageState::from_agents(&agents) {
                    stage.state = derived;
                }
            }
            WORKFLOW_DONE => {
                if let Ok(payload) = serde_json::from_str::<WorkflowDone>(&event.payload_json) {
                    self.state = payload.state;
                    self.finished = true;
                }
            }
            _ => {}
        }
    }

    fn stage_mut(&mut self, name: &str) -> Option<&mut crate::workflow::state::StageStatus> {
        self.state
            .stages
            .iter_mut()
            .find(|stage| stage.name == name)
    }
}

/// Tous les workflows **lancés**, du plus récent au plus ancien.
///
/// Les sessions dérivées en sont exclues : `kaji replay` redéroule le DAG d'un
/// run sur une session fille, qui porte donc elle aussi un `workflow_started`.
/// La lister ferait apparaître chaque rejeu comme un run de plus. Une session
/// de workflow n'a jamais de parent — `kaji workflow run` la crée — donc le
/// parent suffit à trancher. [`find_workflow_run`] ne filtre rien : inspecter
/// un rejeu par son identifiant reste possible.
pub async fn list_workflow_runs(session_manager: &SessionManager) -> Result<Vec<WorkflowRun>> {
    let mut runs = Vec::new();
    for session_id in session_manager
        .sessions_with_event_kind(WORKFLOW_STARTED)
        .await?
    {
        let derived = session_manager
            .get_session(&session_id, false)
            .await
            .is_ok_and(|session| session.parent_session_id.is_some());
        if derived {
            continue;
        }
        let events = session_manager.session_events(&session_id).await?;
        if let Some(run) = WorkflowRun::from_events(&session_id, &events) {
            runs.push(run);
        }
    }
    Ok(runs)
}

/// Le run d'une session donnée, `None` si elle n'en porte pas.
pub async fn find_workflow_run(
    session_manager: &SessionManager,
    session_id: &str,
) -> Result<Option<WorkflowRun>> {
    let events = session_manager.session_events(session_id).await?;
    Ok(WorkflowRun::from_events(session_id, &events))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPEC: &str = r#"
name: revue
stages:
  - name: collecte
    agents:
      - name: scan
        prompt: scanne
  - name: deploie
    depends_on: [collecte]
    gate: approve
    agents:
      - name: pousse
        prompt: pousse
"#;

    fn spec() -> WorkflowSpec {
        WorkflowSpec::from_yaml(SPEC).unwrap()
    }

    fn event(id: i64, kind: &str, payload: serde_json::Value) -> SessionEvent {
        SessionEvent {
            id,
            turn_seq: 2,
            ts_ms: 1_000 + id,
            kind: kind.to_string(),
            payload_json: payload.to_string(),
        }
    }

    /// Le préfixe d'événements d'un run arrivé jusqu'à sa gate : `collecte` a
    /// tourné, `deploie` a ouvert la sienne et attend.
    fn events_up_to_the_gate() -> Vec<SessionEvent> {
        vec![
            event(
                1,
                WORKFLOW_STARTED,
                serde_json::json!({"workflow": "revue", "spec": spec()}),
            ),
            event(
                2,
                STAGE_STARTED,
                serde_json::json!({"stage": "collecte", "gate": "auto"}),
            ),
            event(
                3,
                AGENT_STARTED,
                serde_json::json!({
                    "stage": "collecte",
                    "agent": "scan",
                    "session_id": "enfant-1",
                    "model": null,
                }),
            ),
            event(
                4,
                AGENT_DONE,
                serde_json::json!({
                    "stage": "collecte",
                    "agent": "scan",
                    "session_id": "enfant-1",
                    "state": "done",
                    "tokens": 120,
                    "duration_ms": 800,
                }),
            ),
            event(
                5,
                STAGE_STARTED,
                serde_json::json!({"stage": "deploie", "gate": "approve"}),
            ),
        ]
    }

    #[test]
    fn a_run_still_in_flight_is_rebuilt_from_its_events() {
        let run = WorkflowRun::from_events("parent-1", &events_up_to_the_gate()).unwrap();

        assert_eq!(run.workflow, "revue");
        assert_eq!(run.session_id, "parent-1");
        assert_eq!(run.started_at_ms, 1_001);
        assert!(!run.finished, "aucun workflow_done au journal");

        let collecte = run.state.stage("collecte").unwrap();
        assert_eq!(collecte.state, StageState::Done);
        assert_eq!(collecte.agents[0].state, AgentState::Done);
        assert_eq!(collecte.agents[0].tokens, 120);
        assert_eq!(collecte.agents[0].session_id.as_deref(), Some("enfant-1"));

        assert_eq!(
            run.state.stage("deploie").unwrap().state,
            StageState::Waiting
        );
        assert_eq!(run.pending_gates(), vec!["deploie"]);
        assert!(
            run.outcome().is_none(),
            "un run inachevé n'a pas d'issue à annoncer"
        );
    }

    /// L'état figé par `workflow_done` fait autorité : il porte les états de
    /// stage que le journal n'émet nulle part ailleurs (une cascade
    /// d'annulation n'a pas d'événement propre).
    #[test]
    fn a_finished_run_takes_the_state_frozen_by_workflow_done() {
        let mut events = events_up_to_the_gate();
        events.push(event(
            6,
            GATE_DECISION,
            serde_json::json!({"stage": "deploie", "decision": "deny"}),
        ));
        let mut final_state = WorkflowState::from_spec(&spec());
        final_state.stages[0].state = StageState::Done;
        final_state.stages[0].agents[0].state = AgentState::Done;
        final_state.stages[1].state = StageState::Cancelled;
        final_state.stages[1].agents[0].state = AgentState::Cancelled;
        events.push(event(
            7,
            WORKFLOW_DONE,
            serde_json::json!({"workflow": "revue", "state": final_state}),
        ));

        let run = WorkflowRun::from_events("parent-1", &events).unwrap();

        assert!(run.finished);
        assert_eq!(run.outcome(), Some(WorkflowOutcome::Cancelled));
        assert_eq!(
            run.gates.get("deploie").copied(),
            Some(GateDecision::Deny),
            "la décision reste lisible après coup"
        );
        assert!(
            run.pending_gates().is_empty(),
            "un run fini n'attend plus rien"
        );
    }

    /// Une gate refusée annule son stage avant même le `workflow_done` : c'est
    /// ce que `status` doit montrer d'un run tué entre les deux.
    #[test]
    fn a_denied_gate_cancels_its_stage_without_waiting_for_workflow_done() {
        let mut events = events_up_to_the_gate();
        events.push(event(
            6,
            GATE_DECISION,
            serde_json::json!({"stage": "deploie", "decision": "deny"}),
        ));

        let run = WorkflowRun::from_events("parent-1", &events).unwrap();

        assert_eq!(
            run.state.stage("deploie").unwrap().state,
            StageState::Cancelled
        );
        assert!(run.pending_gates().is_empty());
    }

    #[test]
    fn a_session_without_workflow_started_is_not_a_run() {
        let events = vec![event(1, "turn_start", serde_json::json!({}))];
        assert!(WorkflowRun::from_events("parent-1", &events).is_none());
    }
}
