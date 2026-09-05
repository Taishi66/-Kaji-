//! Ce qu'un tour rejoué lit à la place de ce qu'il ferait vivre.
//!
//! `ReplayMode` dit qu'un tour rejoue ; `ReplaySource` dit *quoi*. Il réunit le
//! journal indexé (`EventCursor`) et la position du rejeu (`ReplayPosition`,
//! partagée avec le `ReplayProvider`) : le tour ouvert par `begin_turn` désigne
//! ainsi la même trace pour l'horloge, le bloc mémoire, la compaction et les
//! approbations que pour les appels LLM — une seule ouverture de tour, aucune
//! seconde source de vérité
//! (`docs/superpowers/specs/2026-08-27-event-log-v2-replay-exact-design.md`, S3).

use std::sync::Arc;

use tracing::warn;

use crate::conversation::message::{Message, MessageContent};
use crate::permission::Permission;
use crate::replay::cursor::EventCursor;
use crate::replay::manifest::ToolManifest;
use crate::replay::provider::ReplayPosition;

#[derive(Clone)]
pub struct ReplaySource {
    cursor: Arc<EventCursor>,
    position: Arc<ReplayPosition>,
}

impl ReplaySource {
    pub fn new(cursor: Arc<EventCursor>, position: Arc<ReplayPosition>) -> Self {
        Self { cursor, position }
    }

    pub fn cursor(&self) -> Arc<EventCursor> {
        Arc::clone(&self.cursor)
    }

    /// Le tour du journal en cours de rejeu.
    pub fn turn(&self) -> i64 {
        self.position.turn()
    }

    /// Le bloc mémoire enregistré pour ce tour. Absent ⇒ pas de bloc, comme à
    /// l'enregistrement : le splice réel n'est jamais rejoué.
    pub fn memory_block(&self) -> Option<String> {
        crate::replay::cursor::per_call(
            &self.cursor.memory_blocks,
            self.turn(),
            self.position.call(),
        )
        .cloned()
    }

    /// Le bloc `turn-context` enregistré pour l'appel que la boucle assemble.
    /// Absent ⇒ pas de bloc, comme un modèle sous le seuil
    /// `MIN_CONTEXT_FOR_MOIM`.
    pub fn turn_context(&self) -> Option<String> {
        crate::replay::cursor::per_call(
            &self.cursor.turn_contexts,
            self.turn(),
            self.position.call(),
        )
        .cloned()
    }

    /// L'environnement d'extensions enregistré pour l'appel que la boucle
    /// assemble : outils envoyés au provider et fragments de prompt système qui
    /// en dérivent. Absent ⇒ aucun outil et aucun fragment, comme une session
    /// sans extension.
    pub fn tool_manifest(&self) -> Option<ToolManifest> {
        crate::replay::cursor::per_call(
            &self.cursor.tool_manifests,
            self.turn(),
            self.position.call(),
        )
        .cloned()
    }

    /// Vrai quand le journal porte un manifeste pour l'appel exact que la
    /// boucle s'apprête à faire, c'est-à-dire quand l'enregistrement a
    /// réassemblé son prompt devant cet appel-là. C'est cette présence — et non
    /// les fichiers de hints d'aujourd'hui — qui décide de la réassemblée au
    /// rejeu : un fichier supprimé depuis l'enregistrement supprimerait sinon
    /// l'appel qui les portait, un fichier ajouté en créerait un.
    pub fn reassembled_before_current_call(&self) -> bool {
        self.cursor
            .tool_manifests
            .contains_key(&(self.turn(), self.position.call()))
    }

    /// La réponse que la post-passe `toolshim` a rendue pour l'appel que la
    /// boucle s'apprête à faire. À lire avant `Provider::stream`, qui consomme
    /// l'index d'appel. Absente ⇒ session sans `toolshim`, ou journal antérieur
    /// à ce kind : la post-passe tourne alors comme avant. Lecture exacte, pas
    /// `per_call` — chaque appel produit la sienne, donc en hériter d'un appel
    /// précédent servirait une réponse d'un autre tour de boucle.
    pub fn toolshim_message(&self) -> Option<Message> {
        self.cursor
            .toolshim_messages
            .get(&(self.turn(), self.position.call()))
            .cloned()
    }

    /// L'estampille d'horloge que le prompt système portait à
    /// l'enregistrement. Le tour n'en lit qu'une.
    pub fn clock_read(&self) -> Option<String> {
        self.cursor.clock_reads.get(&self.turn())?.first().cloned()
    }

    /// Le résumé que la compaction a produit devant cet appel à
    /// l'enregistrement. Absent alors que le tour a compacté ⇒ le rejeu échoue
    /// plutôt que de résumer à nouveau avec un modèle vivant.
    pub fn condense_summary(&self) -> Option<Message> {
        crate::replay::cursor::per_call(
            &self.cursor.condense_summaries,
            self.turn(),
            self.position.call(),
        )
        .cloned()
    }

    /// Le résumé que l'enregistrement a substitué à cette paire d'outils.
    /// Absent alors que le tour rejoué veut résumer la paire ⇒ le rejeu échoue
    /// plutôt que d'appeler un modèle vivant.
    pub fn tool_pair_summary(&self, tool_call_id: &str) -> Option<Message> {
        self.cursor
            .tool_pair_summaries
            .get(&(self.turn(), tool_call_id.to_string()))
            .cloned()
    }

    /// Vrai quand le journal porte un résumé pour cette paire au tour rejoué.
    /// C'est cette présence, et non le seuil recalculé sur la machine de rejeu,
    /// qui décide qu'une paire est remplacée par son résumé (spec S3).
    pub fn has_tool_pair_summary(&self, tool_call_id: &str) -> bool {
        self.cursor
            .tool_pair_summaries
            .contains_key(&(self.turn(), tool_call_id.to_string()))
    }

    pub fn condensed(&self) -> bool {
        self.cursor.condense_turns.contains(&self.turn())
    }

    /// L'approbation enregistrée pour `request_id`. Absente ⇒ refus : un outil
    /// que le journal ne montre pas approuvé n'a pas tourné à
    /// l'enregistrement. `AllowOnce`/`DenyOnce` plutôt que la permission
    /// d'origine — le rejeu reproduit la décision d'exécution sans rejouer ses
    /// effets persistants (grants utilisateur, `NeverAllow`).
    pub fn approval(&self, request_id: &str) -> Permission {
        match self
            .cursor
            .approvals
            .get(&(self.turn(), request_id.to_string()))
        {
            Some(true) => Permission::AllowOnce,
            Some(false) => Permission::DenyOnce,
            None => {
                warn!(
                    turn_seq = self.turn(),
                    %request_id,
                    "replay: aucune approbation enregistrée pour cet appel — refusé"
                );
                Permission::DenyOnce
            }
        }
    }
}

/// La décision de compaction du tour. Au rejeu elle suit `condense_triggered`,
/// jamais le seuil recalculé : l'usage du tour rejoué n'est pas celui de
/// l'enregistrement (spec S3).
pub fn condense_decision(replay: Option<&ReplaySource>, live: bool) -> bool {
    match replay {
        Some(source) => source.condensed(),
        None => live,
    }
}

/// La réponse d'approbation que le journal a enregistrée pour `request_id`,
/// sous la forme que la boucle machine à états attend d'un client. `None` hors
/// rejeu — c'est l'utilisateur qui répond.
pub fn replayed_confirmation(replay: Option<&ReplaySource>, request_id: &str) -> Option<Message> {
    let permission = replay?.approval(request_id);
    Some(
        Message::user()
            .with_content(MessageContent::action_required_tool_confirmation_response(
                request_id, permission,
            ))
            .with_visibility(false, false),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replay::cursor::LogMeta;
    use std::collections::{HashMap, HashSet};

    fn open(cursor: EventCursor, turn: i64) -> ReplaySource {
        let position = Arc::new(ReplayPosition::default());
        position.begin_turn(turn);
        ReplaySource::new(Arc::new(cursor), position)
    }

    fn cursor() -> EventCursor {
        EventCursor {
            log_meta: LogMeta {
                kaji_version: "test".to_string(),
                schema_version: 17,
                idgen_seed: "seed".to_string(),
            },
            llm_responses: HashMap::new(),
            tool_results: HashMap::new(),
            memory_blocks: HashMap::from([((2, 0), "bloc du tour 2".to_string())]),
            turn_contexts: HashMap::from([(
                (2, 0),
                "<turn-context>tour 2</turn-context>".to_string(),
            )]),
            tool_manifests: HashMap::new(),
            toolshim_messages: HashMap::new(),
            clock_reads: HashMap::from([(2, vec!["2026-08-27 09:00 +00:00".to_string()])]),
            condense_turns: HashSet::from([3]),
            condense_summaries: HashMap::from([(
                (3, 0),
                Message::user().with_text("résumé de compaction du tour 3"),
            )]),
            tool_pair_summaries: HashMap::from([(
                (2, "call-allowed".to_string()),
                Message::user().with_text("résumé de la paire call-allowed"),
            )]),
            approvals: HashMap::from([
                ((2, "call-allowed".to_string()), true),
                ((2, "call-denied".to_string()), false),
            ]),
            gate_decisions: HashMap::new(),
            workflow_artifacts: HashMap::new(),
        }
    }

    #[test]
    fn the_open_turn_selects_what_is_served() {
        let source = open(cursor(), 2);
        assert_eq!(source.memory_block().as_deref(), Some("bloc du tour 2"));
        assert_eq!(
            source.turn_context().as_deref(),
            Some("<turn-context>tour 2</turn-context>")
        );
        assert_eq!(
            source.clock_read().as_deref(),
            Some("2026-08-27 09:00 +00:00")
        );

        let other_turn = open(cursor(), 5);
        assert_eq!(other_turn.memory_block(), None);
        assert_eq!(other_turn.turn_context(), None);
        assert_eq!(other_turn.clock_read(), None);
    }

    /// L'appel LLM de résumé passe par `Provider::complete`, hors du canal
    /// `(turn_seq, call_idx)` du provider : il a sa propre clé, et le tour
    /// ouvert la désigne comme il désigne le bloc mémoire.
    #[test]
    fn the_compaction_summary_is_served_for_the_turn_that_compacted() {
        let compacted = open(cursor(), 3);
        assert_eq!(
            compacted
                .condense_summary()
                .map(|summary| summary.as_concat_text()),
            Some("résumé de compaction du tour 3".to_string())
        );
        assert!(open(cursor(), 2).condense_summary().is_none());
    }

    /// Le résumé de paires d'outils mute la conversation persistée : il est
    /// servi depuis le journal, adressé par la paire qu'il remplace et par le
    /// tour qui l'a produit.
    #[test]
    fn the_tool_pair_summary_is_served_for_the_pair_that_was_summarized() {
        let source = open(cursor(), 2);
        assert_eq!(
            source
                .tool_pair_summary("call-allowed")
                .map(|summary| summary.as_concat_text()),
            Some("résumé de la paire call-allowed".to_string())
        );
        assert!(source.tool_pair_summary("call-denied").is_none());
        assert!(open(cursor(), 5)
            .tool_pair_summary("call-allowed")
            .is_none());
    }

    #[test]
    fn compaction_follows_the_log_only_when_replaying() {
        let replay = open(cursor(), 3);
        assert!(condense_decision(Some(&replay), false));

        let untouched_turn = open(cursor(), 2);
        assert!(!condense_decision(Some(&untouched_turn), true));

        assert!(condense_decision(None, true));
        assert!(!condense_decision(None, false));
    }

    #[test]
    fn an_unlogged_approval_denies() {
        let source = open(cursor(), 2);
        assert_eq!(source.approval("call-allowed"), Permission::AllowOnce);
        assert_eq!(source.approval("call-denied"), Permission::DenyOnce);
        assert_eq!(source.approval("call-unknown"), Permission::DenyOnce);
    }

    #[test]
    fn no_confirmation_is_replayed_outside_replay() {
        assert!(replayed_confirmation(None, "call-allowed").is_none());
        assert!(replayed_confirmation(Some(&open(cursor(), 2)), "call-allowed").is_some());
    }
}
