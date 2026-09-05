//! Comment un agent de workflow est lancé.
//!
//! L'exécuteur ne connaît que [`AgentRunner`] ; l'implémentation de production
//! [`SubagentRunner`] descend sur `agents::subagent_handler::run_subagent_task`
//! — le **même** primitif de spawn que l'outil `delegate` de summon. Le
//! workflow n'ouvre donc pas un second mécanisme : il assemble la recette et la
//! `TaskConfig` comme summon, puis passe la main au chemin partagé.
//!
//! Le lancement est en deux temps : `prepare` crée la session enfant (avec son
//! `parent_session_id`) et rend son id, `run` l'exécute. C'est ce découpage qui
//! permet à `agent_started` de porter l'id de la session enfant **avant** que
//! l'agent tourne, et au garde de budget de tokens de lire l'usage de cette
//! session pendant qu'elle tourne.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use kaji_core::workflow::AgentSource;
use tokio_util::sync::CancellationToken;

use crate::agents::subagent_handler::{run_subagent_task, SubagentRunParams};
use crate::agents::subagent_task_config::TaskConfig;
use crate::agents::{AgentConfig, KajiPlatform};
use crate::config::permission::PermissionManager;
use crate::config::{Config, KajiMode};
use crate::providers;
use crate::recipe::build_recipe::build_recipe_from_template;
use crate::recipe::Recipe;
use crate::replay::cursor::EventCursor;
use crate::session::extension_data::EnabledExtensionsState;
use crate::session::{SessionManager, SessionType};
use crate::workflow::events::{AgentDone, WorkflowRecipeContent};
use crate::workflow::state::{AgentState, FailureCause};

#[derive(Debug, Clone)]
pub struct AgentRunRequest {
    pub stage: String,
    pub agent: String,
    pub source: AgentSource,
    pub model: Option<String>,
    /// Entrées déjà substituées : plus aucun `{{stage.agent.output}}` ici.
    pub inputs: BTreeMap<String, String>,
    /// La recette, résolue par l'exécuteur — du disque en exécution vivante,
    /// du journal au rejeu. Le runner ne lit jamais le fichier lui-même :
    /// c'est ce qui empêche une recette éditée entre deux runs de changer
    /// silencieusement le prompt d'un rejeu.
    pub recipe: Option<ResolvedRecipe>,
    pub parent_session_id: String,
    pub working_dir: PathBuf,
}

impl AgentRunRequest {
    pub fn label(&self) -> String {
        format!("{}.{}", self.stage, self.agent)
    }

    /// Les entrées rendues en bloc markdown, pour un agent dont la source est
    /// un prompt libre — une recette, elle, les reçoit en paramètres.
    pub fn inputs_block(&self) -> String {
        if self.inputs.is_empty() {
            return String::new();
        }
        let mut block = String::from("\n\n# Inputs\n");
        for (name, value) in &self.inputs {
            block.push_str(&format!("\n## {name}\n\n{value}\n"));
        }
        block
    }
}

/// Une recette prête à être rendue : son contenu et le dossier depuis lequel
/// elle résout ses inclusions.
#[derive(Debug, Clone)]
pub struct ResolvedRecipe {
    pub content: String,
    pub parent_dir: PathBuf,
}

impl From<&WorkflowRecipeContent> for ResolvedRecipe {
    fn from(recipe: &WorkflowRecipeContent) -> Self {
        Self {
            content: recipe.content.clone(),
            parent_dir: PathBuf::from(&recipe.parent_dir),
        }
    }
}

/// L'issue enregistrée d'un agent, telle que le journal la rend au rejeu.
/// Elle porte ce que `run` ne sait pas exprimer — un dépassement de budget,
/// une annulation — pour qu'un rejeu reproduise l'état exact plutôt qu'une
/// approximation en erreur.
#[derive(Debug, Clone)]
pub struct RecordedOutcome {
    pub state: AgentState,
    pub output: Option<String>,
    pub tokens: i64,
}

#[async_trait]
pub trait AgentRunner: Send + Sync {
    /// Crée la session enfant de l'agent et rend son id.
    async fn prepare(&self, request: &AgentRunRequest) -> Result<String, String>;

    /// Exécute l'agent dans la session préparée. `cancel` est le jeton que
    /// l'exécuteur déclenche sur dépassement de budget ou annulation.
    async fn run(
        &self,
        request: AgentRunRequest,
        session_id: &str,
        cancel: CancellationToken,
    ) -> Result<String, String>;

    /// Tokens cumulés de la session enfant, lus pour le budget `max_tokens`.
    async fn tokens_used(&self, _session_id: &str) -> i64 {
        0
    }

    /// L'issue de cet agent servie depuis un journal. `None` en exécution
    /// vivante : l'état se construit alors de `run` et des gardes de budget.
    /// `Some` court-circuite le lancement — c'est ce qui rend le rejeu d'un
    /// workflow hermétique.
    async fn recorded_outcome(&self, _request: &AgentRunRequest) -> Option<RecordedOutcome> {
        None
    }
}

pub struct SubagentRunner {
    session_manager: Arc<SessionManager>,
    use_login_shell_path: bool,
}

impl SubagentRunner {
    pub fn new(session_manager: Arc<SessionManager>) -> Self {
        Self {
            session_manager,
            use_login_shell_path: false,
        }
    }

    pub fn with_use_login_shell_path(mut self, use_login_shell_path: bool) -> Self {
        self.use_login_shell_path = use_login_shell_path;
        self
    }

    fn recipe(&self, request: &AgentRunRequest) -> Result<Recipe, String> {
        match &request.source {
            AgentSource::Prompt(prompt) => Recipe::builder()
                .version("1.0.0")
                .title(format!("Workflow: {}", request.label()))
                .description(format!("Agent « {} » du workflow", request.agent))
                .prompt(format!("{prompt}{}", request.inputs_block()))
                .build()
                .map_err(|error| error.to_string()),
            AgentSource::Recipe(path) => {
                let recipe = request.recipe.as_ref().ok_or_else(|| {
                    format!("recette « {} » non résolue par l'exécuteur", path.display())
                })?;
                let parameters: Vec<(String, String)> = request
                    .inputs
                    .iter()
                    .map(|(name, value)| (name.clone(), value.clone()))
                    .collect();
                build_recipe_from_template(
                    recipe.content.clone(),
                    &recipe.parent_dir,
                    parameters,
                    None::<fn(&str, &str) -> Result<String, anyhow::Error>>,
                )
                .map_err(|error| error.to_string())
            }
        }
    }
}

#[async_trait]
impl AgentRunner for SubagentRunner {
    async fn prepare(&self, request: &AgentRunRequest) -> Result<String, String> {
        let session = self
            .session_manager
            .create_session(
                request.working_dir.clone(),
                request.label(),
                SessionType::SubAgent,
                KajiMode::Auto,
            )
            .await
            .map_err(|error| error.to_string())?;

        self.session_manager
            .update(&session.id)
            .parent_session_id(Some(request.parent_session_id.clone()))
            .apply()
            .await
            .map_err(|error| error.to_string())?;

        Ok(session.id)
    }

    async fn run(
        &self,
        request: AgentRunRequest,
        session_id: &str,
        cancel: CancellationToken,
    ) -> Result<String, String> {
        let recipe = self.recipe(&request)?;

        let parent = self
            .session_manager
            .get_session(&request.parent_session_id, false)
            .await
            .map_err(|error| error.to_string())?;

        let provider_name = parent
            .provider_name
            .clone()
            .ok_or_else(|| "aucun provider configuré sur la session parente".to_string())?;

        let mut model_config = match parent.model_config.clone() {
            Some(config) => config,
            None => crate::model_config::model_config_from_user_config(&provider_name, "default")
                .map_err(|error| error.to_string())?,
        };
        if let Some(model) = request.model.as_ref() {
            if model != &model_config.model_name {
                model_config =
                    crate::model_config::model_config_from_user_config_with_session_settings(
                        &provider_name,
                        model,
                        Some(&model_config),
                        None,
                        None,
                    )
                    .map_err(|error| error.to_string())?;
            }
        }

        let extensions = EnabledExtensionsState::extensions_or_default(
            Some(&parent.extension_data),
            Config::global(),
        );
        let provider = providers::get_from_registry(&provider_name)
            .await
            .map_err(|error| error.to_string())?
            .create(extensions.clone())
            .await
            .map_err(|error| error.to_string())?;

        let task_config = TaskConfig::new(
            provider,
            model_config,
            &request.parent_session_id,
            &request.working_dir,
            extensions,
        );

        // Auto comme summon : un sous-agent n'a pas de canal d'approbation
        // remonté au parent, tout autre mode le bloquerait sur sa confirmation.
        let agent_config = AgentConfig::new(
            Arc::clone(&self.session_manager),
            PermissionManager::instance(),
            None,
            KajiMode::Auto,
            true,
            KajiPlatform::KajiCli,
        )
        .with_use_login_shell_path(self.use_login_shell_path);

        run_subagent_task(SubagentRunParams {
            config: agent_config,
            recipe,
            task_config,
            return_last_only: true,
            session_id: session_id.to_string(),
            cancellation_token: Some(cancel),
            on_message: None,
            notification_tx: None,
        })
        .await
        .map_err(|error| error.to_string())
    }

    async fn tokens_used(&self, session_id: &str) -> i64 {
        self.session_manager
            .get_session_usage_totals(session_id)
            .await
            .ok()
            .and_then(|totals| totals.accumulated_usage.total_tokens)
            .unwrap_or(0) as i64
    }
}

/// Le runner du rejeu : il sert la session enfant, la sortie et l'issue de
/// chaque agent depuis `agent_done` / `workflow_artifact`, et ne lance **aucun**
/// sous-agent. Sans lui, rejouer la session parente d'un workflow relancerait
/// de vrais agents, de vrais appels LLM et produirait des sorties différentes —
/// les entrées substituées des stages descendants divergeraient en silence.
///
/// Une clé absente du journal est une erreur nommée, jamais un retour au
/// lancement : un journal purgé de ses artefacts ne rejoue pas, il le dit.
pub struct ReplayRunner {
    agents: HashMap<(String, String), AgentDone>,
    artifacts: HashMap<(String, String), String>,
}

impl ReplayRunner {
    pub fn new(
        agents: HashMap<(String, String), AgentDone>,
        artifacts: HashMap<(String, String), String>,
    ) -> Self {
        Self { agents, artifacts }
    }

    pub fn from_cursor(cursor: &EventCursor) -> Self {
        Self::new(
            cursor.workflow_agents.clone(),
            cursor.workflow_artifacts.clone(),
        )
    }

    fn recorded(&self, request: &AgentRunRequest) -> Option<&AgentDone> {
        self.agents
            .get(&(request.stage.clone(), request.agent.clone()))
    }
}

#[async_trait]
impl AgentRunner for ReplayRunner {
    async fn prepare(&self, request: &AgentRunRequest) -> Result<String, String> {
        let Some(done) = self.recorded(request) else {
            return Err(format!(
                "agent « {} » absent du journal : le rejeu ne lance aucun sous-agent",
                request.label()
            ));
        };
        match (&done.session_id, &done.state) {
            (Some(session_id), _) => Ok(session_id.clone()),
            // L'agent avait déjà échoué à la préparation à l'enregistrement :
            // le rejeu rejoue cet échec-là, avec son message.
            (None, AgentState::Failed(FailureCause::Error(error))) => Err(error.clone()),
            (None, state) => Err(format!(
                "agent « {} » enregistré {} sans session enfant",
                request.label(),
                state.label()
            )),
        }
    }

    async fn run(
        &self,
        request: AgentRunRequest,
        _session_id: &str,
        _cancel: CancellationToken,
    ) -> Result<String, String> {
        Err(format!(
            "agent « {} » : le rejeu sert le journal, il ne lance pas de sous-agent",
            request.label()
        ))
    }

    async fn tokens_used(&self, session_id: &str) -> i64 {
        self.agents
            .values()
            .find(|done| done.session_id.as_deref() == Some(session_id))
            .map(|done| done.tokens)
            .unwrap_or(0)
    }

    async fn recorded_outcome(&self, request: &AgentRunRequest) -> Option<RecordedOutcome> {
        let done = self.recorded(request)?;
        let key = (request.stage.clone(), request.agent.clone());
        let output = self.artifacts.get(&key).cloned();
        // Une sortie purgée sur un agent qui avait réussi ne se remplace pas
        // par du vide : les descendants la substituent dans leur prompt.
        let state = match (&done.state, &output) {
            (AgentState::Done, None) => AgentState::Failed(FailureCause::Error(format!(
                "sortie de « {}.{} » purgée du journal : le rejeu n'a rien à substituer",
                request.stage, request.agent
            ))),
            (state, _) => state.clone(),
        };
        Some(RecordedOutcome {
            state,
            output,
            tokens: done.tokens,
        })
    }
}
