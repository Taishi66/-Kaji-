//! D'où vient la décision d'une gate `approve`.
//!
//! Deux sources, une seule interface : [`LiveGates`] attend une décision
//! humaine (`WorkflowHandle::approve/deny`), [`ReplayGates`] la sert depuis le
//! journal v2. Le rejeu ne redemande **jamais** une approbation — c'est la
//! règle replay d'AGENTS.md appliquée à l'orchestration : l'approbation est de
//! l'état externe, elle est capturée puis servie.
//!
//! Une gate peut aussi n'avoir **aucune** décision au journal : l'annulation du
//! workflow est tombée pendant qu'elle attendait. Le journal le dit tout de
//! même, par l'issue que `workflow_done` a figée — le rejeu rejoue alors
//! l'annulation à ce point plutôt que d'inventer une décision ou de crier à la
//! divergence. Hors d'un run annulé, une gate absente reste une vraie
//! divergence.

use std::collections::HashMap;
use std::sync::Mutex;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tracing::warn;

use crate::replay::cursor::EventCursor;
use crate::workflow::state::{WorkflowOutcome, WorkflowState};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateDecision {
    Approve,
    Deny,
}

impl GateDecision {
    pub fn approved(self) -> bool {
        self == GateDecision::Approve
    }

    pub fn label(self) -> &'static str {
        match self {
            GateDecision::Approve => "approuvée",
            GateDecision::Deny => "refusée",
        }
    }
}

/// Ce qu'une décision posée par [`crate::workflow::WorkflowHandle`] a produit.
/// Quatre cas et non un booléen : « ce workflow prend des décisions vivantes »
/// ne dit pas si la décision servira, et T6 ne doit afficher « approuvé » ni
/// sur un stage mort, ni sur un stage qui n'a pas de gate à franchir.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateVerdict {
    /// Décision enregistrée : le stage la lira quand il ouvrira sa gate, ou
    /// l'a déjà sous les yeux.
    Applied,
    /// Aucun stage de ce nom dans le workflow.
    UnknownStage,
    /// Le stage existe et n'est pas terminal, mais il est en `gate: auto` :
    /// il ne consultera jamais de décision, l'enregistrer serait mentir.
    NoGate,
    /// Le stage ne prendra plus de décision vivante : il est terminal, ou le
    /// workflow rejoue (ses gates viennent du journal).
    Settled,
}

impl GateVerdict {
    pub fn applied(&self) -> bool {
        *self == GateVerdict::Applied
    }

    pub fn label(&self) -> &'static str {
        match self {
            GateVerdict::Applied => "décision enregistrée",
            GateVerdict::UnknownStage => "stage inconnu",
            GateVerdict::NoGate => "stage sans gate",
            GateVerdict::Settled => "stage déjà terminal ou rejeu",
        }
    }
}

/// Ce qu'une source rend au stage qui ouvre sa gate. `Cancelled` n'est pas une
/// troisième décision : c'est l'absence de décision, expliquée — le workflow a
/// été annulé pendant l'attente, et le stage doit rejouer cette annulation, pas
/// en déduire un refus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateOutcome {
    Decided(GateDecision),
    Cancelled,
}

#[async_trait]
pub trait GateSource: Send + Sync {
    async fn decide(&self, stage: &str) -> Result<GateOutcome>;
}

/// Les décisions vivantes. Le compteur `watch` sert de réveil : un waiter
/// s'abonne **avant** de lire la table, donc une décision posée entre la
/// lecture et l'attente le réveille quand même.
pub struct LiveGates {
    decisions: Mutex<HashMap<String, GateDecision>>,
    version: watch::Sender<u64>,
}

impl Default for LiveGates {
    fn default() -> Self {
        let (version, _) = watch::channel(0);
        Self {
            decisions: Mutex::new(HashMap::new()),
            version,
        }
    }
}

impl LiveGates {
    pub fn record(&self, stage: &str, decision: GateDecision) {
        self.decisions
            .lock()
            .expect("gates empoisonnées")
            .insert(stage.to_string(), decision);
        self.version.send_modify(|version| *version += 1);
    }

    pub fn decided(&self, stage: &str) -> Option<GateDecision> {
        self.decisions
            .lock()
            .expect("gates empoisonnées")
            .get(stage)
            .copied()
    }
}

#[async_trait]
impl GateSource for LiveGates {
    async fn decide(&self, stage: &str) -> Result<GateOutcome> {
        let mut version = self.version.subscribe();
        loop {
            if let Some(decision) = self.decided(stage) {
                return Ok(GateOutcome::Decided(decision));
            }
            version.changed().await?;
        }
    }
}

/// Les décisions du journal, adressées par nom de stage. Un stage n'ouvre sa
/// gate qu'une fois par exécution et une session parente ne porte qu'un
/// workflow (`kaji workflow run` crée sa session) : le nom suffit comme clé.
pub struct ReplayGates {
    decisions: HashMap<String, GateDecision>,
    cancelled_run: bool,
    lenient: bool,
}

impl ReplayGates {
    pub fn new(decisions: HashMap<String, GateDecision>) -> Self {
        Self {
            decisions,
            cancelled_run: false,
            lenient: false,
        }
    }

    /// Les gates du journal, plus l'issue qu'il a figée : les deux se lisent
    /// ensemble, une gate sans décision ne veut pas dire la même chose selon
    /// que le run s'est terminé annulé ou non.
    pub fn from_cursor(cursor: &EventCursor) -> Self {
        Self::new(cursor.gate_decisions.clone()).after_a_cancelled_run(matches!(
            cursor.workflow_final.as_ref().map(WorkflowState::outcome),
            Some(WorkflowOutcome::Cancelled)
        ))
    }

    pub fn after_a_cancelled_run(mut self, cancelled_run: bool) -> Self {
        self.cancelled_run = cancelled_run;
        self
    }

    pub fn lenient(mut self, lenient: bool) -> Self {
        self.lenient = lenient;
        self
    }
}

#[async_trait]
impl GateSource for ReplayGates {
    async fn decide(&self, stage: &str) -> Result<GateOutcome> {
        match self.decisions.get(stage) {
            Some(decision) => Ok(GateOutcome::Decided(*decision)),
            // L'annulation passe avant `lenient` : elle n'est pas une tolérance
            // d'audit, c'est ce que le journal dit qu'il s'est passé.
            None if self.cancelled_run => Ok(GateOutcome::Cancelled),
            // Un rejeu ne peut pas inventer une approbation : en strict il
            // s'arrête, en lenient il refuse le stage et continue plutôt que
            // de laisser tourner des agents que personne n'a approuvés.
            None if self.lenient => {
                warn!(stage, "gate absente du journal : refusée (rejeu lenient)");
                Ok(GateOutcome::Decided(GateDecision::Deny))
            }
            None => Err(anyhow!(
                "gate « {stage} » absente du journal : le rejeu ne redemande jamais une approbation"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn a_live_gate_wakes_the_waiter_registered_before_the_decision() {
        let gates = Arc::new(LiveGates::default());
        let waiter = {
            let gates = Arc::clone(&gates);
            tokio::spawn(async move { gates.decide("revue").await.unwrap() })
        };

        tokio::task::yield_now().await;
        gates.record("revue", GateDecision::Approve);

        assert_eq!(
            waiter.await.unwrap(),
            GateOutcome::Decided(GateDecision::Approve)
        );
    }

    #[tokio::test]
    async fn a_replayed_gate_answers_without_a_live_decision() {
        let gates = ReplayGates::new(HashMap::from([("revue".to_string(), GateDecision::Deny)]));

        assert_eq!(
            gates.decide("revue").await.unwrap(),
            GateOutcome::Decided(GateDecision::Deny)
        );
        assert!(
            gates.decide("absente").await.is_err(),
            "un rejeu strict refuse d'inventer une décision"
        );
    }

    /// La gate manquante d'un run annulé n'est pas la même chose que la gate
    /// manquante d'un run abouti : la première est expliquée par le journal,
    /// la seconde est une divergence.
    #[tokio::test]
    async fn a_gate_missing_from_a_cancelled_run_replays_the_cancellation() {
        let cancelled = ReplayGates::new(HashMap::new()).after_a_cancelled_run(true);
        assert_eq!(
            cancelled.decide("revue").await.unwrap(),
            GateOutcome::Cancelled
        );

        assert!(
            ReplayGates::new(HashMap::new())
                .decide("revue")
                .await
                .is_err(),
            "hors annulation, une gate absente reste une divergence"
        );
    }

    /// `--lenient` sert à auditer des divergences en sortant vert. Une
    /// annulation n'en est pas une : elle doit se rejouer telle quelle, sinon
    /// l'audit lit « gate refusée » là où personne n'a rien refusé.
    #[tokio::test]
    async fn a_cancelled_run_outranks_the_lenient_fallback() {
        let gates = ReplayGates::new(HashMap::new())
            .after_a_cancelled_run(true)
            .lenient(true);

        assert_eq!(gates.decide("revue").await.unwrap(), GateOutcome::Cancelled);
    }
}
