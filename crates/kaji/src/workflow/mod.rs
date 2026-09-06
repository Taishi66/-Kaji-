//! Exécution d'un workflow déclaratif (S3) : ordonnanceur DAG au-dessus du
//! primitif de spawn de summon, journalisé en kinds v2 sur la session parente.
//!
//! La spec YAML et sa validation vivent dans `kaji_core::workflow` ; ce module
//! ne consomme qu'une spec **déjà validée**.

pub mod artifacts;
pub mod events;
pub mod executor;
pub mod gate;
pub mod registry;
pub mod runner;
pub mod state;

#[cfg(test)]
mod tests;

pub use executor::{WorkflowExecutor, WorkflowHandle, CANCEL_GRACE, SHUTDOWN_GRACE};
pub use gate::{GateDecision, GateOutcome, GateSource, GateVerdict, LiveGates, ReplayGates};
pub use registry::{find_workflow_run, list_workflow_runs, WorkflowRun};
pub use runner::{
    AgentRunRequest, AgentRunner, RecordedOutcome, ReplayRunner, ResolvedRecipe, SubagentRunner,
};
pub use state::{
    AgentState, AgentStatus, BudgetLimit, FailureCause, StageState, StageStatus, WorkflowOutcome,
    WorkflowState,
};
