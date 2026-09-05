//! Les kinds v2 de l'orchestration, écrits sur la session **parente**.
//!
//! Six kinds structurels — `workflow_started`, `stage_started`,
//! `agent_started`, `agent_done`, `gate_decision`, `workflow_done` — décrivent
//! la topologie et les décisions : petits, permanents, ils sont l'historique du
//! workflow au même titre que `turn_start`. Un septième, `workflow_artifact`,
//! porte la sortie complète d'un agent : volumineux, purgeable, tranché du
//! `agent_done` qui n'en garde que les compteurs
//! (`replay::retention::PURGEABLE_KINDS`).
//!
//! Une seule de ces entrées est **servie** au rejeu : `gate_decision`. C'est la
//! seule qui entre dans le déroulé — une approbation humaine est de l'état
//! externe, et un rejeu qui la redemanderait ne serait plus hermétique. Les
//! autres décrivent ce qui s'est passé, elles ne le pilotent pas.
//!
//! Le workflow occupe un `turn_seq` unique sur la session parente et n'écrit
//! ni `turn_start` ni `turn_end` : il ne s'agit pas d'un tour d'agent, et une
//! borne ouverte ferait refuser le chargement du curseur.

use std::sync::Arc;

use anyhow::Result;
use kaji_core::workflow::{Gate, WorkflowSpec};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::warn;

use crate::session::SessionManager;
use crate::workflow::gate::GateDecision;
use crate::workflow::state::{AgentState, WorkflowState};

pub const WORKFLOW_STARTED: &str = "workflow_started";
pub const STAGE_STARTED: &str = "stage_started";
pub const AGENT_STARTED: &str = "agent_started";
pub const AGENT_DONE: &str = "agent_done";
pub const GATE_DECISION: &str = "gate_decision";
pub const WORKFLOW_ARTIFACT: &str = "workflow_artifact";
pub const WORKFLOW_DONE: &str = "workflow_done";

/// Les kinds de l'orchestration, dans l'ordre d'apparition d'une exécution.
pub const WORKFLOW_KINDS: [&str; 7] = [
    WORKFLOW_STARTED,
    STAGE_STARTED,
    AGENT_STARTED,
    GATE_DECISION,
    WORKFLOW_ARTIFACT,
    AGENT_DONE,
    WORKFLOW_DONE,
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowStarted {
    pub workflow: String,
    pub spec: WorkflowSpec,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageStarted {
    pub stage: String,
    pub gate: Gate,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentStarted {
    pub stage: String,
    pub agent: String,
    pub session_id: String,
    pub model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDone {
    pub stage: String,
    pub agent: String,
    pub session_id: Option<String>,
    pub state: AgentState,
    pub tokens: i64,
    pub duration_ms: i64,
}

/// La sortie d'un agent, seul payload volumineux de la famille — d'où sa place
/// dans les kinds purgeables et sa séparation d'avec `agent_done`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowArtifact {
    pub stage: String,
    pub agent: String,
    pub output: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateDecided {
    pub stage: String,
    pub decision: GateDecision,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowDone {
    pub workflow: String,
    pub state: WorkflowState,
}

/// Écrit les kinds de l'orchestration sur la session parente. Comme
/// `replay::record::RecordSink`, toute écriture est non fatale : un échec
/// marque la session non rejouable au lieu de faire tomber le workflow.
pub struct WorkflowRecorder {
    session_manager: Arc<SessionManager>,
    session_id: String,
    turn_seq: i64,
}

impl WorkflowRecorder {
    /// Pose `log_meta` avant de réserver le `turn_seq` : le méta vit au tour 1
    /// et le workflow prend le suivant, donc les deux ne se marchent pas dessus
    /// sur une session neuve.
    pub async fn open(session_manager: Arc<SessionManager>, session_id: String) -> Result<Self> {
        session_manager
            .append_log_meta_if_absent(&session_id)
            .await?;
        let turn_seq = session_manager.next_turn_seq(&session_id).await?;
        Ok(Self {
            session_manager,
            session_id,
            turn_seq,
        })
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn turn_seq(&self) -> i64 {
        self.turn_seq
    }

    async fn append<T: Serialize>(&self, kind: &str, payload: &T) {
        let payload_json = match serde_json::to_value(payload) {
            Ok(Value::Object(mut object)) => {
                object.insert("turn_seq".to_string(), Value::from(self.turn_seq));
                Value::Object(object).to_string()
            }
            Ok(other) => other.to_string(),
            Err(error) => {
                warn!(%error, kind, "workflow: payload non sérialisable");
                return;
            }
        };

        if let Err(error) = self
            .session_manager
            .append_event(&self.session_id, self.turn_seq, kind, &payload_json)
            .await
        {
            warn!(
                %error,
                kind,
                session_id = %self.session_id,
                "event log v2: écriture échouée — session marquée non rejouable"
            );
            if let Err(error) = self
                .session_manager
                .mark_not_replayable(&self.session_id)
                .await
            {
                warn!(%error, session_id = %self.session_id, "workflow: mark_not_replayable a aussi échoué");
            }
        }
    }

    pub async fn workflow_started(&self, spec: &WorkflowSpec) {
        self.append(
            WORKFLOW_STARTED,
            &WorkflowStarted {
                workflow: spec.name.clone(),
                spec: spec.clone(),
            },
        )
        .await;
    }

    pub async fn stage_started(&self, stage: &str, gate: Gate) {
        self.append(
            STAGE_STARTED,
            &StageStarted {
                stage: stage.to_string(),
                gate,
            },
        )
        .await;
    }

    pub async fn agent_started(
        &self,
        stage: &str,
        agent: &str,
        session_id: &str,
        model: &Option<String>,
    ) {
        self.append(
            AGENT_STARTED,
            &AgentStarted {
                stage: stage.to_string(),
                agent: agent.to_string(),
                session_id: session_id.to_string(),
                model: model.clone(),
            },
        )
        .await;
    }

    pub async fn agent_done(&self, done: &AgentDone) {
        self.append(AGENT_DONE, done).await;
    }

    pub async fn artifact(&self, stage: &str, agent: &str, output: &str) {
        self.append(
            WORKFLOW_ARTIFACT,
            &WorkflowArtifact {
                stage: stage.to_string(),
                agent: agent.to_string(),
                output: output.to_string(),
            },
        )
        .await;
    }

    pub async fn gate_decision(&self, stage: &str, decision: GateDecision) {
        self.append(
            GATE_DECISION,
            &GateDecided {
                stage: stage.to_string(),
                decision,
            },
        )
        .await;
    }

    pub async fn workflow_done(&self, state: &WorkflowState) {
        self.append(
            WORKFLOW_DONE,
            &WorkflowDone {
                workflow: state.workflow.clone(),
                state: state.clone(),
            },
        )
        .await;
    }
}
