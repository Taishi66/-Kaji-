//! L'ordonnanceur DAG.
//!
//! L'ordre d'exécution vient du graphe `depends_on`, jamais de la position des
//! stages dans le document : un stage part dès que **toutes** ses dépendances
//! sont `Done`, et deux stages sans lien partent ensemble. Les agents d'un même
//! stage partent tous en même temps (fan-out).
//!
//! L'exécuteur vit hors des deux boucles agent : il pilote des sessions, il
//! n'en est pas une. Ses points de contact avec la boucle sont les sites déjà
//! partagés — `session_manager::append_event` pour le journal,
//! `run_subagent_task` pour le spawn — donc rien à appliquer deux fois côté
//! legacy/machine à états.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use futures::stream::{FuturesUnordered, StreamExt};
use kaji_core::workflow::{AgentSpec, Gate, WorkflowSpec};
use tokio_util::sync::CancellationToken;

use crate::workflow::artifacts::Artifacts;
use crate::workflow::events::{AgentDone, WorkflowRecorder};
use crate::workflow::gate::{GateDecision, GateSource, LiveGates};
use crate::workflow::runner::{AgentRunRequest, AgentRunner};
use crate::workflow::state::{AgentState, BudgetLimit, FailureCause, StageState, WorkflowState};

/// Période de relecture de l'usage de la session enfant pour le budget
/// `max_tokens`. Le dépassement se constate donc à un demi-tick près : un
/// budget de tokens borne le coût, il ne le coupe pas au token exact.
const TOKEN_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Délai laissé à un agent pour s'arrêter proprement après l'annulation, avant
/// que l'exécuteur passe à la suite sans lui.
const CANCEL_GRACE: Duration = Duration::from_secs(5);

struct Shared {
    state: Mutex<WorkflowState>,
    artifacts: Mutex<Artifacts>,
    cancel: CancellationToken,
    live_gates: Option<Arc<LiveGates>>,
}

/// La prise de contrôle d'un workflow en vol : approuver/refuser une gate,
/// annuler, lire l'état. T5 (CLI) et T6 (mission-control) ne consomment que
/// ça.
#[derive(Clone)]
pub struct WorkflowHandle {
    shared: Arc<Shared>,
}

impl WorkflowHandle {
    pub fn snapshot(&self) -> WorkflowState {
        self.shared.state.lock().expect("état empoisonné").clone()
    }

    /// `false` quand le workflow ne prend pas de décision vivante — un rejeu
    /// sert ses gates depuis le journal et n'attend personne.
    pub fn approve(&self, stage: &str) -> bool {
        self.decide(stage, GateDecision::Approve)
    }

    pub fn deny(&self, stage: &str) -> bool {
        self.decide(stage, GateDecision::Deny)
    }

    fn decide(&self, stage: &str, decision: GateDecision) -> bool {
        let Some(gates) = self.shared.live_gates.as_ref() else {
            return false;
        };
        gates.record(stage, decision);
        true
    }

    pub fn cancel(&self) {
        self.shared.cancel.cancel();
    }
}

pub struct WorkflowExecutor {
    spec: WorkflowSpec,
    runner: Arc<dyn AgentRunner>,
    gates: Arc<dyn GateSource>,
    recorder: WorkflowRecorder,
    parent_session_id: String,
    working_dir: PathBuf,
    shared: Arc<Shared>,
}

impl WorkflowExecutor {
    /// Exécution vivante : les gates attendent une décision de
    /// [`WorkflowHandle`].
    pub fn new(
        spec: WorkflowSpec,
        runner: Arc<dyn AgentRunner>,
        recorder: WorkflowRecorder,
        parent_session_id: String,
        working_dir: PathBuf,
    ) -> Self {
        let live_gates = Arc::new(LiveGates::default());
        let gates = Arc::clone(&live_gates) as Arc<dyn GateSource>;
        Self::build(
            spec,
            runner,
            gates,
            Some(live_gates),
            recorder,
            parent_session_id,
            working_dir,
        )
    }

    /// Rejeu : les gates viennent du journal, aucune approbation n'est
    /// redemandée.
    pub fn replaying(
        spec: WorkflowSpec,
        runner: Arc<dyn AgentRunner>,
        gates: Arc<dyn GateSource>,
        recorder: WorkflowRecorder,
        parent_session_id: String,
        working_dir: PathBuf,
    ) -> Self {
        Self::build(
            spec,
            runner,
            gates,
            None,
            recorder,
            parent_session_id,
            working_dir,
        )
    }

    fn build(
        spec: WorkflowSpec,
        runner: Arc<dyn AgentRunner>,
        gates: Arc<dyn GateSource>,
        live_gates: Option<Arc<LiveGates>>,
        recorder: WorkflowRecorder,
        parent_session_id: String,
        working_dir: PathBuf,
    ) -> Self {
        let shared = Arc::new(Shared {
            state: Mutex::new(WorkflowState::from_spec(&spec)),
            artifacts: Mutex::new(Artifacts::default()),
            cancel: CancellationToken::new(),
            live_gates,
        });
        Self {
            spec,
            runner,
            gates,
            recorder,
            parent_session_id,
            working_dir,
            shared,
        }
    }

    pub fn handle(&self) -> WorkflowHandle {
        WorkflowHandle {
            shared: Arc::clone(&self.shared),
        }
    }

    pub async fn run(self) -> Result<WorkflowState> {
        self.recorder.workflow_started(&self.spec).await;

        let indexes: HashMap<&str, usize> = self
            .spec
            .stages
            .iter()
            .enumerate()
            .map(|(index, stage)| (stage.name.as_str(), index))
            .collect();

        let mut pending: BTreeSet<usize> = (0..self.spec.stages.len()).collect();
        let mut finished: HashSet<usize> = HashSet::new();
        let mut running = FuturesUnordered::new();

        loop {
            let ready: Vec<usize> = pending
                .iter()
                .copied()
                .filter(|index| self.dependencies_met(*index, &indexes, &finished))
                .collect();
            for index in &ready {
                pending.remove(index);
                running.push(self.run_stage(*index));
            }

            let Some((index, state)) = running.next().await else {
                break;
            };
            if state == StageState::Done {
                finished.insert(index);
            } else {
                self.cancel_descendants(index, &indexes, &mut pending);
            }
        }

        let state = self.snapshot();
        self.recorder.workflow_done(&state).await;
        Ok(state)
    }

    fn dependencies_met(
        &self,
        index: usize,
        indexes: &HashMap<&str, usize>,
        finished: &HashSet<usize>,
    ) -> bool {
        self.spec.stages[index].depends_on.iter().all(|dependency| {
            indexes
                .get(dependency.as_str())
                .is_some_and(|dependency| finished.contains(dependency))
        })
    }

    /// Un stage qui n'aboutit pas — gate refusée, échec, annulation — emporte
    /// tout ce qui en descend : la fermeture transitive se calcule sur les
    /// stages encore en attente, jamais sur ceux qui tournent déjà.
    fn cancel_descendants(
        &self,
        root: usize,
        indexes: &HashMap<&str, usize>,
        pending: &mut BTreeSet<usize>,
    ) {
        let mut cancelled: HashSet<usize> = HashSet::from([root]);
        loop {
            let next: Vec<usize> = pending
                .iter()
                .copied()
                .filter(|index| {
                    self.spec.stages[*index]
                        .depends_on
                        .iter()
                        .any(|dependency| {
                            indexes
                                .get(dependency.as_str())
                                .is_some_and(|dependency| cancelled.contains(dependency))
                        })
                })
                .collect();
            if next.is_empty() {
                return;
            }
            for index in next {
                pending.remove(&index);
                cancelled.insert(index);
                self.set_stage(index, StageState::Cancelled);
                self.cancel_stage_agents(index);
            }
        }
    }

    async fn run_stage(&self, index: usize) -> (usize, StageState) {
        let stage = &self.spec.stages[index];

        if self.shared.cancel.is_cancelled() {
            self.set_stage(index, StageState::Cancelled);
            self.cancel_stage_agents(index);
            return (index, StageState::Cancelled);
        }

        self.set_stage(index, StageState::Running);
        self.recorder.stage_started(&stage.name, stage.gate).await;

        if stage.gate == Gate::Approve {
            self.set_stage(index, StageState::Waiting);
            let decision = tokio::select! {
                decision = self.gates.decide(&stage.name) => decision,
                _ = self.shared.cancel.cancelled() => {
                    self.set_stage(index, StageState::Cancelled);
                    self.cancel_stage_agents(index);
                    return (index, StageState::Cancelled);
                }
            };
            let decision = match decision {
                Ok(decision) => decision,
                Err(error) => {
                    let state = StageState::Failed(FailureCause::Error(error.to_string()));
                    self.set_stage(index, state.clone());
                    self.cancel_stage_agents(index);
                    return (index, state);
                }
            };
            self.recorder.gate_decision(&stage.name, decision).await;
            if !decision.approved() {
                self.set_stage(index, StageState::Cancelled);
                self.cancel_stage_agents(index);
                return (index, StageState::Cancelled);
            }
            self.set_stage(index, StageState::Running);
        }

        let outcomes = futures::future::join_all(
            stage
                .agents
                .iter()
                .map(|agent| self.run_agent(index, agent)),
        )
        .await;

        let state = outcomes
            .iter()
            .find_map(|outcome| match outcome {
                AgentState::Failed(cause) => Some(StageState::Failed(cause.clone())),
                AgentState::Cancelled => Some(StageState::Cancelled),
                _ => None,
            })
            .unwrap_or(StageState::Done);

        self.set_stage(index, state.clone());
        (index, state)
    }

    async fn run_agent(&self, stage_index: usize, agent: &AgentSpec) -> AgentState {
        let stage = &self.spec.stages[stage_index];
        let request = AgentRunRequest {
            stage: stage.name.clone(),
            agent: agent.name.clone(),
            source: agent.source.clone(),
            model: agent.model.clone(),
            inputs: self.substituted_inputs(agent),
            parent_session_id: self.parent_session_id.clone(),
            working_dir: self.working_dir.clone(),
        };

        self.set_agent(stage_index, &agent.name, AgentState::Running);

        let session_id = match self.runner.prepare(&request).await {
            Ok(session_id) => session_id,
            Err(error) => {
                let state = AgentState::Failed(FailureCause::Error(error));
                self.finish_agent(stage_index, agent, None, state.clone(), 0, 0)
                    .await;
                return state;
            }
        };
        self.set_agent_session(stage_index, &agent.name, &session_id);
        self.recorder
            .agent_started(&stage.name, &agent.name, &session_id, &agent.model)
            .await;

        let started = Instant::now();
        let cancel = self.shared.cancel.child_token();
        let mut run = Box::pin(self.runner.run(request, &session_id, cancel.clone()));

        let interrupted = tokio::select! {
            result = &mut run => Ok(result),
            _ = duration_guard(stage.budgets.max_duration_s) => Err(Interrupt::Budget(BudgetLimit::Duration)),
            _ = token_guard(self.runner.as_ref(), &session_id, stage.budgets.max_tokens) => {
                Err(Interrupt::Budget(BudgetLimit::Tokens))
            }
            _ = self.shared.cancel.cancelled() => Err(Interrupt::Cancelled),
        };

        let state = match interrupted {
            Ok(Ok(output)) => {
                self.shared
                    .artifacts
                    .lock()
                    .expect("artefacts empoisonnés")
                    .insert(&stage.name, &agent.name, output.clone());
                self.recorder
                    .artifact(&stage.name, &agent.name, &output)
                    .await;
                AgentState::Done
            }
            Ok(Err(error)) => AgentState::Failed(FailureCause::Error(error)),
            Err(interrupt) => {
                cancel.cancel();
                let _ = tokio::time::timeout(CANCEL_GRACE, &mut run).await;
                match interrupt {
                    Interrupt::Budget(limit) => AgentState::Failed(FailureCause::Budget(limit)),
                    Interrupt::Cancelled => AgentState::Cancelled,
                }
            }
        };

        let tokens = self.runner.tokens_used(&session_id).await;
        let duration_ms = started.elapsed().as_millis() as i64;
        self.finish_agent(
            stage_index,
            agent,
            Some(session_id.clone()),
            state.clone(),
            tokens,
            duration_ms,
        )
        .await;
        state
    }

    fn substituted_inputs(&self, agent: &AgentSpec) -> std::collections::BTreeMap<String, String> {
        let artifacts = self.shared.artifacts.lock().expect("artefacts empoisonnés");
        agent
            .inputs
            .iter()
            .map(|(name, template)| (name.clone(), artifacts.substitute(template)))
            .collect()
    }

    async fn finish_agent(
        &self,
        stage_index: usize,
        agent: &AgentSpec,
        session_id: Option<String>,
        state: AgentState,
        tokens: i64,
        duration_ms: i64,
    ) {
        {
            let mut workflow = self.shared.state.lock().expect("état empoisonné");
            if let Some(status) = workflow.stages[stage_index]
                .agents
                .iter_mut()
                .find(|status| status.name == agent.name)
            {
                status.state = state.clone();
                status.tokens = tokens;
                status.duration_ms = duration_ms;
            }
        }
        self.recorder
            .agent_done(&AgentDone {
                stage: self.spec.stages[stage_index].name.clone(),
                agent: agent.name.clone(),
                session_id,
                state,
                tokens,
                duration_ms,
            })
            .await;
    }

    fn snapshot(&self) -> WorkflowState {
        self.shared.state.lock().expect("état empoisonné").clone()
    }

    fn set_stage(&self, index: usize, state: StageState) {
        self.shared.state.lock().expect("état empoisonné").stages[index].state = state;
    }

    /// Les agents d'un stage annulé n'ont jamais tourné : leur état suit celui
    /// du stage plutôt que de rester `Pending` dans la vue.
    fn cancel_stage_agents(&self, index: usize) {
        let mut workflow = self.shared.state.lock().expect("état empoisonné");
        for agent in &mut workflow.stages[index].agents {
            if !agent.state.is_terminal() {
                agent.state = AgentState::Cancelled;
            }
        }
    }

    fn set_agent(&self, stage_index: usize, agent: &str, state: AgentState) {
        let mut workflow = self.shared.state.lock().expect("état empoisonné");
        if let Some(status) = workflow.stages[stage_index]
            .agents
            .iter_mut()
            .find(|status| status.name == agent)
        {
            status.state = state;
        }
    }

    fn set_agent_session(&self, stage_index: usize, agent: &str, session_id: &str) {
        let mut workflow = self.shared.state.lock().expect("état empoisonné");
        if let Some(status) = workflow.stages[stage_index]
            .agents
            .iter_mut()
            .find(|status| status.name == agent)
        {
            status.session_id = Some(session_id.to_string());
        }
    }
}

enum Interrupt {
    Budget(BudgetLimit),
    Cancelled,
}

/// Un budget absent ne doit jamais gagner la course du `select!` : sans borne,
/// le garde ne se termine pas.
async fn duration_guard(max_duration_s: Option<i64>) {
    match max_duration_s {
        Some(seconds) => tokio::time::sleep(Duration::from_secs(seconds.max(0) as u64)).await,
        None => std::future::pending().await,
    }
}

async fn token_guard(runner: &dyn AgentRunner, session_id: &str, max_tokens: Option<i64>) {
    let Some(max_tokens) = max_tokens else {
        return std::future::pending().await;
    };
    loop {
        tokio::time::sleep(TOKEN_POLL_INTERVAL).await;
        if runner.tokens_used(session_id).await > max_tokens {
            return;
        }
    }
}
