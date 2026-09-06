//! `kaji workflow run|list|status` : lancer un workflow déclaratif, revoir
//! ceux qui ont tourné, et reprendre l'état de l'un d'eux depuis son journal
//! (spec S3, patron `commands/replay.rs`).
//!
//! **`run` crée toujours une session dédiée**, jamais un tour de plus sur une
//! conversation ouverte : le tour d'orchestration est revendiqué en exclusif
//! (`claim_exclusive_turn_start`), une session n'en porte qu'un, et les gates
//! y sont adressées par nom de stage — deux workflows sur une même session
//! écraseraient leurs décisions l'un l'autre. La session est en
//! [`SessionType::Hidden`] : ce n'est pas une conversation à reprendre, c'est
//! le journal du run, et `kaji workflow list` est sa surface.
//!
//! Codes de sortie : `0` workflow terminé ; `1` workflow échoué ou annulé
//! (l'issue est nommée), comme toute erreur inattendue via `anyhow` ; `2`
//! spec illisible ou invalide — aucune session n'est alors créée ; `130`
//! interruption au clavier, après annulation propre des agents en vol.

use std::collections::HashSet;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use kaji::config::{Config, KajiMode};
use kaji::session::session_manager::SessionType;
use kaji::session::SessionManager;
use kaji::workflow::events::WorkflowRecorder;
use kaji::workflow::{
    find_workflow_run, list_workflow_runs, AgentState, AgentStatus, FailureCause, GateDecision,
    StageState, StageStatus, SubagentRunner, WorkflowExecutor, WorkflowHandle, WorkflowOutcome,
    WorkflowRun, WorkflowState,
};
use kaji_core::workflow::{Gate, WorkflowSpec};

const EXIT_FAILED: i32 = 1;
const EXIT_SPEC: i32 = 2;
const EXIT_INTERRUPTED: i32 = 130;

/// Période de relecture de l'état vivant. Assez court pour que la progression
/// suive les agents, assez long pour ne pas noyer le terminal : l'exécuteur
/// n'a pas de canal d'événements, la vue le sonde.
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// D'où vient la décision d'une gate `approve` en ligne de commande.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateControl {
    /// `--approve-all` : tout est approuvé sans rien demander.
    ApproveAll,
    /// Le mode par défaut au terminal : une question par gate.
    Ask,
    /// Ni drapeau, ni terminal. Une gate y est **refusée** plutôt qu'attendue
    /// sans borne : personne ne viendra répondre.
    Unattended,
}

pub async fn handle_workflow_run(path: PathBuf, approve_all: bool) -> Result<()> {
    let spec = match read_spec(&path) {
        Ok(spec) => spec,
        Err(error) => {
            eprintln!("kaji workflow : {error}");
            std::process::exit(EXIT_SPEC);
        }
    };

    let session_manager = Arc::new(SessionManager::instance());
    let config = Config::global();
    let provider_name = config
        .get_kaji_provider()
        .map_err(|_| anyhow!("aucun provider configuré — lancer `kaji configure`"))?;
    let model_name = config
        .get_kaji_model()
        .map_err(|_| anyhow!("aucun modèle configuré — lancer `kaji configure`"))?;
    let model_config =
        kaji::model_config::model_config_from_user_config(&provider_name, &model_name)?;

    let working_dir = std::env::current_dir()?;
    let session = session_manager
        .create_session(
            working_dir.clone(),
            format!("workflow {}", spec.name),
            SessionType::Hidden,
            KajiMode::Auto,
        )
        .await?;
    // Les agents descendent sur le provider de leur session parente : sans ce
    // report, `SubagentRunner::run` refuserait chaque lancement.
    session_manager
        .update(&session.id)
        .provider_name(&provider_name)
        .model_config(model_config)
        .apply()
        .await?;

    let recorder =
        WorkflowRecorder::open(Arc::clone(&session_manager), session.id.clone(), &spec.name)
            .await?;
    let executor = WorkflowExecutor::new(
        spec.clone(),
        Arc::new(SubagentRunner::new(Arc::clone(&session_manager))),
        recorder,
        session.id.clone(),
        working_dir,
    )?;
    let handle = executor.handle();

    println!(
        "kaji workflow : « {} » → session « {} »",
        spec.name, session.id
    );
    let control = gate_control(approve_all, std::io::stdin().is_terminal());

    let mut run = tokio::spawn(executor.run());
    let drive = drive_run(
        &handle,
        &mut run,
        control,
        || async {
            let _ = tokio::signal::ctrl_c().await;
        },
        |stage| async move { ask_gate(&stage).await },
    )
    .await?;

    let outcome = drive.state.outcome();
    println!(
        "kaji workflow : « {} » {}",
        drive.state.workflow,
        outcome_label(&outcome)
    );

    let code = exit_code(&outcome, drive.interrupted);
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

/// Ce qu'un run rend à son appelant : l'état final, et si une interruption
/// l'a écourté — les deux entrent dans le code de sortie.
struct DriveOutcome {
    state: WorkflowState,
    interrupted: bool,
}

/// Ce qu'un tour de boucle a produit. Les branches du `select!` se contentent
/// de nommer l'événement : la suite s'écrit après, où les emprunts des futurs
/// encore en attente sont relâchés.
enum Step {
    Finished(WorkflowState),
    Interrupted,
    Gate(String, Option<GateDecision>),
    Tick,
}

/// L'invite d'une gate en cours : la question posée et la réponse à venir. Elle
/// vit hors du `select!` pour survivre aux tours qu'une autre branche gagne.
struct PendingGate {
    stage: String,
    answer: std::pin::Pin<Box<dyn std::future::Future<Output = Option<GateDecision>> + Send>>,
}

/// La branche « une gate a répondu », inerte tant qu'aucune n'est posée. Le
/// futur de l'invite reste chez l'appelant : seul cet emprunt est abandonné
/// quand une autre branche gagne, jamais l'attente elle-même.
async fn wait_for_gate(pending: Option<&mut PendingGate>) -> (String, Option<GateDecision>) {
    match pending {
        Some(gate) => {
            let decision = gate.answer.as_mut().await;
            (gate.stage.clone(), decision)
        }
        None => std::future::pending().await,
    }
}

/// Le pilote d'un run en vol : sonde l'état, imprime ce qui change, tranche
/// les gates ouvertes, et annule sur interruption.
///
/// La source d'interruption est un paramètre — `Ctrl+C` en production, une
/// source scriptée sous test : `tokio::signal::ctrl_c()` n'est pas armable
/// depuis un test sans envoyer un vrai signal au binaire de test entier. Elle
/// est armée **une seule fois**, hors de la boucle : un futur reconstruit à
/// chaque tour se désabonne à chaque drop, et tokio jette un SIGINT que
/// personne n'écoute — après avoir désarmé la mort du processus.
///
/// L'invite d'une gate est une branche du `select!`, jamais le corps d'une
/// autre : pendant que l'opérateur réfléchit, la fin du run et l'interruption
/// restent observables.
async fn drive_run<S, F, A, G>(
    handle: &WorkflowHandle,
    run: &mut tokio::task::JoinHandle<Result<WorkflowState>>,
    control: GateControl,
    interrupts: S,
    ask: A,
) -> Result<DriveOutcome>
where
    S: Fn() -> F,
    F: std::future::Future<Output = ()>,
    A: Fn(String) -> G,
    G: std::future::Future<Output = Option<GateDecision>> + Send + 'static,
{
    let mut previous = handle.snapshot();
    let mut asked: HashSet<String> = HashSet::new();
    let mut interrupted = false;
    let mut pending: Option<PendingGate> = None;
    let mut ticker = tokio::time::interval(POLL_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let interrupt = interrupts();
    tokio::pin!(interrupt);

    let state = loop {
        let step = tokio::select! {
            finished = &mut *run => Step::Finished(finished??),
            () = &mut interrupt => Step::Interrupted,
            (stage, decision) = wait_for_gate(pending.as_mut()) => Step::Gate(stage, decision),
            _ = ticker.tick() => Step::Tick,
        };

        match step {
            Step::Finished(state) => break state,
            Step::Interrupted => {
                // Le second Ctrl+C ne laisse plus de grâce : le premier a déjà
                // demandé l'arrêt, et tokio garde la main sur le signal.
                if interrupted {
                    std::process::exit(EXIT_INTERRUPTED);
                }
                interrupted = true;
                pending = None;
                handle.cancel().await;
                // Réarmée pour le second appui, la boucle étant désormais
                // sondée en continu : aucune fenêtre d'aveuglement.
                interrupt.set(interrupts());
                eprintln!("kaji workflow : annulation demandée — arrêt des agents en vol");
            }
            Step::Gate(stage, decision) => {
                pending = None;
                apply_gate_decision(handle, &stage, decision);
                if !interrupted {
                    pending = next_gate(handle, control, &mut asked, &ask);
                }
            }
            Step::Tick => {
                previous = report_progress(handle, previous);
                if !interrupted && pending.is_none() {
                    pending = next_gate(handle, control, &mut asked, &ask);
                }
            }
        }
    };

    report_progress_against(handle, &previous, &state);
    Ok(DriveOutcome { state, interrupted })
}

pub async fn handle_workflow_list() -> Result<()> {
    let session_manager = SessionManager::instance();
    let runs = list_workflow_runs(&session_manager).await?;
    if runs.is_empty() {
        println!("kaji workflow : aucun workflow enregistré");
        return Ok(());
    }
    for run in &runs {
        println!("{}", list_line(run));
    }
    Ok(())
}

pub async fn handle_workflow_status(session_id: String) -> Result<()> {
    let session_manager = SessionManager::instance();
    let run = match find_workflow_run(&session_manager, &session_id).await? {
        Some(run) => run,
        None => {
            let known = session_manager
                .get_session(&session_id, false)
                .await
                .is_ok();
            return Err(anyhow!("{}", missing_run_message(&session_id, known)));
        }
    };
    for line in status_report(&run) {
        println!("{line}");
    }
    Ok(())
}

/// Un identifiant qui ne désigne aucune session et une session qui n'a jamais
/// lancé de workflow ne se corrigent pas de la même façon : la première invite
/// à revoir l'identifiant, la seconde dit que la session existe mais n'a rien
/// à montrer ici.
fn missing_run_message(session_id: &str, known: bool) -> String {
    if known {
        format!(
            "la session « {session_id} » existe mais n'a jamais lancé de workflow — \
             `kaji workflow list` donne celles qui en portent un"
        )
    } else {
        format!(
            "aucune session « {session_id} » — vérifier l'identifiant, \
             `kaji workflow list` donne ceux des workflows enregistrés"
        )
    }
}

fn read_spec(path: &Path) -> Result<WorkflowSpec> {
    let yaml = std::fs::read_to_string(path)
        .map_err(|error| anyhow!("spec « {} » illisible : {error}", path.display()))?;
    WorkflowSpec::from_yaml(&yaml)
        .map_err(|error| anyhow!("spec « {} » invalide : {error}", path.display()))
}

fn gate_control(approve_all: bool, interactive: bool) -> GateControl {
    match (approve_all, interactive) {
        (true, _) => GateControl::ApproveAll,
        (false, true) => GateControl::Ask,
        (false, false) => GateControl::Unattended,
    }
}

/// La prochaine gate ouverte à trancher, et l'attente de sa réponse. `asked`
/// empêche de reposer la question pendant que l'exécuteur n'a pas encore
/// consommé la décision — le stage reste `Waiting` un tick de plus.
///
/// Une seule à la fois : au terminal les questions se posent en file, et une
/// décision immédiate rouvre le choix de la suivante dans la foulée.
fn next_gate<A, G>(
    handle: &WorkflowHandle,
    control: GateControl,
    asked: &mut HashSet<String>,
    ask: &A,
) -> Option<PendingGate>
where
    A: Fn(String) -> G,
    G: std::future::Future<Output = Option<GateDecision>> + Send + 'static,
{
    let stage = handle
        .snapshot()
        .stages
        .iter()
        .find(|stage| stage.state == StageState::Waiting && !asked.contains(&stage.name))
        .map(|stage| stage.name.clone())?;

    asked.insert(stage.clone());
    let answer: std::pin::Pin<Box<dyn std::future::Future<Output = _> + Send>> = match control {
        GateControl::ApproveAll => Box::pin(std::future::ready(Some(GateDecision::Approve))),
        GateControl::Ask => Box::pin(ask(stage.clone())),
        GateControl::Unattended => Box::pin(std::future::ready(None)),
    };
    Some(PendingGate { stage, answer })
}

/// Applique la réponse à une gate ; `None` est le cas sans personne, qui refuse
/// plutôt que d'attendre sans borne.
fn apply_gate_decision(handle: &WorkflowHandle, stage: &str, answered: Option<GateDecision>) {
    let decision = match answered {
        Some(decision) => decision,
        None => {
            let (decision, message) = unattended_verdict(stage);
            eprintln!("kaji workflow : {message}");
            decision
        }
    };
    let verdict = if decision.approved() {
        handle.approve(stage)
    } else {
        handle.deny(stage)
    };
    if !verdict.applied() {
        println!(
            "kaji workflow : gate « {stage} » non tranchée — {}",
            verdict.label()
        );
    }
}

/// Pose la question au terminal jusqu'à une réponse lisible. `None` sur une
/// entrée fermée : le terminal a disparu en cours de route, la gate retombe
/// alors dans le cas sans personne.
async fn ask_gate(stage: &str) -> Option<GateDecision> {
    let stage = stage.to_string();
    tokio::task::spawn_blocking(move || loop {
        print!("kaji workflow : gate « {stage} » — approuver ? [o/n] ");
        let _ = std::io::stdout().flush();
        let mut answer = String::new();
        match std::io::stdin().read_line(&mut answer) {
            Ok(0) | Err(_) => return None,
            Ok(_) => {}
        }
        if let Some(decision) = gate_answer(&answer) {
            return Some(decision);
        }
    })
    .await
    .ok()
    .flatten()
}

fn gate_answer(input: &str) -> Option<GateDecision> {
    match input.trim().to_lowercase().as_str() {
        "o" | "oui" | "y" | "yes" => Some(GateDecision::Approve),
        "n" | "non" | "no" => Some(GateDecision::Deny),
        _ => None,
    }
}

/// Ce que devient une gate que personne ne peut trancher : elle est
/// **refusée**, pas laissée ouverte ni contournée par une annulation nue.
///
/// Un refus est journalisé (`gate_decision`), donc la session reste rejouable
/// à l'identique ; une annulation nue ne laisserait aucune décision au
/// journal, et `kaji replay` divergerait en refusant d'inventer l'approbation
/// manquante. Le stage et sa descendance sont annulés par la cascade
/// habituelle du refus.
fn unattended_verdict(stage: &str) -> (GateDecision, String) {
    (
        GateDecision::Deny,
        format!(
            "gate « {stage} » ouverte sans personne pour décider (entrée non interactive) — \
             refusée ; relancer avec --approve-all pour approuver sans demander"
        ),
    )
}

fn report_progress(handle: &WorkflowHandle, previous: WorkflowState) -> WorkflowState {
    let current = handle.snapshot();
    report_progress_against(handle, &previous, &current);
    current
}

fn report_progress_against(
    handle: &WorkflowHandle,
    previous: &WorkflowState,
    current: &WorkflowState,
) {
    let artifact_len = |stage: &str, agent: &str| {
        handle
            .artifact(stage, agent)
            .map(|output| output.chars().count())
    };
    for line in progress_lines(previous, current, &artifact_len) {
        println!("{line}");
    }
}

/// Les seules lignes qu'un tick imprime : ce qui a **changé** d'état depuis le
/// précédent. Sonder l'état vivant sans ce filtre réécrirait le workflow
/// entier cinq fois par seconde.
fn progress_lines(
    previous: &WorkflowState,
    current: &WorkflowState,
    artifact_len: &dyn Fn(&str, &str) -> Option<usize>,
) -> Vec<String> {
    let mut lines = Vec::new();
    for (index, stage) in current.stages.iter().enumerate() {
        let Some(before) = previous.stages.get(index) else {
            continue;
        };
        if before.state != stage.state {
            lines.push(stage_line(stage));
        }
        for (agent_index, agent) in stage.agents.iter().enumerate() {
            let Some(before) = before.agents.get(agent_index) else {
                continue;
            };
            if before.state != agent.state {
                lines.push(agent_line(
                    &stage.name,
                    agent,
                    artifact_len(&stage.name, &agent.name),
                ));
            }
        }
    }
    lines
}

fn stage_line(stage: &StageStatus) -> String {
    match &stage.state {
        StageState::Waiting => format!("stage « {} » — gate : décision attendue", stage.name),
        StageState::Failed(cause) => {
            format!("stage « {} » — échoué : {}", stage.name, cause_label(cause))
        }
        state => format!("stage « {} » — {}", stage.name, state.label()),
    }
}

fn agent_line(stage: &str, agent: &AgentStatus, artifact: Option<usize>) -> String {
    let state = match &agent.state {
        AgentState::Failed(cause) => format!("échoué : {}", cause_label(cause)),
        state => state.label().to_string(),
    };
    let mut line = format!("  {stage}.{} — {state}", agent.name);
    if agent.tokens > 0 {
        line.push_str(&format!(" · {} tokens", agent.tokens));
    }
    if agent.duration_ms > 0 {
        line.push_str(&format!(" · {}", duration_label(agent.duration_ms)));
    }
    if let Some(chars) = artifact {
        line.push_str(&format!(" · artefact {chars} car."));
    }
    line
}

/// Un budget coupé nomme le champ de la spec qui l'a coupé : le message doit
/// pointer la ligne YAML à corriger, pas seulement dire « échoué ».
fn cause_label(cause: &FailureCause) -> String {
    match cause {
        FailureCause::Budget(limit) => format!("budget {} dépassé", limit.field()),
        FailureCause::Error(error) => error.clone(),
    }
}

fn duration_label(ms: i64) -> String {
    if ms < 1_000 {
        format!("{ms} ms")
    } else {
        format!("{:.1} s", ms as f64 / 1_000.0)
    }
}

fn outcome_label(outcome: &WorkflowOutcome) -> String {
    match outcome {
        WorkflowOutcome::Failed(cause) => format!("échoué : {}", cause_label(cause)),
        other => other.label().to_string(),
    }
}

fn exit_code(outcome: &WorkflowOutcome, interrupted: bool) -> i32 {
    if interrupted {
        return EXIT_INTERRUPTED;
    }
    match outcome {
        WorkflowOutcome::Done => 0,
        WorkflowOutcome::Failed(_) | WorkflowOutcome::Cancelled => EXIT_FAILED,
    }
}

/// Un run sans `workflow_done` n'annonce pas d'issue : il tourne encore, ou il
/// a été tué. Le déclarer « échoué » serait faux dans le premier cas.
fn run_state_label(run: &WorkflowRun) -> String {
    match run.outcome() {
        Some(outcome) => outcome_label(&outcome),
        None => "en cours".to_string(),
    }
}

fn list_line(run: &WorkflowRun) -> String {
    format!(
        "{} · {} · {} · session {}",
        local_date(run.started_at_ms),
        run.workflow,
        run_state_label(run),
        run.session_id
    )
}

fn status_report(run: &WorkflowRun) -> Vec<String> {
    let mut lines = vec![format!(
        "workflow « {} » · session {} · démarré {} · {}",
        run.workflow,
        run.session_id,
        local_date(run.started_at_ms),
        run_state_label(run)
    )];
    for stage in &run.state.stages {
        lines.push(match stage.gate {
            Gate::Approve => format!("{} (gate {})", stage_line(stage), stage.gate.label()),
            Gate::Auto => stage_line(stage),
        });
        for agent in &stage.agents {
            lines.push(agent_line(&stage.name, agent, None));
        }
    }
    let pending = run.pending_gates();
    if !pending.is_empty() {
        lines.push(format!("gate(s) en attente : {}", pending.join(", ")));
    }
    lines
}

fn local_date(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|instant| {
            instant
                .with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| "date inconnue".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeMap;

    use kaji::workflow::{AgentRunRequest, AgentRunner, BudgetLimit};
    use tokio_util::sync::CancellationToken;

    const SPEC: &str = r#"
name: revue
stages:
  - name: collecte
    agents:
      - name: scan
        prompt: scanne
  - name: deploie
    depends_on: [collecte]
    gate: approve
    agents:
      - name: pousse
        prompt: pousse
"#;

    fn state() -> WorkflowState {
        WorkflowState::from_spec(&WorkflowSpec::from_yaml(SPEC).unwrap())
    }

    fn no_artifact(_stage: &str, _agent: &str) -> Option<usize> {
        None
    }

    #[test]
    fn the_exit_code_names_the_outcome_and_the_interruption_wins() {
        assert_eq!(exit_code(&WorkflowOutcome::Done, false), 0);
        assert_eq!(
            exit_code(
                &WorkflowOutcome::Failed(FailureCause::Budget(BudgetLimit::Tokens)),
                false
            ),
            1
        );
        assert_eq!(exit_code(&WorkflowOutcome::Cancelled, false), 1);
        assert_eq!(
            exit_code(&WorkflowOutcome::Cancelled, true),
            130,
            "Ctrl+C sort en 130, pas en échec ordinaire"
        );
    }

    /// Une spec illisible ou invalide sort en 2, distinct de l'échec d'un
    /// workflow qui a bel et bien tourné.
    #[test]
    fn an_invalid_spec_is_refused_before_any_session_is_created() {
        let error = read_spec(std::path::Path::new("/absent/workflow.yaml")).unwrap_err();
        assert!(
            error.to_string().contains("/absent/workflow.yaml"),
            "{error}"
        );
        assert_eq!(EXIT_SPEC, 2);
    }

    #[test]
    fn a_stage_transition_is_printed_once_per_change() {
        let previous = state();
        let mut current = previous.clone();
        current.stages[0].state = StageState::Running;

        let lines = progress_lines(&previous, &current, &no_artifact);

        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("collecte"), "{lines:?}");
        assert!(lines[0].contains("en cours"), "{lines:?}");
        assert!(
            progress_lines(&current, &current, &no_artifact).is_empty(),
            "un état inchangé n'imprime rien"
        );
    }

    #[test]
    fn a_finished_agent_line_carries_its_counters_and_its_artifact() {
        let previous = state();
        let mut current = previous.clone();
        current.stages[0].agents[0].state = AgentState::Done;
        current.stages[0].agents[0].tokens = 120;
        current.stages[0].agents[0].duration_ms = 3_200;

        let lines = progress_lines(&previous, &current, &|stage, agent| {
            (stage == "collecte" && agent == "scan").then_some(42)
        });

        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("collecte.scan"), "{lines:?}");
        assert!(lines[0].contains("120"), "{lines:?}");
        assert!(lines[0].contains("3.2 s"), "{lines:?}");
        assert!(
            lines[0].contains("42"),
            "{lines:?}: la taille de l'artefact"
        );
    }

    /// Un budget coupé doit nommer la ligne YAML à corriger, jamais « échoué »
    /// tout court.
    #[test]
    fn a_budget_failure_names_the_field_that_cut_the_agent() {
        let previous = state();
        let mut current = previous.clone();
        current.stages[0].agents[0].state =
            AgentState::Failed(FailureCause::Budget(BudgetLimit::Duration));

        let lines = progress_lines(&previous, &current, &no_artifact);

        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("max_duration_s"), "{lines:?}");
    }

    #[test]
    fn a_waiting_gate_is_announced_as_a_decision_to_take() {
        let previous = state();
        let mut current = previous.clone();
        current.stages[1].state = StageState::Waiting;

        let lines = progress_lines(&previous, &current, &no_artifact);

        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("deploie"), "{lines:?}");
        assert!(lines[0].contains("gate"), "{lines:?}");
    }

    /// Les trois façons de trancher une gate : le drapeau non interactif, le
    /// terminal, et le cas sans personne — qui doit refuser plutôt que
    /// d'attendre sans borne une décision qui ne viendra jamais.
    #[test]
    fn the_gate_control_follows_the_flag_then_the_terminal() {
        assert_eq!(gate_control(true, false), GateControl::ApproveAll);
        assert_eq!(gate_control(true, true), GateControl::ApproveAll);
        assert_eq!(gate_control(false, true), GateControl::Ask);
        assert_eq!(gate_control(false, false), GateControl::Unattended);
    }

    /// Une gate que personne ne peut trancher est **refusée**, pas laissée en
    /// plan : le refus part au journal, donc la session se rejoue à
    /// l'identique. Une annulation nue n'y laisserait aucune décision et
    /// `kaji replay` divergerait.
    #[test]
    fn an_unattended_gate_is_denied_and_names_the_flag_that_would_have_worked() {
        let (decision, message) = unattended_verdict("deploie");

        assert_eq!(decision, GateDecision::Deny);
        assert!(message.contains("deploie"), "{message}");
        assert!(message.contains("--approve-all"), "{message}");
    }

    #[test]
    fn a_gate_answer_reads_both_languages_and_re_asks_on_anything_else() {
        assert_eq!(gate_answer("o"), Some(GateDecision::Approve));
        assert_eq!(gate_answer("Oui"), Some(GateDecision::Approve));
        assert_eq!(gate_answer(" y \n"), Some(GateDecision::Approve));
        assert_eq!(gate_answer("n"), Some(GateDecision::Deny));
        assert_eq!(gate_answer("non"), Some(GateDecision::Deny));
        assert_eq!(gate_answer(""), None);
        assert_eq!(gate_answer("peut-être"), None);
    }

    fn run(finished: bool, state: WorkflowState) -> WorkflowRun {
        WorkflowRun {
            session_id: "abc123".to_string(),
            workflow: "revue".to_string(),
            started_at_ms: 1_788_557_600_000,
            spec: WorkflowSpec::from_yaml(SPEC).unwrap(),
            state,
            gates: BTreeMap::new(),
            finished,
        }
    }

    #[test]
    fn a_list_line_carries_the_session_the_state_and_the_date() {
        let mut state = state();
        state.stages[0].state = StageState::Done;
        state.stages[0].agents[0].state = AgentState::Done;
        state.stages[1].state = StageState::Done;
        state.stages[1].agents[0].state = AgentState::Done;

        let line = list_line(&run(true, state));

        assert!(line.contains("abc123"), "{line}");
        assert!(line.contains("revue"), "{line}");
        assert!(line.contains("terminé"), "{line}");
        assert!(line.contains("2026"), "{line}");
    }

    /// Un run sans `workflow_done` n'annonce pas d'issue : il est en cours ou
    /// il a été tué, et prétendre « échoué » serait faux dans le premier cas.
    #[test]
    fn an_unfinished_run_is_listed_as_in_flight() {
        let line = list_line(&run(false, state()));
        assert!(line.contains("en cours"), "{line}");
    }

    #[test]
    fn the_status_report_shows_the_topology_and_the_pending_gates() {
        let mut state = state();
        state.stages[0].state = StageState::Done;
        state.stages[0].agents[0].state = AgentState::Done;
        state.stages[0].agents[0].tokens = 120;
        state.stages[1].state = StageState::Waiting;

        let report = status_report(&run(false, state)).join("\n");

        assert!(report.contains("revue"), "{report}");
        assert!(report.contains("collecte"), "{report}");
        assert!(report.contains("scan"), "{report}");
        assert!(report.contains("120"), "{report}");
        assert!(report.contains("deploie"), "{report}");
        assert!(
            report.contains("gate(s) en attente : deploie"),
            "{report}: la gate qui attend une décision doit ressortir"
        );
    }

    /// Un identifiant inconnu et une session sans workflow demandent deux
    /// corrections différentes : le même message pour les deux laisse
    /// l'utilisateur chercher une faute de frappe qui n'existe pas.
    #[test]
    fn a_missing_run_tells_an_unknown_session_apart_from_one_without_a_workflow() {
        let unknown = missing_run_message("abc123", false);
        let known = missing_run_message("abc123", true);

        assert_ne!(unknown, known);
        assert!(unknown.contains("abc123"), "{unknown}");
        assert!(known.contains("abc123"), "{known}");
        assert!(
            unknown.contains("aucune session"),
            "{unknown}: l'identifiant est à revoir"
        );
        assert!(
            known.contains("existe") && known.contains("jamais lancé"),
            "{known}: la session existe, elle n'a simplement rien à montrer"
        );
    }

    /// Un agent qui ne rend rien tant que son jeton n'est pas tiré : la cible
    /// d'un test d'interruption, qui doit trouver le workflow encore en vol.
    struct HangingRunner;

    #[async_trait::async_trait]
    impl AgentRunner for HangingRunner {
        async fn prepare(&self, request: &AgentRunRequest) -> Result<String, String> {
            Ok(format!("fixture_{}", request.label()))
        }

        async fn run(
            &self,
            _request: AgentRunRequest,
            _session_id: &str,
            cancel: CancellationToken,
        ) -> Result<String, String> {
            cancel.cancelled().await;
            Err("annulé".to_string())
        }
    }

    /// Ctrl+C pendant un run : la boucle annule le workflow en vol, en sort
    /// avec l'issue `Cancelled`, et retient l'interruption — c'est elle, pas
    /// l'issue, qui donne le code 130.
    ///
    /// L'interruption est injectée : `tokio::signal::ctrl_c()` ne s'arme pas
    /// depuis un test sans envoyer un vrai `SIGINT` au binaire de test entier.
    #[tokio::test]
    async fn ctrl_c_cancels_the_run_in_flight_and_exits_130() {
        let data_dir = tempfile::tempdir().unwrap();
        let session_manager = Arc::new(SessionManager::new(data_dir.path().join("data")));
        let working_dir = data_dir.path().join("workspace");
        let session = session_manager
            .create_session(
                working_dir.clone(),
                "workflow test".to_string(),
                SessionType::Hidden,
                KajiMode::Auto,
            )
            .await
            .unwrap();

        let spec = WorkflowSpec::from_yaml(SPEC).unwrap();
        let recorder =
            WorkflowRecorder::open(Arc::clone(&session_manager), session.id.clone(), &spec.name)
                .await
                .unwrap();
        let executor = WorkflowExecutor::new(
            spec,
            Arc::new(HangingRunner),
            recorder,
            session.id.clone(),
            working_dir,
        )
        .unwrap();
        let handle = executor.handle();
        let mut run = tokio::spawn(executor.run());

        // Un seul permis : la boucle réarme la source à chaque tour, et un
        // second Ctrl+C sortirait du processus de test.
        let interrupt = Arc::new(tokio::sync::Notify::new());
        let source = {
            let interrupt = Arc::clone(&interrupt);
            move || {
                let interrupt = Arc::clone(&interrupt);
                async move { interrupt.notified().await }
            }
        };

        let press = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            interrupt.notify_one();
        });
        let drive = tokio::time::timeout(
            Duration::from_secs(10),
            drive_run(
                &handle,
                &mut run,
                GateControl::Unattended,
                source,
                never_asked,
            ),
        )
        .await
        .expect("l'interruption sort de la boucle")
        .unwrap();
        press.await.unwrap();

        assert!(drive.interrupted);
        assert_eq!(drive.state.outcome(), WorkflowOutcome::Cancelled);
        assert_eq!(
            drive.state.stages[0].agents[0].state,
            AgentState::Cancelled,
            "l'agent en vol est coupé, pas laissé en cours"
        );
        assert_eq!(
            exit_code(&drive.state.outcome(), drive.interrupted),
            130,
            "l'interruption prime sur l'issue annulée"
        );
    }

    /// Une invite de gate sans réponse : l'opérateur regarde la question et
    /// presse Ctrl+C. Le pilote doit rester sondable pendant ce temps — sinon
    /// l'interruption n'est vue par personne et le seul moyen de sortir est de
    /// répondre à la gate.
    async fn never_asked(_stage: String) -> Option<GateDecision> {
        unreachable!("aucune gate n'est posée dans ce test")
    }

    const GATED_SPEC: &str = r#"
name: revue
stages:
  - name: deploie
    gate: approve
    agents:
      - name: pousse
        prompt: pousse
"#;

    #[tokio::test]
    async fn ctrl_c_while_a_gate_waits_for_an_answer_cancels_the_run_and_exits_130() {
        let data_dir = tempfile::tempdir().unwrap();
        let session_manager = Arc::new(SessionManager::new(data_dir.path().join("data")));
        let working_dir = data_dir.path().join("workspace");
        let session = session_manager
            .create_session(
                working_dir.clone(),
                "workflow test".to_string(),
                SessionType::Hidden,
                KajiMode::Auto,
            )
            .await
            .unwrap();

        let spec = WorkflowSpec::from_yaml(GATED_SPEC).unwrap();
        let recorder =
            WorkflowRecorder::open(Arc::clone(&session_manager), session.id.clone(), &spec.name)
                .await
                .unwrap();
        let executor = WorkflowExecutor::new(
            spec,
            Arc::new(HangingRunner),
            recorder,
            session.id.clone(),
            working_dir,
        )
        .unwrap();
        let handle = executor.handle();
        let mut run = tokio::spawn(executor.run());

        let interrupt = Arc::new(tokio::sync::Notify::new());
        let source = {
            let interrupt = Arc::clone(&interrupt);
            move || {
                let interrupt = Arc::clone(&interrupt);
                async move { interrupt.notified().await }
            }
        };

        // L'invite est posée puis plus rien : c'est l'opérateur qui réfléchit.
        let asked = Arc::new(tokio::sync::Notify::new());
        let ask = {
            let asked = Arc::clone(&asked);
            move |_stage: String| {
                let asked = Arc::clone(&asked);
                async move {
                    asked.notify_one();
                    std::future::pending::<Option<GateDecision>>().await
                }
            }
        };

        let press = tokio::spawn(async move {
            asked.notified().await;
            interrupt.notify_one();
        });
        let drive = tokio::time::timeout(
            Duration::from_secs(10),
            drive_run(&handle, &mut run, GateControl::Ask, source, ask),
        )
        .await
        .expect("l'interruption sort de la boucle même pendant l'invite")
        .unwrap();
        press.await.unwrap();

        assert!(drive.interrupted);
        assert_eq!(drive.state.outcome(), WorkflowOutcome::Cancelled);
        assert_eq!(
            exit_code(&drive.state.outcome(), drive.interrupted),
            130,
            "l'interruption pendant une gate sort en 130"
        );
    }
}
