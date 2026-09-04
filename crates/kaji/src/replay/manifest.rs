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
