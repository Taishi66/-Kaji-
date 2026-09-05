//! Quels tours du journal un rejeu redéroule, et avec quel message.
//!
//! `kaji replay` réinjecte le message qui a ouvert chaque tour enregistré ; le
//! doré de bout en bout rejoue la même session par le même plan. Les deux
//! passent donc par ce module plutôt que par deux extractions parallèles — une
//! divergence entre le plan du CLI et celui des tests rendrait le doré aveugle
//! à la logique réellement exécutée en production
//! (`docs/superpowers/specs/2026-08-27-event-log-v2-replay-exact-design.md`, S3).

use std::collections::{HashMap, HashSet};

use rmcp::model::Role;

use crate::conversation::message::Message;
use crate::session::session_manager::SessionEvent;

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
pub enum PlannedTurn {
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
pub fn replay_plan(events: &[SessionEvent], until: Option<i64>) -> Vec<(i64, PlannedTurn)> {
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
    use crate::conversation::message::MessageContentBlock;

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
}
