//! L'état vivant d'un workflow : ce que le mission-control affiche et ce que
//! `workflow_done` fige dans le journal.
//!
//! Les états sont sérialisés en externally-tagged snake_case, donc
//! `"pending"` pour un état simple et `{"failed":{"budget":"duration"}}` pour
//! un échec : la cause voyage avec l'état plutôt que dans un champ parallèle
//! qu'on peut oublier de lire.

use kaji_core::workflow::Gate;
use serde::{Deserialize, Serialize};

/// Le budget qui a coupé l'agent. Nommé par le champ de la spec qui le porte,
/// pour que le message rendu à l'utilisateur pointe la ligne YAML à corriger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetLimit {
    Tokens,
    Duration,
}

impl BudgetLimit {
    pub fn field(self) -> &'static str {
        match self {
            BudgetLimit::Tokens => "max_tokens",
            BudgetLimit::Duration => "max_duration_s",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureCause {
    Budget(BudgetLimit),
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    Pending,
    Running,
    Done,
    Failed(FailureCause),
    Cancelled,
}

impl AgentState {
    pub fn is_terminal(&self) -> bool {
        !matches!(self, AgentState::Pending | AgentState::Running)
    }

    pub fn label(&self) -> &'static str {
        match self {
            AgentState::Pending => "en attente",
            AgentState::Running => "en cours",
            AgentState::Done => "terminé",
            AgentState::Failed(_) => "échoué",
            AgentState::Cancelled => "annulé",
        }
    }
}

/// `Waiting` est l'attente d'une décision de gate, pas une attente de
/// dépendance : un stage dont les dépendances ne sont pas finies reste
/// `Pending`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StageState {
    Pending,
    Running,
    Waiting,
    Done,
    Failed(FailureCause),
    Cancelled,
}

impl StageState {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            StageState::Done | StageState::Failed(_) | StageState::Cancelled
        )
    }

    pub fn label(&self) -> &'static str {
        match self {
            StageState::Pending => "en attente",
            StageState::Running => "en cours",
            StageState::Waiting => "gate",
            StageState::Done => "terminé",
            StageState::Failed(_) => "échoué",
            StageState::Cancelled => "annulé",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentStatus {
    pub name: String,
    pub state: AgentState,
    pub session_id: Option<String>,
    pub tokens: i64,
    pub duration_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageStatus {
    pub name: String,
    pub state: StageState,
    pub gate: Gate,
    pub agents: Vec<AgentStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowState {
    pub workflow: String,
    pub stages: Vec<StageStatus>,
}

impl WorkflowState {
    pub fn from_spec(spec: &kaji_core::workflow::WorkflowSpec) -> Self {
        Self {
            workflow: spec.name.clone(),
            stages: spec
                .stages
                .iter()
                .map(|stage| StageStatus {
                    name: stage.name.clone(),
                    state: StageState::Pending,
                    gate: stage.gate,
                    agents: stage
                        .agents
                        .iter()
                        .map(|agent| AgentStatus {
                            name: agent.name.clone(),
                            state: AgentState::Pending,
                            session_id: None,
                            tokens: 0,
                            duration_ms: 0,
                        })
                        .collect(),
                })
                .collect(),
        }
    }

    pub fn stage(&self, name: &str) -> Option<&StageStatus> {
        self.stages.iter().find(|stage| stage.name == name)
    }

    /// L'état d'ensemble : `Failed` dès qu'un stage a échoué, `Cancelled` si
    /// l'un a été annulé (gate refusée, annulation), `Done` sinon.
    pub fn outcome(&self) -> StageState {
        if let Some(failed) = self
            .stages
            .iter()
            .find(|stage| matches!(stage.state, StageState::Failed(_)))
        {
            return failed.state.clone();
        }
        if self
            .stages
            .iter()
            .any(|stage| stage.state == StageState::Cancelled)
        {
            return StageState::Cancelled;
        }
        StageState::Done
    }
}
