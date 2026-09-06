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
//!
//! Un workflow n'a pas de borne de temps propre : une gate sans approbateur
//! attend indéfiniment, et [`WorkflowHandle::cancel`] est **la** sortie. Elle
//! réveille l'attente de gate comme elle coupe un agent en vol, et laisse le
//! workflow en `Cancelled` avec son journal fermé.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use futures::stream::{FuturesUnordered, StreamExt};
use kaji_core::workflow::{AgentSource, AgentSpec, Gate, WorkflowSpec};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::recipe::local_recipes::load_local_recipe_file;
use crate::workflow::artifacts::Artifacts;
use crate::workflow::events::{AgentDone, WorkflowRecipeContent, WorkflowRecorder};
use crate::workflow::gate::{GateDecision, GateOutcome, GateSource, GateVerdict, LiveGates};
use crate::workflow::runner::{AgentRunRequest, AgentRunner, ResolvedRecipe};
use crate::workflow::state::{AgentState, BudgetLimit, FailureCause, StageState, WorkflowState};

/// Période de relecture de l'usage des sessions enfants pour le budget
/// `max_tokens`. Le dépassement se constate donc à un demi-tick près : un
/// budget de tokens borne le coût, il ne le coupe pas au token exact.
const TOKEN_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Délai laissé à un agent pour s'arrêter proprement après l'annulation, avant
/// que l'exécuteur passe à la suite sans lui.
pub const CANCEL_GRACE: Duration = Duration::from_secs(5);

/// Ce qu'un appelant qui ferme le workflow de l'extérieur — la TUI qui quitte —
/// doit lui laisser. Strictement plus que [`CANCEL_GRACE`] : un agent sourd à
/// son jeton fait attendre l'exécuteur cette grâce-là **avant** qu'il ne
/// l'abandonne, et il lui reste ensuite à clore son journal. Attendre la même
/// durée, c'est partir juste avant `workflow_done` — mesuré en vrai sur un
/// agent bloqué en appel HTTP.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(CANCEL_GRACE.as_secs() * 2);

struct Shared {
    state: Mutex<WorkflowState>,
    artifacts: Mutex<Artifacts>,
    cancel: CancellationToken,
    /// Le journal de l'exécution, partagé avec [`WorkflowHandle`] : l'annulation
    /// s'écrit depuis la prise de contrôle, au moment où elle tombe, pas depuis
    /// l'exécuteur qui ne la constate qu'ensuite.
    recorder: Arc<WorkflowRecorder>,
    workflow: String,
    /// L'annulation n'est journalisée qu'une fois. Deux appelants peuvent la
    /// demander ensemble — le Ctrl+C du CLI et l'arrêt de la TUI — et deux
    /// `workflow_cancelled` diraient deux annulations là où il n'y en a eu
    /// qu'une.
    cancel_announced: AtomicBool,
    live_gates: Option<Arc<LiveGates>>,
    pauses: PauseSwitch,
    /// Les stages dont le fan-out est parti : leurs deux points d'arrêt sont
    /// derrière eux, une pause n'y mordrait plus.
    launched: Mutex<HashSet<String>>,
    agent_cancels: Mutex<AgentCancels>,
}

impl Shared {
    fn mark_fan_out_left(&self, stage: &str) {
        self.launched
            .lock()
            .expect("stages lancés empoisonnés")
            .insert(stage.to_string());
    }

    fn fan_out_left(&self, stage: &str) -> bool {
        self.launched
            .lock()
            .expect("stages lancés empoisonnés")
            .contains(stage)
    }

    /// Enregistre l'intention d'annuler un agent et coupe son jeton s'il est
    /// déjà armé. L'intention est gardée pour l'agent qui n'a pas encore
    /// démarré : sans elle, une annulation posée pendant la préparation se
    /// perdrait entre la demande et l'armement.
    fn request_agent_cancel(&self, key: (String, String)) {
        let mut cancels = self.agent_cancels.lock().expect("annulations empoisonnées");
        if let Some(token) = cancels.tokens.get(&key) {
            token.cancel();
        }
        cancels.requested.insert(key);
    }

    fn arm_agent_cancel(&self, key: (String, String), token: &CancellationToken) {
        let mut cancels = self.agent_cancels.lock().expect("annulations empoisonnées");
        if cancels.requested.contains(&key) {
            token.cancel();
        }
        cancels.tokens.insert(key, token.clone());
    }

    fn disarm_agent_cancel(&self, key: &(String, String)) {
        self.agent_cancels
            .lock()
            .expect("annulations empoisonnées")
            .tokens
            .remove(key);
    }
}

/// Les jetons d'annulation par agent, et les annulations demandées avant leur
/// armement. Les deux vivent sous le **même** verrou : c'est ce qui ferme la
/// course entre `cancel_agent` et le démarrage de l'agent visé.
#[derive(Default)]
struct AgentCancels {
    requested: HashSet<(String, String)>,
    tokens: HashMap<(String, String), CancellationToken>,
}

/// Les stages suspendus par un opérateur. Même patron que [`LiveGates`] : la
/// table porte l'état, le `watch` sert de réveil — un stage qui s'abonne avant
/// de relire la table ne peut pas rater une reprise.
struct PauseSwitch {
    paused: Mutex<HashSet<String>>,
    version: watch::Sender<u64>,
}

impl Default for PauseSwitch {
    fn default() -> Self {
        let (version, _) = watch::channel(0);
        Self {
            paused: Mutex::new(HashSet::new()),
            version,
        }
    }
}

impl PauseSwitch {
    fn set(&self, stage: &str, paused: bool) {
        {
            let mut table = self.paused.lock().expect("pauses empoisonnées");
            if paused {
                table.insert(stage.to_string());
            } else {
                table.remove(stage);
            }
        }
        self.version.send_modify(|version| *version += 1);
    }

    fn is_paused(&self, stage: &str) -> bool {
        self.paused
            .lock()
            .expect("pauses empoisonnées")
            .contains(stage)
    }

    async fn wait_until_resumed(&self, stage: &str) {
        let mut version = self.version.subscribe();
        while self.is_paused(stage) {
            if version.changed().await.is_err() {
                return;
            }
        }
    }
}

/// D'où vient le contenu d'une recette référencée par la spec : du disque en
/// exécution vivante — et il est alors journalisé —, du journal au rejeu.
///
/// `Disk` mémoïse ce qu'il a lu : un chemin référencé par N agents est lu et
/// journalisé **une** fois, pas N. Sans quoi un stage de 5 agents sur la même
/// recette écrirait 5 copies de son contenu au journal — sur un kind créé
/// précisément parce que ce payload est volumineux — et pourrait servir deux
/// versions différentes si le fichier changeait en cours de run.
enum RecipeSource {
    Disk(Mutex<HashMap<String, WorkflowRecipeContent>>),
    Recorded(HashMap<String, WorkflowRecipeContent>),
}

/// Ce que devient une demande de pause ou de reprise. Quatre verdicts et non un
/// booléen : une pause posée sur un stage dont le fan-out est parti n'a plus de
/// point d'arrêt où mordre, et l'annoncer « suspendu » ferait attendre un arrêt
/// qui ne viendra pas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PauseVerdict {
    /// Demande enregistrée : le stage la lira à son prochain point d'arrêt.
    Applied,
    /// Aucun stage de ce nom dans le workflow.
    UnknownStage,
    /// Le stage est terminal : il n'a plus rien à suspendre.
    Settled,
    /// Le fan-out est parti, les deux points d'arrêt sont derrière lui.
    AlreadyLaunched,
}

impl PauseVerdict {
    pub fn applied(&self) -> bool {
        *self == PauseVerdict::Applied
    }

    pub fn label(&self) -> &'static str {
        match self {
            PauseVerdict::Applied => "demande enregistrée",
            PauseVerdict::UnknownStage => "stage inconnu",
            PauseVerdict::Settled => "stage déjà terminé",
            PauseVerdict::AlreadyLaunched => {
                "stage déjà lancé, aucun point d'arrêt restant — `x` coupe un agent"
            }
        }
    }
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

    pub fn approve(&self, stage: &str) -> GateVerdict {
        self.decide(stage, GateDecision::Approve)
    }

    pub fn deny(&self, stage: &str) -> GateVerdict {
        self.decide(stage, GateDecision::Deny)
    }

    /// Quatre verdicts et non un booléen : « ce workflow prend des décisions
    /// vivantes » ne dit pas si la décision servira. Une décision posée sur un
    /// stage inexistant, mort, ou **sans gate** ne doit pas s'afficher comme
    /// une approbation — `run_stage` ne consomme une décision que sur un stage
    /// `gate: approve`. Le refus de terminalité passe avant celui de gate : un
    /// stage fini est réglé, qu'il ait eu une gate ou non.
    fn decide(&self, stage: &str, decision: GateDecision) -> GateVerdict {
        let snapshot = self.snapshot();
        let Some(status) = snapshot.stage(stage) else {
            return GateVerdict::UnknownStage;
        };
        let Some(gates) = self.shared.live_gates.as_ref() else {
            return GateVerdict::Settled;
        };
        if status.state.is_terminal() {
            return GateVerdict::Settled;
        }
        if status.gate != Gate::Approve {
            return GateVerdict::NoGate;
        }
        gates.record(stage, decision);
        GateVerdict::Applied
    }

    /// Suspend un stage à son prochain point d'arrêt — avant son démarrage,
    /// et de nouveau après sa gate.
    ///
    /// La suspension n'est **pas** rétroactive, et le refus le dit : un stage
    /// dont le fan-out est parti n'a plus de point d'arrêt où mordre, la
    /// demande est refusée par son nom plutôt qu'enregistrée sans effet — c'est
    /// [`Self::cancel_agent`] qui coupe un agent en vol.
    pub fn pause(&self, stage: &str) -> PauseVerdict {
        self.switch_pause(stage, true)
    }

    /// Relâche un stage suspendu. Refuse un stage inconnu ou terminal, jamais
    /// un stage simplement pas en pause : y reprendre est un non-événement.
    pub fn resume(&self, stage: &str) -> PauseVerdict {
        self.switch_pause(stage, false)
    }

    /// Les stages qu'un opérateur a demandé de suspendre. Un stage encore
    /// `Pending` n'atteint son point d'arrêt qu'en démarrant : son `StageState`
    /// ne dira `Paused` que plus tard, et une vue qui ne lirait que l'état ne
    /// saurait pas qu'une pause est déjà posée — elle en poserait une seconde
    /// au lieu de la lever.
    pub fn paused_stages(&self) -> HashSet<String> {
        self.shared
            .pauses
            .paused
            .lock()
            .expect("pauses empoisonnées")
            .clone()
    }

    fn switch_pause(&self, stage: &str, paused: bool) -> PauseVerdict {
        let snapshot = self.snapshot();
        let Some(status) = snapshot.stage(stage) else {
            return PauseVerdict::UnknownStage;
        };
        if status.state.is_terminal() {
            return PauseVerdict::Settled;
        }
        if paused && self.shared.fan_out_left(stage) {
            return PauseVerdict::AlreadyLaunched;
        }
        self.shared.pauses.set(stage, paused);
        PauseVerdict::Applied
    }

    /// Coupe **un** agent, sans toucher au reste de son fan-out ni au
    /// workflow. Rend `false` sur un agent inconnu ou déjà terminal.
    ///
    /// Une annulation posée avant le démarrage de l'agent est gardée : elle
    /// coupera le jeton dès son armement, plutôt que de se perdre.
    pub fn cancel_agent(&self, stage: &str, agent: &str) -> bool {
        let snapshot = self.snapshot();
        let Some(status) = snapshot
            .stage(stage)
            .and_then(|stage| stage.agents.iter().find(|status| status.name == agent))
        else {
            return false;
        };
        if status.state.is_terminal() {
            return false;
        }
        self.shared
            .request_agent_cancel((stage.to_string(), agent.to_string()));
        true
    }

    /// La sortie publiée par un agent, telle que les descendants la
    /// substitueront. Ce que les vues affichent d'un agent terminé, sans
    /// relire le journal.
    pub fn artifact(&self, stage: &str, agent: &str) -> Option<String> {
        self.shared
            .artifacts
            .lock()
            .expect("artefacts empoisonnés")
            .get(stage, agent)
            .map(str::to_string)
    }

    /// La seule sortie d'un workflow suspendu : une gate sans approbateur
    /// attend sans borne, et c'est l'annulation qui la réveille. Elle coupe
    /// aussi les agents en vol et empêche les stages restants de démarrer.
    ///
    /// Elle **se journalise** avant de couper : `workflow_cancelled` date
    /// l'annulation par sa place dans le flux d'événements, et c'est cette
    /// place — pas l'issue finale — que le rejeu lit pour savoir quelles gates
    /// n'ont jamais eu de décision. L'écriture précède la coupure pour que
    /// l'event précède toutes ses conséquences.
    ///
    /// Elle ne libère pas la session parente : le `turn_start` du workflow y
    /// reste, donc une reprise après annulation passe par une **nouvelle**
    /// session (voir [`crate::workflow::events::WorkflowRecorderError`]).
    pub async fn cancel(&self) {
        if self.shared.cancel_announced.swap(true, Ordering::SeqCst) {
            return;
        }
        self.shared
            .recorder
            .workflow_cancelled(&self.shared.workflow)
            .await;
        self.shared.cancel.cancel();
    }
}

pub struct WorkflowExecutor {
    spec: WorkflowSpec,
    runner: Arc<dyn AgentRunner>,
    gates: Arc<dyn GateSource>,
    recipes: RecipeSource,
    recorder: Arc<WorkflowRecorder>,
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
    ) -> Result<Self> {
        let live_gates = Arc::new(LiveGates::default());
        let gates = Arc::clone(&live_gates) as Arc<dyn GateSource>;
        Self::build(
            spec,
            runner,
            gates,
            Some(live_gates),
            RecipeSource::Disk(Mutex::new(HashMap::new())),
            recorder,
            parent_session_id,
            working_dir,
        )
    }

    /// Rejeu : les gates et les recettes viennent du journal, aucune
    /// approbation n'est redemandée et aucun fichier n'est relu.
    pub fn replaying(
        spec: WorkflowSpec,
        runner: Arc<dyn AgentRunner>,
        gates: Arc<dyn GateSource>,
        recipes: HashMap<String, WorkflowRecipeContent>,
        recorder: WorkflowRecorder,
        parent_session_id: String,
        working_dir: PathBuf,
    ) -> Result<Self> {
        Self::build(
            spec,
            runner,
            gates,
            None,
            RecipeSource::Recorded(recipes),
            recorder,
            parent_session_id,
            working_dir,
        )
    }

    /// La spec est validée **ici**, pas seulement à la lecture du YAML : elle
    /// peut aussi venir d'un payload `workflow_started` relu, et un
    /// `depends_on` pointant un stage disparu ferait sortir la boucle sans
    /// avoir rien exécuté.
    #[allow(clippy::too_many_arguments)]
    fn build(
        spec: WorkflowSpec,
        runner: Arc<dyn AgentRunner>,
        gates: Arc<dyn GateSource>,
        live_gates: Option<Arc<LiveGates>>,
        recipes: RecipeSource,
        recorder: WorkflowRecorder,
        parent_session_id: String,
        working_dir: PathBuf,
    ) -> Result<Self> {
        spec.validate()?;
        let recorder = Arc::new(recorder);
        let shared = Arc::new(Shared {
            state: Mutex::new(WorkflowState::from_spec(&spec)),
            artifacts: Mutex::new(Artifacts::default()),
            cancel: CancellationToken::new(),
            recorder: Arc::clone(&recorder),
            workflow: spec.name.clone(),
            cancel_announced: AtomicBool::new(false),
            live_gates,
            pauses: PauseSwitch::default(),
            launched: Mutex::new(HashSet::new()),
            agent_cancels: Mutex::new(AgentCancels::default()),
        });
        Ok(Self {
            spec,
            runner,
            gates,
            recipes,
            recorder,
            parent_session_id,
            working_dir,
            shared,
        })
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

        if !self.hold_while_paused(index).await {
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
                Ok(GateOutcome::Decided(decision)) => decision,
                // Le journal porte une annulation et cette gate n'a jamais eu
                // de décision : ce stage-là est tombé avec elle. La coupure
                // s'arrête à lui et à sa descendance — annuler tout le workflow
                // ici couperait des stages que l'enregistrement a vus aboutir
                // **avant** que l'annulation ne tombe, et le journal ne dit pas
                // qu'un rejeu atteint les gates dans le même ordre.
                Ok(GateOutcome::Cancelled) => {
                    self.set_stage(index, StageState::Cancelled);
                    self.cancel_stage_agents(index);
                    return (index, StageState::Cancelled);
                }
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

        // Second point d'arrêt : une gate approuvée peut avoir ouvert un stage
        // qu'un opérateur veut retenir avant que son fan-out parte.
        if !self.hold_while_paused(index).await {
            return (index, StageState::Cancelled);
        }
        self.set_stage(index, StageState::Running);
        // Passé ce point, plus aucune pause ne peut retenir le stage : la
        // demande sera refusée par son nom au lieu d'être rangée sans effet.
        self.shared.mark_fan_out_left(&stage.name);

        // Le budget `max_tokens` appartient au stage, donc à tout son fan-out :
        // un seul garde somme l'usage des sessions enfants et coupe les agents
        // ensemble. Un garde par agent laisserait N agents dépenser N × le
        // budget déclaré.
        let overspent = CancellationToken::new();
        let stage_over = CancellationToken::new();
        let agents = async {
            let outcomes = futures::future::join_all(
                stage
                    .agents
                    .iter()
                    .map(|agent| self.run_agent(index, agent, &overspent)),
            )
            .await;
            stage_over.cancel();
            outcomes
        };
        let (outcomes, ()) = futures::future::join(
            agents,
            self.token_budget_guard(index, stage.budgets.max_tokens, &overspent, &stage_over),
        )
        .await;

        let state = stage_state(&outcomes);
        self.set_stage(index, state.clone());
        (index, state)
    }

    async fn run_agent(
        &self,
        stage_index: usize,
        agent: &AgentSpec,
        overspent: &CancellationToken,
    ) -> AgentState {
        let stage = &self.spec.stages[stage_index];
        self.set_agent(stage_index, &agent.name, AgentState::Running);

        let recipe = match self.resolve_recipe(&agent.source).await {
            Ok(recipe) => recipe,
            Err(error) => {
                let state = AgentState::Failed(FailureCause::Error(error));
                self.finish_agent(stage_index, agent, None, state.clone(), 0, 0)
                    .await;
                return state;
            }
        };

        let request = AgentRunRequest {
            stage: stage.name.clone(),
            agent: agent.name.clone(),
            source: agent.source.clone(),
            model: agent.model.clone(),
            inputs: self.substituted_inputs(agent),
            recipe,
            parent_session_id: self.parent_session_id.clone(),
            working_dir: self.working_dir.clone(),
        };

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

        // Rejeu : l'issue vient du journal, aucun sous-agent ne part. Les
        // gardes de budget ne sont pas armés — un budget dépassé à
        // l'enregistrement est déjà dans l'état servi.
        if let Some(recorded) = self.runner.recorded_outcome(&request).await {
            if let Some(output) = &recorded.output {
                self.publish_artifact(&stage.name, &agent.name, output)
                    .await;
            }
            self.finish_agent(
                stage_index,
                agent,
                Some(session_id),
                recorded.state.clone(),
                recorded.tokens,
                0,
            )
            .await;
            return recorded.state;
        }

        let started = Instant::now();
        // Enfant du jeton du workflow : une annulation d'ensemble le tire
        // aussi, et `cancel_agent` peut le tirer seul.
        let cancel = self.shared.cancel.child_token();
        let key = (stage.name.clone(), agent.name.clone());
        self.shared.arm_agent_cancel(key.clone(), &cancel);
        let mut run = Box::pin(self.runner.run(request, &session_id, cancel.clone()));

        let interrupted = tokio::select! {
            result = &mut run => Ok(result),
            _ = duration_guard(stage.budgets.max_duration_s) => Err(Interrupt::Budget(BudgetLimit::Duration)),
            _ = overspent.cancelled() => Err(Interrupt::Budget(BudgetLimit::Tokens)),
            _ = cancel.cancelled() => Err(Interrupt::Cancelled),
        };

        let state = match interrupted {
            Ok(Ok(output)) => {
                self.publish_artifact(&stage.name, &agent.name, &output)
                    .await;
                AgentState::Done
            }
            Ok(Err(error)) => match self.interruption(overspent, &cancel) {
                Some(state) => {
                    warn!(
                        stage = %stage.name,
                        agent = %agent.name,
                        session_id = %session_id,
                        %error,
                        "workflow: erreur d'agent masquée par l'interruption en cours"
                    );
                    state
                }
                None => AgentState::Failed(FailureCause::Error(error)),
            },
            Err(interrupt) => {
                cancel.cancel();
                if tokio::time::timeout(CANCEL_GRACE, &mut run).await.is_err() {
                    warn!(
                        stage = %stage.name,
                        agent = %agent.name,
                        session_id = %session_id,
                        "workflow: agent toujours en vol après la grâce d'annulation — abandonné"
                    );
                }
                match interrupt {
                    Interrupt::Budget(limit) => AgentState::Failed(FailureCause::Budget(limit)),
                    Interrupt::Cancelled => AgentState::Cancelled,
                }
            }
        };

        self.shared.disarm_agent_cancel(&key);
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

    /// Un agent qui rend une erreur **parce qu'il vient d'être coupé** n'est
    /// pas un agent en échec : le jeton tiré fait foi sur le message rendu.
    /// Sans cette précédence, `cancel()` donnerait tantôt `Cancelled`, tantôt
    /// `Failed(Error("annulé"))`, au gré du `select!`.
    ///
    /// Le budget passe avant le jeton de l'agent : le dépassement tire ce
    /// jeton lui-même, et « coupé faute de tokens » est plus informatif
    /// qu'« annulé ».
    fn interruption(
        &self,
        overspent: &CancellationToken,
        agent_cancel: &CancellationToken,
    ) -> Option<AgentState> {
        if self.shared.cancel.is_cancelled() {
            return Some(AgentState::Cancelled);
        }
        if overspent.is_cancelled() {
            return Some(AgentState::Failed(FailureCause::Budget(
                BudgetLimit::Tokens,
            )));
        }
        if agent_cancel.is_cancelled() {
            return Some(AgentState::Cancelled);
        }
        None
    }

    /// Retient un stage suspendu à son point d'arrêt. Rend `false` quand
    /// l'attente est rompue par l'annulation du workflow — le stage et ses
    /// agents sont alors déjà marqués.
    async fn hold_while_paused(&self, index: usize) -> bool {
        let stage = &self.spec.stages[index];
        if !self.shared.pauses.is_paused(&stage.name) {
            return true;
        }
        self.set_stage(index, StageState::Paused);
        tokio::select! {
            () = self.shared.pauses.wait_until_resumed(&stage.name) => true,
            _ = self.shared.cancel.cancelled() => {
                self.set_stage(index, StageState::Cancelled);
                self.cancel_stage_agents(index);
                false
            }
        }
    }

    /// Le contenu d'une recette est de l'état externe qui entre dans le prompt
    /// de l'agent : il est journalisé à l'exécution et servi au rejeu, jamais
    /// relu du disque une seconde fois.
    async fn resolve_recipe(&self, source: &AgentSource) -> Result<Option<ResolvedRecipe>, String> {
        let AgentSource::Recipe(path) = source else {
            return Ok(None);
        };
        let path = path.to_string_lossy().to_string();
        match &self.recipes {
            RecipeSource::Recorded(recorded) => recorded
                .get(&path)
                .map(|recipe| Some(ResolvedRecipe::from(recipe)))
                .ok_or_else(|| {
                    format!(
                        "recette « {path} » absente du journal : le rejeu ne relit pas le disque"
                    )
                }),
            RecipeSource::Disk(read) => {
                if let Some(recipe) = read.lock().expect("recettes empoisonnées").get(&path) {
                    return Ok(Some(ResolvedRecipe::from(recipe)));
                }
                let file = load_local_recipe_file(&path).map_err(|error| error.to_string())?;
                let recipe = WorkflowRecipeContent {
                    path: path.clone(),
                    parent_dir: file.parent_dir.to_string_lossy().to_string(),
                    content: file.content,
                };
                let first_read = read
                    .lock()
                    .expect("recettes empoisonnées")
                    .insert(path, recipe.clone())
                    .is_none();
                if first_read {
                    self.recorder.recipe(&recipe).await;
                }
                Ok(Some(ResolvedRecipe::from(&recipe)))
            }
        }
    }

    async fn publish_artifact(&self, stage: &str, agent: &str, output: &str) {
        self.shared
            .artifacts
            .lock()
            .expect("artefacts empoisonnés")
            .insert(stage, agent, output.to_string());
        self.recorder.artifact(stage, agent, output).await;
    }

    /// Somme l'usage des sessions enfants du stage — celles déjà préparées —
    /// et coupe tout le fan-out au franchissement. Le garde s'arrête avec le
    /// stage : il ne survit pas à ses agents.
    async fn token_budget_guard(
        &self,
        stage_index: usize,
        max_tokens: Option<i64>,
        overspent: &CancellationToken,
        stage_over: &CancellationToken,
    ) {
        let Some(max_tokens) = max_tokens else {
            return;
        };
        loop {
            tokio::select! {
                _ = stage_over.cancelled() => return,
                _ = tokio::time::sleep(TOKEN_POLL_INTERVAL) => {}
            }
            if self.stage_tokens(stage_index).await > max_tokens {
                overspent.cancel();
                return;
            }
        }
    }

    async fn stage_tokens(&self, stage_index: usize) -> i64 {
        let sessions: Vec<String> = {
            let workflow = self.shared.state.lock().expect("état empoisonné");
            workflow.stages[stage_index]
                .agents
                .iter()
                .filter_map(|agent| agent.session_id.clone())
                .collect()
        };
        let mut total = 0;
        for session in sessions {
            total += self.runner.tokens_used(&session).await;
        }
        total
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

/// L'état d'un stage à partir de celui de ses agents — la **même** dérivation
/// que celle dont `kaji workflow status` se sert pour relire un run depuis son
/// journal ([`StageState::from_agents`]).
///
/// `run_agent` ne rend que des états terminaux et la validation de la spec
/// interdit un stage sans agent : la dérivation aboutit toujours ici.
fn stage_state(outcomes: &[AgentState]) -> StageState {
    StageState::from_agents(outcomes).unwrap_or(StageState::Done)
}

enum Interrupt {
    Budget(BudgetLimit),
    Cancelled,
}

/// Un budget absent ne doit jamais gagner la course du `select!` : sans borne,
/// le garde ne se termine pas.
async fn duration_guard(max_duration_s: Option<i64>) {
    match max_duration_s {
        Some(seconds) => tokio::time::sleep(Duration::from_secs(seconds as u64)).await,
        None => std::future::pending().await,
    }
}
