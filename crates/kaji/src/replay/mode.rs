//! Le mode d'exécution d'un tour rejoué.
//!
//! Porté par l'`Agent` (`set_replay_mode`), lu par l'enveloppe `reply()` et
//! par les deux boucles via `Agent::is_replay()`. Sa présence seule rend le
//! tour hermétique : aucune écriture dans le journal qu'il relit, ni dans la
//! mémoire P1, l'usage ledger ou les checkpoints
//! (`docs/superpowers/specs/2026-08-27-event-log-v2-replay-exact-design.md`, S3).

use kaji_providers::model::ModelConfig;

use crate::config::KajiMode;
use crate::session::Session;

/// Le tour rejoue `source_session_id`. `lenient` laisse le replay continuer
/// sur divergence au lieu de s'arrêter ; `until_turn` borne le replay au
/// `turn_seq` donné (`None` = jusqu'au dernier tour du log). `kaji_mode` est
/// celui de la session **enregistrée** : le prompt système en dépend
/// (`is_autonomous`, branche `Chat`), donc la requête hachée aussi — le porter
/// ici plutôt que de le reposer au CLI rend l'oubli impossible.
#[derive(Clone, Debug)]
pub struct ReplayMode {
    pub source_session_id: String,
    pub lenient: bool,
    pub until_turn: Option<i64>,
    pub kaji_mode: KajiMode,
    /// Le `ModelConfig` de la session enregistrée. `toolshim` vide la liste
    /// d'outils envoyée au provider, réécrit le prompt système et convertit les
    /// messages, `context_limit` pilote les seuils : tout cela entre dans la
    /// requête hachée. En inventer un rendrait toute session toolshim
    /// irrejouable dès le tour 1, appel 0. `None` pour une session sans config
    /// persistée — le rejeu garde alors celui que l'appelant a monté.
    ///
    /// C'est la base, pas le dernier mot : le manifeste de chaque appel porte
    /// les champs qui entrent dans le prompt et le hash
    /// (`replay::manifest::TurnModelConfig`) et les recouvre, parce que la
    /// ligne `sessions` n'en garde qu'un exemplaire — celui du dernier tour.
    pub model_config: Option<ModelConfig>,
}

impl ReplayMode {
    pub fn new(source_session_id: String, kaji_mode: KajiMode) -> Self {
        Self {
            source_session_id,
            lenient: false,
            until_turn: None,
            kaji_mode,
            model_config: None,
        }
    }

    /// Le mode **et** le `ModelConfig` de la session enregistrée. Les deux
    /// vivent dans la session, pas dans le journal : une session déjà
    /// enregistrée porte donc déjà de quoi être rejouée fidèlement.
    pub fn for_session(session: &Session) -> Self {
        Self {
            source_session_id: session.id.clone(),
            lenient: false,
            until_turn: None,
            kaji_mode: session.kaji_mode,
            model_config: session.model_config.clone(),
        }
    }
}
