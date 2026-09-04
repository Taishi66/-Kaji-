//! Combien de temps le journal v2 garde de quoi rejouer.
//!
//! Le journal mêle deux natures d'events. La structure d'un tour — bornes,
//! messages, usage, approbations, checkpoints, notifications MCP,
//! remplacements d'historique, `log_meta`, `condense_triggered` — est
//! l'historique de la session : permanente, et petite. Les payloads du rejeu
//! — requêtes et réponses LLM, résultats d'outils, bloc mémoire, manifeste
//! d'outils, lectures d'horloge — ne servent qu'à rejouer ce tour à
//! l'identique ; ce sont eux
//! qui pèsent, et eux seuls que la rétention efface.
//!
//! `KAJI_REPLAY_RETENTION_DAYS` règle la fenêtre, en jours : `30` par défaut,
//! `0` purge tout au prochain démarrage, une valeur négative ne purge jamais.
//! Une session amputée de ses payloads est marquée `replayable = 0` : le rejeu
//! la refuse (`ReplayUnavailable::Purged`) au lieu de rejouer un journal troué.

use chrono::Utc;

use crate::config::Config;

pub const DEFAULT_RETENTION_DAYS: i64 = 30;
pub const RETENTION_DAYS_KEY: &str = "KAJI_REPLAY_RETENTION_DAYS";

/// Les kinds effacés par la rétention. Tout kind absent de cette liste est
/// permanent — la purge ne le voit jamais.
pub const PURGEABLE_KINDS: [&str; 7] = [
    "llm_request",
    "llm_response",
    "tool_result",
    "memory_block",
    "tool_manifest",
    "condense_summary",
    "clock_reads",
];

const MS_PER_DAY: i64 = 24 * 60 * 60 * 1000;

pub fn retention_days() -> i64 {
    Config::global()
        .get_param::<i64>(RETENTION_DAYS_KEY)
        .unwrap_or(DEFAULT_RETENTION_DAYS)
}

/// L'instant avant lequel un payload est purgeable, `None` quand la rétention
/// est négative — jamais purger. Une rétention de `0` jour purge tout, y
/// compris ce qui vient d'être écrit, d'où la borne maximale plutôt que
/// « maintenant » : sinon un event de la milliseconde courante y survivrait.
#[allow(clippy::disallowed_methods)] // rétention : maintenance au boot, hors boucle agent
pub fn cutoff_ms(retention_days: i64) -> Option<i64> {
    match retention_days {
        days if days < 0 => None,
        0 => Some(i64::MAX),
        days => Some(Utc::now().timestamp_millis() - days * MS_PER_DAY),
    }
}
