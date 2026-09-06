//! Les kinds v2 de l'orchestration, écrits sur la session **parente**.
//!
//! Sept kinds structurels — `workflow_started`, `stage_started`,
//! `agent_started`, `agent_done`, `gate_decision`, `workflow_cancelled`,
//! `workflow_done` — décrivent la topologie et les décisions : petits,
//! permanents, ils sont l'historique du workflow au même titre que
//! `turn_start`. Un huitième, `workflow_artifact`, porte la sortie complète
//! d'un agent : volumineux, purgeable, tranché du `agent_done` qui n'en garde
//! que les compteurs (`replay::retention::PURGEABLE_KINDS`).
//!
//! Un neuvième, `workflow_recipe`, porte le **contenu** des recettes
//! référencées par la spec : un fichier lu au moment du run est de l'état
//! externe qui entre dans le prompt d'un agent, donc il est capturé — et
//! volumineux comme un artefact, donc purgeable lui aussi.
//!
//! Quatre de ces entrées sont **servies** au rejeu : `gate_decision` (une
//! approbation humaine, jamais redemandée), `workflow_cancelled` (l'annulation
//! d'un opérateur, servie par [`crate::workflow::gate::ReplayGates`] aux gates
//! qui n'ont pas eu de décision), `workflow_artifact` + `agent_done` (la sortie
//! et l'issue de chaque agent, servies par
//! [`crate::workflow::runner::ReplayRunner`] au lieu de relancer un
//! sous-agent) et `workflow_recipe` (le fichier, servi sans relire le disque).
//! Les autres décrivent ce qui s'est passé, elles ne le pilotent pas.
//!
//! Le workflow occupe un `turn_seq` unique sur la session parente, qu'il
//! **revendique** avec un `turn_start` — l'index unique
//! `idx_session_events_turn_alloc` ferme l'allocation du numéro, et la
//! transaction de [`SessionManager::claim_exclusive_turn_start`] ferme
//! l'exclusivité du workflow sur la session — et qu'il referme avec un
//! `turn_end` à `workflow_done`. Un workflow tué laisse donc une borne
//! ouverte, exactement comme un tour d'agent tué : `first_unclosed_turn` la
//! voit — y compris sous les tours d'agent qu'une session reprise a joués
//! par-dessus —, et le curseur refuse un journal tronqué au lieu de le rejouer
//! à moitié.
//!
//! Un agent qui échoue à `prepare` n'a pas de session enfant : il n'émet que
//! `agent_done`, sans `agent_started` préalable. Les vues se construisent donc
//! sur `WorkflowState`, jamais sur la seule paire d'events.

use std::sync::Arc;

use anyhow::Result;
use kaji_core::workflow::{Gate, WorkflowSpec};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::warn;

use crate::session::session_manager::TurnClaim;
use crate::session::SessionManager;
use crate::workflow::gate::GateDecision;
use crate::workflow::state::{AgentState, WorkflowState};

/// La borne fermante du tour de workflow. L'ouvrante — le `turn_start` — est
/// écrite par [`SessionManager::claim_exclusive_turn_start`], qui la revendique
/// sous l'index unique d'allocation. Le workflow pose donc les mêmes bornes que
/// la boucle agent : c'est ce qui rend son tour visible à l'allocation, à la
/// détection de troncature et au plan de rejeu.
const TURN_END: &str = "turn_end";

pub const WORKFLOW_STARTED: &str = "workflow_started";
pub const STAGE_STARTED: &str = "stage_started";
pub const AGENT_STARTED: &str = "agent_started";
pub const AGENT_DONE: &str = "agent_done";
pub const GATE_DECISION: &str = "gate_decision";
pub const WORKFLOW_ARTIFACT: &str = "workflow_artifact";
pub const WORKFLOW_RECIPE: &str = "workflow_recipe";
pub const WORKFLOW_CANCELLED: &str = "workflow_cancelled";
pub const WORKFLOW_DONE: &str = "workflow_done";

/// Les kinds de l'orchestration, dans l'ordre d'apparition d'une exécution.
/// Exhaustif : un test cloue que le recorder n'en écrit pas d'autre.
pub const WORKFLOW_KINDS: [&str; 9] = [
    WORKFLOW_STARTED,
    WORKFLOW_RECIPE,
    STAGE_STARTED,
    AGENT_STARTED,
    GATE_DECISION,
    WORKFLOW_ARTIFACT,
    AGENT_DONE,
    WORKFLOW_CANCELLED,
    WORKFLOW_DONE,
];

/// Le champ du payload `turn_start` qui distingue un tour d'orchestration d'un
/// tour d'agent. C'est lui qui rend le tour de workflow reconnaissable au plan
/// de rejeu (`replay::plan`) — un tour d'agent se rejoue en réinjectant son
/// message user, un tour de workflow en redéroulant son DAG depuis le journal.
pub const TURN_WORKFLOW_FIELD: &str = "workflow";

/// Le nom du workflow porté par un payload de `turn_start`, `None` pour un
/// tour d'agent.
pub fn workflow_of_turn_start(payload_json: &str) -> Option<String> {
    serde_json::from_str::<Value>(payload_json)
        .ok()?
        .get(TURN_WORKFLOW_FIELD)?
        .as_str()
        .map(str::to_string)
}

/// Pourquoi un workflow ne peut pas s'attacher à une session.
#[derive(Debug, thiserror::Error)]
pub enum WorkflowRecorderError {
    /// La marque d'un workflow est son `turn_start`, et il reste au journal
    /// **même annulé** : reprendre un workflow après `cancel()` demande donc
    /// une nouvelle session, jamais un second tour sur celle-ci (les gates y
    /// sont adressées par nom de stage, un second run écraserait les
    /// décisions du premier).
    #[error("la session « {0} » porte déjà le workflow « {1} » : une session parente n'en porte qu'un, même annulé — relancer demande une nouvelle session")]
    AlreadyCarriesAWorkflow(String, String),

    #[error("impossible de revendiquer un tour sur la session « {0} » après {1} tentatives : un autre écrivain alloue en boucle")]
    TurnAllocationLost(String, u32),
}

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

/// La sortie d'un agent, l'un des deux payloads volumineux de la famille —
/// d'où sa place dans les kinds purgeables et sa séparation d'avec
/// `agent_done`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowArtifact {
    pub stage: String,
    pub agent: String,
    pub output: String,
}

/// Le contenu d'une recette référencée par la spec, lu **une fois par
/// chemin** — à sa première référence — puis servi tel quel aux autres agents
/// du même run. Le *chemin* seul ne suffit pas : le fichier peut changer entre
/// l'enregistrement et le rejeu, et il entre dans le prompt de l'agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowRecipeContent {
    pub path: String,
    /// Le dossier depuis lequel la recette résout ses inclusions, tel que
    /// résolu à l'enregistrement (expansion du `~` comprise) : le rejeu ne
    /// peut pas le redériver du chemin sans refaire cette résolution.
    pub parent_dir: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateDecided {
    pub stage: String,
    pub decision: GateDecision,
}

/// L'annulation d'un opérateur, **datée par sa place dans le journal**.
/// `workflow_done` dit qu'un run s'est terminé annulé ; il ne dit pas à quel
/// moment l'annulation est tombée, et un refus de gate suffit d'ailleurs à
/// figer la même issue. Ce kind-là dit les deux : que quelqu'un a annulé, et
/// devant quels événements. C'est ce que le rejeu sert aux gates restées sans
/// décision (règle « Replay v2 » d'AGENTS.md : l'annulation est de l'état
/// externe qui pilote le run, elle a son kind).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowCancelled {
    pub workflow: String,
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

/// Combien de fois `open` réessaie un **numéro** de tour perdu au profit d'un
/// autre écrivain de la session (jamais une exclusivité perdue : celle-là est
/// définitive). Chaque perte fait avancer `MAX(turn_seq)` d'au moins un :
/// quelques tentatives suffisent, et une boucle infinie masquerait un écrivain
/// fou.
const TURN_CLAIM_ATTEMPTS: u32 = 8;

impl WorkflowRecorder {
    /// Pose `log_meta`, puis **revendique** son tour par un `turn_start` écrit
    /// d'abord : c'est l'écriture qui tranche, pas une lecture préalable.
    /// [`SessionManager::claim_exclusive_turn_start`] refuse la session dans la
    /// même transaction que l'INSERT, donc deux `open()` concurrents ne peuvent
    /// pas s'attacher tous les deux — le perdant est **refusé**
    /// (`AlreadyCarriesAWorkflow`), jamais poussé sur le tour suivant.
    ///
    /// Le seul cas qui fait réessayer est un tour **d'agent** qui a pris le
    /// numéro visé entre `next_turn_seq` et l'INSERT (la boucle agent alloue
    /// hors transaction) : le workflow va alors chercher un tour libre plus
    /// loin, au lieu d'écrire ses events sous un numéro déjà tenu.
    pub async fn open(
        session_manager: Arc<SessionManager>,
        session_id: String,
        workflow: &str,
    ) -> Result<Self> {
        session_manager
            .append_log_meta_if_absent(&session_id)
            .await?;

        let payload = serde_json::json!({
            "query_preview": format!("workflow « {workflow} »"),
            TURN_WORKFLOW_FIELD: workflow,
        })
        .to_string();

        for _ in 0..TURN_CLAIM_ATTEMPTS {
            let turn_seq = session_manager.next_turn_seq(&session_id).await?;
            match session_manager
                .claim_exclusive_turn_start(&session_id, turn_seq, TURN_WORKFLOW_FIELD, &payload)
                .await?
            {
                TurnClaim::Claimed => {
                    return Ok(Self {
                        session_manager,
                        session_id,
                        turn_seq,
                    })
                }
                TurnClaim::AlreadyExclusive(existing) => {
                    return Err(WorkflowRecorderError::AlreadyCarriesAWorkflow(
                        session_id, existing,
                    )
                    .into())
                }
                TurnClaim::TurnTaken => continue,
            }
        }

        Err(WorkflowRecorderError::TurnAllocationLost(session_id, TURN_CLAIM_ATTEMPTS).into())
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn turn_seq(&self) -> i64 {
        self.turn_seq
    }

    /// Le seul endroit qui décide de la non-fatalité : **toute** perte d'event
    /// — sérialisation comprise — marque la session non rejouable. Une branche
    /// qui rendrait la main sans le faire laisserait un journal troué se
    /// présenter comme complet, et le rejeu partirait dessus.
    async fn append<T: Serialize>(&self, kind: &str, payload: &T) {
        if let Err(error) = self.try_append(kind, payload).await {
            self.lose_event(kind, &error).await;
        }
    }

    async fn lose_event(&self, kind: &str, error: &anyhow::Error) {
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

    async fn try_append<T: Serialize>(&self, kind: &str, payload: &T) -> Result<()> {
        self.session_manager
            .append_event(
                &self.session_id,
                self.turn_seq,
                kind,
                &self.encode(payload)?,
            )
            .await
    }

    fn encode<T: Serialize>(&self, payload: &T) -> Result<String> {
        Ok(match serde_json::to_value(payload)? {
            Value::Object(mut object) => {
                object.insert("turn_seq".to_string(), Value::from(self.turn_seq));
                Value::Object(object).to_string()
            }
            other => other.to_string(),
        })
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

    pub async fn recipe(&self, recipe: &WorkflowRecipeContent) {
        self.append(WORKFLOW_RECIPE, recipe).await;
    }

    /// Écrite **avant** que le jeton d'annulation ne parte, pour que l'event
    /// précède au journal toutes les conséquences de l'annulation — les
    /// `agent_done` des agents coupés, puis `workflow_done`. Écrite après, sa
    /// place raconterait une annulation tombée plus tard qu'elle ne l'a été, et
    /// c'est précisément cette place que le rejeu lit.
    pub async fn workflow_cancelled(&self, workflow: &str) {
        self.append(
            WORKFLOW_CANCELLED,
            &WorkflowCancelled {
                workflow: workflow.to_string(),
            },
        )
        .await;
    }

    /// Ferme le tour en même temps qu'elle fige l'état : `workflow_done` sans
    /// `turn_end` laisserait le journal indiscernable d'un workflow tué, et
    /// `turn_end` sans `workflow_done` ferait passer un tour invérifiable pour
    /// un tour clos. Les deux INSERT tiennent donc dans une seule transaction —
    /// écrits séparément, le second pouvait échouer seul et condamner la
    /// session sans réparation possible.
    pub async fn workflow_done(&self, state: &WorkflowState) {
        let done = match self.encode(&WorkflowDone {
            workflow: state.workflow.clone(),
            state: state.clone(),
        }) {
            Ok(payload) => payload,
            Err(error) => {
                self.lose_event(WORKFLOW_DONE, &error).await;
                return;
            }
        };

        if let Err(error) = self
            .session_manager
            .append_events(
                &self.session_id,
                self.turn_seq,
                &[(WORKFLOW_DONE, done.as_str()), (TURN_END, "{}")],
            )
            .await
        {
            self.lose_event(WORKFLOW_DONE, &error).await;
        }
    }
}
