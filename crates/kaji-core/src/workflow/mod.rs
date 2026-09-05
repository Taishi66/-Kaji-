//! Orchestration déclarative (S3) : la spec YAML et sa validation vivent ici,
//! l'exécuteur DAG les consomme depuis le crate `kaji`.

pub mod spec;

pub use spec::{
    AgentSource, AgentSpec, Budgets, Gate, InputReference, InputTarget, Stage, WorkflowSpec,
    WorkflowSpecError,
};
