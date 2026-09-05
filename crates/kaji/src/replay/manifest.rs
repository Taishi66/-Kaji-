//! L'environnement d'outils d'un tour, journalisé puis resservi au rejeu.
//!
//! Les extensions sont de l'état externe : serveurs MCP lancés hors du
//! processus, config utilisateur, `permission.yaml`. Un rejeu qui les
//! rechargerait reconstruirait l'environnement d'*aujourd'hui*, pas celui de
//! l'enregistrement — et `kaji replay` n'en charge aucune, donc rendrait
//! `tools = []` et un prompt système sans section extensions. Comme le bloc
//! mémoire et les lectures d'horloge, ce que les extensions ont mis dans la
//! requête est donc capturé à l'enregistrement et servi depuis le journal
//! (`docs/superpowers/specs/2026-08-27-event-log-v2-replay-exact-design.md`, S1).

use std::sync::Arc;

use kaji_providers::model::ModelConfig;
use rmcp::model::Tool;
use serde::{Deserialize, Serialize};

use crate::agents::extension::ExtensionInfo;
use crate::replay::record::{record_tool_manifest, TurnRecorder};
use crate::replay::source::ReplaySource;

/// Ce que l'environnement d'extensions a présenté au modèle pour un tour : les
/// outils envoyés au provider et les fragments de prompt système qui en
/// dérivent. Les deux boucles n'en remplissent pas les mêmes champs — la
/// legacy passe par `PromptBuilder` (`extensions` + compteurs), la machine à
/// états par `build_system_prompt` (`prompt_parts`) — et une session
/// enregistrée par une boucle est rejouée par la même, donc chacune ne relit
/// que ce qu'elle a écrit.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolManifest {
    #[serde(default)]
    pub tools: Vec<Tool>,
    #[serde(default)]
    pub prompt_parts: Vec<(String, String)>,
    #[serde(default)]
    pub extensions: Vec<ExtensionInfo>,
    #[serde(default)]
    pub extension_count: usize,
    #[serde(default)]
    pub tool_count: usize,
    /// Vrai quand l'extension code-execution était active. La boucle legacy
    /// pousse ce drapeau dans le prompt système (`with_code_execution_mode`)
    /// sans qu'il transparaisse dans la liste d'outils ; la machine à états ne
    /// s'en sert que pour préparer les outils, dont la sortie est déjà là.
    #[serde(default)]
    pub code_execution_active: bool,
    /// Les instructions des extensions frontend, que la boucle legacy lit
    /// vivantes au moment du build. La machine à états les pousse dans
    /// `prompt_parts` avant la capture.
    #[serde(default)]
    pub frontend_instructions: Option<String>,
    /// Le bloc de hints du working dir (`AGENTS.md`, `.kajihints`, …) assemblé
    /// au moment de l'appel. Les deux boucles le relisent du disque à chaque
    /// build : un `git pull` entre l'enregistrement et le rejeu le change.
    #[serde(default)]
    pub hints: Option<String>,
    /// Ce que le `ModelConfig` de l'appel a changé au prompt et à la liste
    /// d'outils. `None` pour un journal antérieur à ce champ : le rejeu retombe
    /// alors sur le `ModelConfig` de la session enregistrée.
    #[serde(default)]
    pub model_config: Option<TurnModelConfig>,
}

/// Les champs du `ModelConfig` qui entrent dans la requête hachée ou dans les
/// seuils du tour. Le `ModelConfig` complet vit dans la ligne `sessions`, donc
/// en un seul exemplaire : un changement de modèle en cours de session écrase
/// celui sous lequel les tours précédents ont été assemblés. Journaliser ces
/// champs par appel rend chaque tour rejouable sous sa propre config.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TurnModelConfig {
    #[serde(default)]
    pub toolshim: bool,
    #[serde(default)]
    pub toolshim_model: Option<String>,
    #[serde(default)]
    pub context_limit: Option<usize>,
}

impl TurnModelConfig {
    pub fn of(model_config: &ModelConfig) -> Self {
        Self {
            toolshim: model_config.toolshim,
            toolshim_model: model_config.toolshim_model.clone(),
            context_limit: model_config.context_limit,
        }
    }

    fn applied_to(&self, base: &ModelConfig) -> ModelConfig {
        ModelConfig {
            toolshim: self.toolshim,
            toolshim_model: self.toolshim_model.clone(),
            context_limit: self.context_limit,
            ..base.clone()
        }
    }
}

impl ToolManifest {
    /// La config sous laquelle assembler l'appel : celle du journal quand il la
    /// porte, celle de `base` sinon.
    pub fn model_config_over(&self, base: &ModelConfig) -> ModelConfig {
        match &self.model_config {
            Some(turn) => turn.applied_to(base),
            None => base.clone(),
        }
    }
}

/// Le manifeste du tour : servi depuis le journal en rejeu, journalisé sinon.
/// Un tour rejoué dont le journal ne porte pas de manifeste rend un manifeste
/// vide — le hash de requête divergera et nommera le tour, plutôt que de faire
/// passer l'environnement courant pour celui de l'enregistrement.
pub async fn turn_tool_manifest(
    recorder: Option<&Arc<TurnRecorder>>,
    replay: Option<&ReplaySource>,
    live: ToolManifest,
) -> ToolManifest {
    if let Some(replay) = replay {
        return replay.tool_manifest().unwrap_or_default();
    }
    record_tool_manifest(recorder, &live).await;
    live
}
