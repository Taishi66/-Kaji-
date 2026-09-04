//! Le mode d'exécution d'un tour rejoué.
//!
//! Porté par l'`Agent` (`set_replay_mode`), lu par l'enveloppe `reply()` et
//! par les deux boucles via `Agent::is_replay()`. Sa présence seule rend le
//! tour hermétique : aucune écriture dans le journal qu'il relit, ni dans la
//! mémoire P1, l'usage ledger ou les checkpoints
//! (`docs/superpowers/specs/2026-08-27-event-log-v2-replay-exact-design.md`, S3).

use crate::config::KajiMode;

/// Le tour rejoue `source_session_id`. `lenient` laisse le replay continuer
/// sur divergence au lieu de s'arrêter ; `until_turn` borne le replay au
/// `turn_seq` donné (`None` = jusqu'au dernier tour du log). `kaji_mode` est
/// celui de la session **enregistrée** : le prompt système en dépend
/// (`is_autonomous`, branche `Chat`), donc la requête hachée aussi — le porter
/// ici plutôt que de le reposer au CLI rend l'oubli impossible.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplayMode {
    pub source_session_id: String,
    pub lenient: bool,
    pub until_turn: Option<i64>,
    pub kaji_mode: KajiMode,
}

impl ReplayMode {
    pub fn new(source_session_id: String, kaji_mode: KajiMode) -> Self {
        Self {
            source_session_id,
            lenient: false,
            until_turn: None,
            kaji_mode,
        }
    }
}
