//! `kaji replay <session-id>` : rejoue une session enregistrée (event log v2)
//! turn par tour, en réinjectant les messages user du journal — la boucle
//! agent réelle tourne, ses entrées non-déterministes sont servies par
//! `ReplayProvider`/`ReplaySource` (`docs/superpowers/specs/2026-08-27-event-log-v2-replay-exact-design.md`, S3).
//!
//! Codes de sortie : `0` rejeu mené à son terme — y compris avec des
//! divergences tolérées par `--lenient`, signalées tour par tour et comptées
//! en fin de rejeu, le mode existant justement pour les auditer — y compris
//! une topologie de workflow différente de l'enregistrement ; `2` divergence
//! fatale pendant un tour (hash de requête ou topologie de workflow en strict,
//! clé absente ou chunk illisible dans les deux modes ; un tour de workflow
//! que le journal ne permet pas de redérouler y sort aussi, sous un autre
//! message), `3` journal tronqué au chargement
//! (dernier tour sans `turn_end`, sans `--until` suffisamment bas pour
//! l'éviter), `4` session non disponible pour le rejeu (pré-v2 ou purgée).
//! Une session introuvable, ou toute autre erreur inattendue, remonte via
//! `anyhow` — code `1` par le traitement par défaut de `main`.

use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use futures::StreamExt;
use kaji::agents::{Agent, AgentEvent, SessionConfig};
use kaji::conversation::message::Message;
use kaji::replay::cursor::{EventCursor, ReplayUnavailable};
use kaji::replay::idgen::SessionIdGen;
use kaji::replay::mode::ReplayMode;
use kaji::replay::plan::{replay_plan, PlannedTurn};
use kaji::replay::provider::ReplayProvider;
use kaji::replay::retention::retention_days;
use kaji::replay::source::ReplaySource;
use kaji::session::session_manager::SessionType;
use kaji::session::SessionManager;
use kaji::workflow::events::WorkflowRecorder;
use kaji::workflow::{ReplayGates, ReplayRunner, WorkflowExecutor};
use kaji_providers::model::ModelConfig;
use rmcp::model::Role;

const EXIT_DIVERGENCE: i32 = 2;
const EXIT_TRUNCATED: i32 = 3;
const EXIT_UNAVAILABLE: i32 = 4;

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
        .with_context(|| format!("session « {session_id} » introuvable"))?;

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
        .any(|(_, planned)| matches!(planned, PlannedTurn::Replay(_) | PlannedTurn::Workflow(_)))
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

    let mut replay_mode = ReplayMode::for_session(&source);
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
            // Marqueur de la session dérivée : l'assemblage d'un tour rejoué
            // prend le `ModelConfig` de la session enregistrée, porté par
            // `ReplayMode` — pas celui-ci.
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
        let user_message = match planned {
            PlannedTurn::Replay(user_message) => user_message,
            // Un tour d'orchestration ne se rejoue pas par un message : son
            // DAG est redéroulé sur le journal, sans qu'aucun sous-agent réel
            // ne parte.
            PlannedTurn::Workflow(workflow) => {
                match replay_workflow(
                    Arc::clone(&session_manager),
                    &cursor,
                    &derived.id,
                    source.working_dir.clone(),
                    &workflow,
                    lenient,
                )
                .await
                {
                    Ok(replay) => {
                        replayed += 1;
                        for line in replay.lines {
                            println!("[tour {turn_seq}] {line}");
                        }
                        match replay.verdict {
                            TurnVerdict::Faithful => {}
                            TurnVerdict::Tolerated(divergence) => {
                                tolerated += 1;
                                println!("[tour {turn_seq}] divergence tolérée — {divergence}");
                            }
                            TurnVerdict::Fatal(divergence) => {
                                eprintln!(
                                    "kaji replay : divergence au tour {turn_seq} — {divergence}"
                                );
                                std::process::exit(EXIT_DIVERGENCE);
                            }
                        }
                    }
                    // Pas une divergence : le journal n'a pas de quoi
                    // redérouler le DAG (payload illisible, session dérivée
                    // qui refuse le workflow). Le rejeu s'arrête là, en le
                    // disant autrement.
                    Err(error) => {
                        eprintln!(
                            "kaji replay : rejeu du workflow impossible au tour {turn_seq} — {error}"
                        );
                        std::process::exit(EXIT_DIVERGENCE);
                    }
                }
                continue;
            }
            PlannedTurn::Skipped => {
                skipped += 1;
                println!("[tour {turn_seq}] aucun message user enregistré — tour sauté");
                continue;
            }
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

/// Rejoue le tour d'orchestration : le DAG repart, mais ses gates, ses
/// recettes et les sorties de ses agents sont servies par le journal
/// (`ReplayGates`, `ReplayRunner`) — aucun sous-agent, aucun appel LLM. L'état
/// final est comparé à celui qu'a figé `workflow_done` : c'est ce qui rend
/// « rejoué sans divergence » vérifiable plutôt que déclaratif.
async fn replay_workflow(
    session_manager: Arc<SessionManager>,
    cursor: &EventCursor,
    derived_session_id: &str,
    working_dir: std::path::PathBuf,
    workflow: &str,
    lenient: bool,
) -> Result<WorkflowReplay> {
    let started = cursor
        .workflow
        .clone()
        .ok_or_else(|| anyhow!("workflow « {workflow} » sans payload workflow_started"))?;

    let recorder =
        WorkflowRecorder::open(session_manager, derived_session_id.to_string(), workflow).await?;
    let executor = WorkflowExecutor::replaying(
        started.spec,
        Arc::new(ReplayRunner::from_cursor(cursor)),
        Arc::new(ReplayGates::from_cursor(cursor).lenient(lenient)),
        cursor.workflow_recipes.clone(),
        recorder,
        derived_session_id.to_string(),
        working_dir,
    )?;

    let state = executor.run().await?;
    let mut lines = vec![format!(
        "workflow « {workflow} » rejoué — {}",
        state.outcome().label()
    )];
    let verdict = workflow_verdict(cursor.workflow_final.as_ref(), &state, workflow, lenient);
    if matches!(verdict, TurnVerdict::Faithful) {
        lines.push(format!(
            "workflow « {workflow} » identique à l'enregistrement"
        ));
    }
    Ok(WorkflowReplay { lines, verdict })
}

/// Ce qu'un tour de workflow rejoué rend à la boucle : sa transcription, et
/// l'écart éventuel à l'état figé par `workflow_done`.
struct WorkflowReplay {
    lines: Vec<String>,
    verdict: TurnVerdict,
}

/// Le sort d'un écart entre l'état rejoué et l'état enregistré.
enum TurnVerdict {
    /// Rien à signaler — topologies identiques, ou aucun état enregistré à
    /// comparer.
    Faithful,
    /// Écart compté et signalé, le rejeu continue (`--lenient`).
    Tolerated(String),
    /// Écart fatal : le rejeu sort en `EXIT_DIVERGENCE` (mode strict).
    Fatal(String),
}

/// Compare la topologie rejouée à celle qu'a figée `workflow_done`, et tranche
/// selon le mode. `--lenient` existe pour **auditer** les divergences en
/// sortant vert : une gate absente du journal y refuse son stage, la cascade
/// suit, et la topologie diverge — la compter comme tolérée est la seule
/// lecture cohérente avec le mode. Le mode strict, lui, reste fatal.
///
/// La comparaison porte sur les noms et les états (`topology()`), jamais sur
/// `tokens`/`duration_ms` : ce qu'elle prouve est la cohérence de
/// l'ordonnanceur avec son journal, pas le déterminisme d'un modèle — aucun
/// agent ne tourne au rejeu.
///
/// Un tour clos **sans** `workflow_done` n'est pas fidèle : la borne est là, le
/// témoin manque, et rien n'a été comparé. L'annoncer « sans divergence »
/// promettrait une vérification qui n'a pas eu lieu — c'est une divergence
/// nommée, fatale en strict et comptée sous `--lenient`.
fn workflow_verdict(
    recorded: Option<&kaji::workflow::WorkflowState>,
    replayed: &kaji::workflow::WorkflowState,
    workflow: &str,
    lenient: bool,
) -> TurnVerdict {
    let divergence = match recorded {
        None => format!(
            "journal incomplet — workflow « {workflow} » clos sans état final \
             enregistré : le rejeu n'a rien contre quoi se mesurer"
        ),
        Some(recorded) if recorded.topology() == replayed.topology() => {
            return TurnVerdict::Faithful
        }
        Some(recorded) => format!(
            "état du workflow « {workflow} » différent de l'enregistrement — \
             enregistré {:?}, rejoué {:?}",
            recorded.topology(),
            replayed.topology()
        ),
    };
    if lenient {
        TurnVerdict::Tolerated(divergence)
    } else {
        TurnVerdict::Fatal(divergence)
    }
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

#[cfg(test)]
mod tests {
    use super::*;

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

    fn state_with_stage(stage_state: serde_json::Value) -> kaji::workflow::WorkflowState {
        serde_json::from_value(serde_json::json!({
            "workflow": "livraison",
            "stages": [{
                "name": "deploie",
                "state": stage_state,
                "gate": "approve",
                "agents": [{
                    "name": "pousse",
                    "state": "done",
                    "session_id": null,
                    "tokens": 0,
                    "duration_ms": 0,
                }],
            }],
        }))
        .unwrap()
    }

    #[test]
    fn an_identical_workflow_turn_is_faithful() {
        let verdict = workflow_verdict(
            Some(&state_with_stage(serde_json::json!("done"))),
            &state_with_stage(serde_json::json!("done")),
            "livraison",
            false,
        );
        assert!(matches!(verdict, TurnVerdict::Faithful));
    }

    /// Un tour de workflow dont le journal ne porte pas d'état final n'a rien
    /// été comparé : le dire fidèle promettrait une vérification qui n'a pas eu
    /// lieu. C'est une divergence nommée — fatale en strict, comptée en
    /// `--lenient`.
    #[test]
    fn a_workflow_turn_without_a_recorded_state_is_an_incomplete_journal() {
        let replayed = state_with_stage(serde_json::json!("done"));

        let TurnVerdict::Fatal(message) = workflow_verdict(None, &replayed, "livraison", false)
        else {
            panic!("sans état final enregistré, le rejeu n'a rien vérifié");
        };
        assert!(
            message.contains("journal incomplet") && message.contains("livraison"),
            "{message}"
        );

        assert!(
            matches!(
                workflow_verdict(None, &replayed, "livraison", true),
                TurnVerdict::Tolerated(_)
            ),
            "`--lenient` la compte au lieu de sortir en 2"
        );
    }

    /// F-9 : `--lenient` existe pour auditer les divergences en sortant vert.
    /// Une topologie de workflow différente y est comptée, pas fatale — en
    /// strict elle reste fatale.
    #[test]
    fn a_diverging_workflow_turn_is_tolerated_in_lenient_and_fatal_in_strict() {
        let recorded = state_with_stage(serde_json::json!("done"));
        let replayed = state_with_stage(serde_json::json!("cancelled"));

        let TurnVerdict::Tolerated(message) =
            workflow_verdict(Some(&recorded), &replayed, "livraison", true)
        else {
            panic!("--lenient compte la divergence au lieu de sortir en 2");
        };
        assert!(message.contains("livraison"), "{message}");

        assert!(
            matches!(
                workflow_verdict(Some(&recorded), &replayed, "livraison", false),
                TurnVerdict::Fatal(_)
            ),
            "le mode strict reste fatal"
        );
    }
}
