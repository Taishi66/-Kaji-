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

use std::collections::BTreeMap;
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
use crate::recipe::local_recipes::load_local_recipe_file;
use crate::recipe::Recipe;
use crate::session::extension_data::EnabledExtensionsState;
use crate::session::{SessionManager, SessionType};

#[derive(Debug, Clone)]
pub struct AgentRunRequest {
    pub stage: String,
    pub agent: String,
    pub source: AgentSource,
    pub model: Option<String>,
    /// Entrées déjà substituées : plus aucun `{{stage.agent.output}}` ici.
    pub inputs: BTreeMap<String, String>,
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
                let file = load_local_recipe_file(&path.to_string_lossy())
                    .map_err(|error| error.to_string())?;
                let parameters: Vec<(String, String)> = request
                    .inputs
                    .iter()
                    .map(|(name, value)| (name.clone(), value.clone()))
                    .collect();
                build_recipe_from_template(
                    file.content,
                    &file.parent_dir,
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
