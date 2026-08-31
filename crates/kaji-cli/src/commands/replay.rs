//! `kaji replay <session-id>` : rejoue une session enregistrée (event log v2)
//! turn par tour, en réinjectant les messages user du journal — la boucle
//! agent réelle tourne, ses entrées non-déterministes sont servies par
//! `ReplayProvider`/`ReplaySource` (`docs/superpowers/specs/2026-08-27-event-log-v2-replay-exact-design.md`, S3).
//!
//! Codes de sortie : `0` rejeu mené à son terme — y compris avec des
//! divergences tolérées par `--lenient`, signalées tour par tour et comptées
//! en fin de rejeu, le mode existant justement pour les auditer ; `2`
//! divergence fatale pendant un tour (hash de requête en strict, clé absente
//! ou chunk illisible dans les deux modes), `3` journal tronqué au chargement
//! (dernier tour sans `turn_end`, sans `--until` suffisamment bas pour
//! l'éviter), `4` session non disponible pour le rejeu (pré-v2 ou purgée).
//! Une session introuvable, ou toute autre erreur inattendue, remonte via
//! `anyhow` — code `1` par le traitement par défaut de `main`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::{Context, Result};
use futures::StreamExt;
use kaji::agents::{Agent, AgentEvent, SessionConfig};
use kaji::config::Config;
use kaji::conversation::message::Message;
use kaji::replay::cursor::{EventCursor, ReplayUnavailable};
use kaji::replay::idgen::SessionIdGen;
use kaji::replay::mode::ReplayMode;
use kaji::replay::provider::ReplayProvider;
use kaji::replay::source::ReplaySource;
use kaji::session::session_manager::{SessionEvent, SessionType};
use kaji_providers::model::ModelConfig;
use rmcp::model::Role;

const EXIT_DIVERGENCE: i32 = 2;
const EXIT_TRUNCATED: i32 = 3;
const EXIT_UNAVAILABLE: i32 = 4;

const DEFAULT_RETENTION_DAYS: i64 = 30;
const TRANSCRIPT_MAX_CHARS: usize = 200;

pub async fn handle_replay_subcommand(
    session_id: String,
    until: Option<i64>,
    lenient: bool,
) -> Result<()> {
    let mut agent = Agent::new();
    let session_manager = agent.config.session_manager.clone();

    let source = session_manager
        .get_session(&session_id, false)
        .await
        .with_context(|| format!("chargement de la session « {session_id} »"))?;

    let cursor = match EventCursor::load_until(&session_manager, &session_id, until).await {
        Ok(cursor) => cursor,
        Err(error) => {
            let Some(unavailable) = error.downcast_ref::<ReplayUnavailable>() else {
                return Err(error);
            };
            let (message, code) = unavailable_report(unavailable, retention_days());
            eprintln!("kaji replay : {message}");
            std::process::exit(code);
        }
    };

    let events = session_manager.session_events(&session_id).await?;
    let plan = replay_plan(&events, until);
    if !plan
        .iter()
        .any(|(_, planned)| matches!(planned, PlannedTurn::Replay(_)))
    {
        println!("kaji replay : aucun tour à rejouer dans « {session_id} »");
        return Ok(());
    }

    let derived = session_manager
        .create_session(
            source.working_dir.clone(),
            format!("replay-of-{session_id}"),
            SessionType::Hidden,
            source.kaji_mode,
        )
        .await?;
    session_manager
        .update(&derived.id)
        .parent_session_id(Some(session_id.clone()))
        .apply()
        .await?;

    let cursor = Arc::new(cursor);
    let provider = ReplayProvider::new(Arc::clone(&cursor), lenient);
    let position = provider.position();
    let divergences = provider.divergences();

    let mut replay_mode = ReplayMode::new(session_id.clone());
    replay_mode.lenient = lenient;
    replay_mode.until_turn = until;

    agent.set_idgen(Arc::new(SessionIdGen::new(&cursor.log_meta.idgen_seed)));
    agent.set_replay_mode(replay_mode);
    agent.set_replay_source(ReplaySource::new(
        Arc::clone(&cursor),
        Arc::clone(&position),
    ));
    agent
        .update_provider(
            Arc::new(provider),
            ModelConfig::new("kaji-replay"),
            &derived.id,
        )
        .await?;

    println!(
        "kaji replay : rejeu de « {session_id} » → session dérivée « {} »",
        derived.id
    );

    let mut replayed = 0;
    let mut skipped = 0;
    let mut tolerated = 0;
    for (turn_seq, planned) in plan {
        let PlannedTurn::Replay(user_message) = planned else {
            skipped += 1;
            println!("[tour {turn_seq}] aucun message user enregistré — tour sauté");
            continue;
        };

        position.begin_turn(turn_seq);
        let outcome = replay_turn(&agent, &derived.id, turn_seq, user_message).await;
        for divergence in divergences.drain() {
            tolerated += 1;
            println!(
                "[tour {turn_seq}] divergence tolérée (appel {}) — requête enregistrée {}, rejouée {}",
                divergence.call_idx, divergence.recorded_hash, divergence.replayed_hash
            );
        }

        match outcome {
            Ok(lines) => {
                replayed += 1;
                for line in lines {
                    println!("{line}");
                }
            }
            Err(error) => {
                eprintln!("kaji replay : divergence au tour {turn_seq} — {error}");
                std::process::exit(EXIT_DIVERGENCE);
            }
        }
    }

    println!(
        "kaji replay : {}",
        replay_summary(replayed, skipped, tolerated)
    );
    Ok(())
}

/// La ligne de fin de rejeu. Elle ne promet la fidélité que lorsque aucune
/// divergence n'a été tolérée, et compte à part les tours que le journal n'a
/// pas de quoi rejouer — sinon « N tour(s) rejoué(s) » ne serait vérifiable
/// contre rien.
fn replay_summary(replayed: usize, skipped: usize, divergences: usize) -> String {
    let mut summary = format!("{replayed} tour(s) rejoué(s)");
    if divergences == 0 {
        summary.push_str(" sans divergence");
    } else {
        summary.push_str(&format!(", {divergences} divergence(s) tolérée(s)"));
    }
    if skipped > 0 {
        summary.push_str(&format!(
            ", {skipped} tour(s) sauté(s) faute de message user enregistré"
        ));
    }
    summary
}

/// Rejoue un tour jusqu'à son terme et rend chaque message produit sous forme
/// de ligne de transcription. Toute erreur du flux (hash divergent, clé
/// absente, chunk illisible) remonte telle quelle — l'appelant décide du code
/// de sortie.
async fn replay_turn(
    agent: &Agent,
    derived_session_id: &str,
    turn_seq: i64,
    user_message: Message,
) -> Result<Vec<String>> {
    let stream = agent
        .reply(
            user_message,
            replay_session_config(derived_session_id),
            None,
        )
        .await?;
    tokio::pin!(stream);

    let mut lines = Vec::new();
    while let Some(event) = stream.next().await {
        if let AgentEvent::Message(message) = event? {
            lines.push(render_message(turn_seq, &message));
        }
    }
    Ok(lines)
}

fn replay_session_config(session_id: &str) -> SessionConfig {
    SessionConfig {
        id: session_id.to_string(),
        schedule_id: None,
        max_turns: None,
        retry_config: None,
    }
}

fn render_message(turn_seq: i64, message: &Message) -> String {
    let role = match message.role {
        Role::User => "user",
        Role::Assistant => "assistant",
    };
    let content = message
        .content
        .iter()
        .map(|block| block.to_string())
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "[tour {turn_seq}] {role}: {}",
        truncate_chars(&content, TRANSCRIPT_MAX_CHARS)
    )
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_string()
    } else {
        let mut truncated: String = text.chars().take(max_chars).collect();
        truncated.push('…');
        truncated
    }
}

fn retention_days() -> i64 {
    Config::global()
        .get_param::<i64>("KAJI_REPLAY_RETENTION_DAYS")
        .unwrap_or(DEFAULT_RETENTION_DAYS)
}

/// Traduit un refus de rejeu en message humain et code de sortie dédié.
/// `Purged` n'a pas la durée de rétention dans son propre payload (elle vit
/// en config, purge par kind — Task 12) : on la relit ici plutôt que de la
/// faire porter par un variant qui ne l'a pas.
pub fn unavailable_report(error: &ReplayUnavailable, retention_days: i64) -> (String, i32) {
    match error {
        ReplayUnavailable::PreV2 => (
            "session enregistrée avant le replay v2 — son journal ne porte pas de quoi rejouer"
                .to_string(),
            EXIT_UNAVAILABLE,
        ),
        ReplayUnavailable::Purged => (
            format!(
                "payloads purgés (rétention {retention_days} j) — la session n'est plus rejouable"
            ),
            EXIT_UNAVAILABLE,
        ),
        ReplayUnavailable::TruncatedAt(turn) => {
            let last_complete = turn - 1;
            let message = if last_complete >= 1 {
                format!(
                    "log tronqué au tour {turn} — replay jusqu'au tour {last_complete} possible \
                     avec --until {last_complete}"
                )
            } else {
                format!("log tronqué au tour {turn} — aucun tour complet à rejouer")
            };
            (message, EXIT_TRUNCATED)
        }
    }
}

/// Le message user de chaque tour, tel qu'enregistré : la boucle journalise
/// `AgentEvent::Message(user_message)` en tout premier événement d'un tour
/// (`agents/agent.rs`, enveloppe de `reply_impl`), donc le premier event
/// `message` de rôle user rencontré pour un `turn_seq` donné est le message
/// qui a ouvert ce tour — une injection ultérieure (steer) n'est jamais
/// rejouée par cette extraction. Un payload illisible est ignoré, pas fatal :
/// une session enregistrée par une version de kaji au format différent ne
/// doit pas paniquer le CLI.
pub fn user_turns(events: &[SessionEvent]) -> Vec<(i64, Message)> {
    let mut opened: HashSet<i64> = HashSet::new();
    let mut turns = Vec::new();
    for event in events {
        if event.kind != "message" {
            continue;
        }
        let Ok(message) = serde_json::from_str::<Message>(&event.payload_json) else {
            continue;
        };
        if !matches!(message.role, Role::User) || !opened.insert(event.turn_seq) {
            continue;
        }
        turns.push((event.turn_seq, message));
    }
    turns
}

/// Ce que le rejeu fait d'un tour du journal.
enum PlannedTurn {
    Replay(Message),
    Skipped,
}

/// Le plan de rejeu : chaque tour ouvert dans le journal, borné par `until`,
/// avec le message qui le rejouera quand il y en a un. Un tour peut n'avoir
/// aucune row `message` — une réponse d'élicitation ou un message non
/// agent-visible laisse `turn_start`/`turn_end` derrière lui sans que la
/// boucle ait émis le moindre `AgentEvent::Message` (`agents/agent.rs`,
/// retours en stream vide de `reply_impl`). Ces tours-là sont marqués sautés
/// plutôt qu'écartés en silence : rien ne les rejoue, mais l'opérateur voit
/// que le journal en portait un de plus que ce qui a été rejoué.
fn replay_plan(events: &[SessionEvent], until: Option<i64>) -> Vec<(i64, PlannedTurn)> {
    let mut messages: HashMap<i64, Message> = user_turns(events).into_iter().collect();
    let mut turn_seqs: Vec<i64> = events
        .iter()
        .filter(|event| event.kind == "turn_start")
        .map(|event| event.turn_seq)
        .chain(messages.keys().copied())
        .filter(|turn_seq| until.is_none_or(|until| *turn_seq <= until))
        .collect();
    turn_seqs.sort_unstable();
    turn_seqs.dedup();

    turn_seqs
        .into_iter()
        .map(|turn_seq| match messages.remove(&turn_seq) {
            Some(message) => (turn_seq, PlannedTurn::Replay(message)),
            None => (turn_seq, PlannedTurn::Skipped),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaji::conversation::message::MessageContentBlock;

    fn event(turn_seq: i64, kind: &str, payload_json: &str) -> SessionEvent {
        SessionEvent {
            id: turn_seq,
            turn_seq,
            ts_ms: 0,
            kind: kind.to_string(),
            payload_json: payload_json.to_string(),
        }
    }

    /// Un payload de row `message` tel que la boucle l'écrit : la sérialisation
    /// du `Message` lui-même (`agent_event_payload`). Sérialisé plutôt
    /// qu'écrit à la main pour que l'extraction soit testée sur le vrai
    /// aller-retour du format, pas sur une approximation.
    fn message_payload(role: Role, text: &str) -> String {
        serde_json::to_string(&Message::new(
            role,
            0,
            vec![MessageContentBlock::text(text)],
        ))
        .expect("a message always serializes")
    }

    fn user_message_payload(text: &str) -> String {
        message_payload(Role::User, text)
    }

    fn assistant_message_payload(text: &str) -> String {
        message_payload(Role::Assistant, text)
    }

    #[test]
    fn pre_v2_maps_to_unavailable() {
        let (message, code) = unavailable_report(&ReplayUnavailable::PreV2, 30);
        assert_eq!(code, EXIT_UNAVAILABLE);
        assert!(message.contains("avant le replay v2"), "{message}");
    }

    #[test]
    fn purged_carries_the_configured_retention() {
        let (message, code) = unavailable_report(&ReplayUnavailable::Purged, 45);
        assert_eq!(code, EXIT_UNAVAILABLE);
        assert!(message.contains("45"), "{message}");
        assert!(message.contains("purgés"), "{message}");
    }

    #[test]
    fn truncated_at_names_the_turn_and_the_until_workaround() {
        let (message, code) = unavailable_report(&ReplayUnavailable::TruncatedAt(5), 30);
        assert_eq!(code, EXIT_TRUNCATED);
        assert!(message.contains("tour 5"), "{message}");
        assert!(message.contains("--until"), "{message}");
        assert!(message.contains("tour 4"), "{message}: N-1 doit être nommé");
    }

    #[test]
    fn a_truncation_on_the_first_turn_promises_no_replayable_prefix() {
        let (message, code) = unavailable_report(&ReplayUnavailable::TruncatedAt(1), 30);
        assert_eq!(code, EXIT_TRUNCATED);
        assert!(!message.contains("--until"), "{message}");
    }

    #[test]
    fn extracts_the_opening_user_message_of_each_turn() {
        let events = vec![
            event(1, "turn_start", "{}"),
            event(1, "message", &user_message_payload("premier tour")),
            event(1, "message", &assistant_message_payload("réponse 1")),
            event(1, "turn_end", "{}"),
            event(2, "turn_start", "{}"),
            event(2, "message", &user_message_payload("second tour")),
            event(2, "turn_end", "{}"),
        ];

        let turns = user_turns(&events);
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].0, 1);
        assert_eq!(turns[1].0, 2);
        assert_eq!(
            turns[0].1.content.first().map(|c| c.to_string()),
            Some("premier tour".to_string())
        );
        assert_eq!(
            turns[1].1.content.first().map(|c| c.to_string()),
            Some("second tour".to_string())
        );
    }

    #[test]
    fn a_later_user_role_message_in_the_same_turn_does_not_shadow_the_opening_one() {
        let events = vec![
            event(1, "message", &user_message_payload("query d'ouverture")),
            event(
                1,
                "message",
                &user_message_payload("steer injecté plus tard"),
            ),
        ];

        let turns = user_turns(&events);
        assert_eq!(turns.len(), 1);
        assert_eq!(
            turns[0].1.content.first().map(|c| c.to_string()),
            Some("query d'ouverture".to_string())
        );
    }

    #[test]
    fn non_message_and_unreadable_payloads_are_skipped_not_fatal() {
        let events = vec![
            event(1, "usage", "{}"),
            event(1, "message", "not json"),
            event(1, "message", &user_message_payload("seul tour valide")),
        ];

        let turns = user_turns(&events);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].0, 1);
    }

    fn planned_turns(events: &[SessionEvent], until: Option<i64>) -> Vec<(i64, bool)> {
        replay_plan(events, until)
            .iter()
            .map(|(turn_seq, planned)| (*turn_seq, matches!(planned, PlannedTurn::Replay(_))))
            .collect()
    }

    #[test]
    fn until_bounds_which_turns_are_replayed() {
        let events = vec![
            event(1, "message", &user_message_payload("tour 1")),
            event(2, "message", &user_message_payload("tour 2")),
            event(3, "message", &user_message_payload("tour 3")),
        ];

        assert_eq!(
            planned_turns(&events, None),
            vec![(1, true), (2, true), (3, true)]
        );
        assert_eq!(planned_turns(&events, Some(2)), vec![(1, true), (2, true)]);
        assert!(planned_turns(&events, Some(0)).is_empty());
    }

    #[test]
    fn a_turn_without_a_user_message_is_planned_as_skipped() {
        let events = vec![
            event(1, "turn_start", "{}"),
            event(1, "message", &user_message_payload("tour 1")),
            event(1, "turn_end", "{}"),
            event(2, "turn_start", "{}"),
            event(2, "clock_reads", "{}"),
            event(2, "turn_end", "{}"),
            event(3, "turn_start", "{}"),
            event(3, "message", &user_message_payload("tour 3")),
            event(3, "turn_end", "{}"),
        ];

        assert_eq!(
            planned_turns(&events, None),
            vec![(1, true), (2, false), (3, true)],
            "un tour ouvert sans message user est signalé, pas effacé du plan"
        );
        assert_eq!(
            planned_turns(&events, Some(2)),
            vec![(1, true), (2, false)],
            "--until borne aussi les tours sautés"
        );
    }

    #[test]
    fn the_summary_counts_tolerated_divergences_instead_of_claiming_fidelity() {
        let summary = replay_summary(3, 0, 2);
        assert!(summary.contains('3'), "{summary}");
        assert!(summary.contains('2'), "{summary}");
        assert!(
            !summary.contains("sans divergence"),
            "{summary}: le rejeu a divergé"
        );
    }

    #[test]
    fn the_summary_claims_fidelity_only_without_divergence() {
        let summary = replay_summary(3, 0, 0);
        assert!(summary.contains("sans divergence"), "{summary}");
    }

    #[test]
    fn the_summary_distinguishes_replayed_from_skipped_turns() {
        let summary = replay_summary(2, 1, 0);
        assert!(summary.contains("2 tour(s) rejoué(s)"), "{summary}");
        assert!(summary.contains("1 tour(s) sauté(s)"), "{summary}");
    }
}
