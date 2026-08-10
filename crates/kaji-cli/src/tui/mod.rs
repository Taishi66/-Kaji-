pub mod app;
pub mod markdown;
pub mod report;
pub mod theme;
pub mod ui;

use anyhow::{Context, Result};
use app::{Action, App, PassDriver};
use futures::stream::BoxStream;
use futures::StreamExt;
use kaji::agents::{Agent, AgentEvent, SessionConfig};
use kaji::config::Config;
use kaji::conversation::message::Message;
use kaji::permission::permission_confirmation::PrincipalType;
use kaji::permission::{Permission, PermissionConfirmation};
use kaji::session::SessionManager;
use kaji_core::sdd::SpecDoc;
use ratatui::crossterm::event::{self, Event};
use ratatui::text::{Line, Span};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub async fn run(
    agent: Agent,
    session_id: String,
    conversation: kaji::conversation::Conversation,
    spec_path: Option<PathBuf>,
) -> Result<()> {
    let spec = resolve_spec(spec_path)?;
    let header = build_header(&session_id);
    let (input_tx, input_rx) = mpsc::channel::<Event>(64);
    std::thread::spawn(move || input_thread(input_tx));
    let mut terminal = ratatui::init();
    let result = event_loop(
        &mut terminal,
        &agent,
        &session_id,
        conversation,
        spec,
        header,
        input_rx,
    )
    .await;
    ratatui::restore();
    result
}

fn build_header(session_id: &str) -> String {
    let config = Config::global();
    let provider = config
        .get_kaji_provider()
        .unwrap_or_else(|_| "?".to_string());
    let model = config.get_kaji_model().unwrap_or_else(|_| "?".to_string());
    format!("{session_id} · {provider}/{model}")
}

fn push_welcome(app: &mut App) {
    app.push_system("鍛冶 bienvenue dans kaji");
    app.push_system("tape ton message puis Entrée");
    app.push_system("/sdd démarre une passe SDD (SPEC.md auto-détecté ou --spec <fichier>)");
    app.push_system("/spec (ou F2) affiche/masque le panneau SPEC · /help réaffiche l'aide");
    app.push_system(
        "/cost affiche l'usage tokens/coût (session, 5 h, 7 j) — budgets optionnels via KAJI_BUDGET_5H / KAJI_BUDGET_7J",
    );
    app.push_system("/docker liste les conteneurs en cours");
    app.push_system("PageUp/PageDown/Home/End font défiler le chat");
    app.push_system("Esc interrompt · Ctrl+C quitte");
}

/// Résumé git dim pour le header — `None` si `dir` n'est pas un dépôt ou si
/// git est absent. N compte les fichiers modifiés + untracked
/// (`git status --porcelain`).
fn git_summary(dir: &Path) -> Option<String> {
    let branch_output = kaji::subprocess::git_command()
        .current_dir(dir)
        .args(["branch", "--show-current"])
        .output()
        .ok()?;
    if !branch_output.status.success() {
        return None;
    }
    let branch = String::from_utf8_lossy(&branch_output.stdout)
        .trim()
        .to_string();
    if branch.is_empty() {
        return None;
    }

    let status_output = kaji::subprocess::git_command()
        .current_dir(dir)
        .args(["status", "--porcelain"])
        .output()
        .ok()?;
    if !status_output.status.success() {
        return None;
    }
    let dirty = String::from_utf8_lossy(&status_output.stdout)
        .lines()
        .filter(|line| !line.is_empty())
        .count();

    Some(format!("{branch} ±{dirty}"))
}

fn refresh_git_status() -> Option<String> {
    git_summary(&std::env::current_dir().ok()?)
}

fn budget_from_env(var: &str) -> Option<report::Budget> {
    std::env::var(var)
        .ok()
        .and_then(|v| report::parse_budget(&v))
}

async fn cost_report(session_manager: &SessionManager, session_id: &str) -> Vec<Line<'static>> {
    let config = Config::global();
    let provider = config
        .get_kaji_provider()
        .unwrap_or_else(|_| "?".to_string());
    let model = config.get_kaji_model().unwrap_or_else(|_| "?".to_string());
    match session_manager.usage_windows(session_id).await {
        Ok(windows) => report::cost_table_lines(
            &windows,
            &provider,
            &model,
            budget_from_env("KAJI_BUDGET_5H"),
            budget_from_env("KAJI_BUDGET_7J"),
        ),
        Err(e) => vec![Line::from(Span::styled(
            format!("erreur /cost : {e}"),
            theme::dim(),
        ))],
    }
}

fn docker_report() -> Vec<Line<'static>> {
    let output = std::process::Command::new("docker")
        .args([
            "ps",
            "--format",
            "{{.Names}}\t{{.Image}}\t{{.Status}}\t{{.Ports}}",
        ])
        .output();

    match output {
        Ok(out) if out.status.success() => {
            report::docker_table_lines(&String::from_utf8_lossy(&out.stdout))
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let first_line = stderr.lines().next().unwrap_or("erreur inconnue");
            vec![Line::from(Span::styled(
                format!("docker indisponible — {first_line}"),
                theme::dim(),
            ))]
        }
        Err(e) => vec![Line::from(Span::styled(
            format!("docker indisponible — {e}"),
            theme::dim(),
        ))],
    }
}

fn input_thread(tx: mpsc::Sender<Event>) {
    loop {
        match event::poll(Duration::from_millis(50)) {
            Ok(true) => {
                let Ok(ev) = event::read() else { return };
                if tx.blocking_send(ev).is_err() {
                    return;
                }
            }
            Ok(false) => {
                if tx.is_closed() {
                    return;
                }
            }
            Err(_) => return,
        }
    }
}

type TurnStream<'a> = BoxStream<'a, anyhow::Result<AgentEvent>>;

async fn next_turn_event(turn: &mut Option<TurnStream<'_>>) -> Option<anyhow::Result<AgentEvent>> {
    match turn {
        Some(stream) => stream.next().await,
        None => std::future::pending().await,
    }
}

#[allow(clippy::too_many_arguments)]
async fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    agent: &Agent,
    session_id: &str,
    conversation: kaji::conversation::Conversation,
    spec: Option<SpecDoc>,
    header: String,
    mut input_rx: mpsc::Receiver<Event>,
) -> Result<()> {
    let mut app = App::new(spec);
    app.header = header;
    app.git_status = refresh_git_status();
    seed_chat(&mut app, &conversation);
    push_welcome(&mut app);
    let session_manager = SessionManager::instance();
    let session_config = SessionConfig {
        id: session_id.to_string(),
        schedule_id: None,
        max_turns: None,
        retry_config: None,
    };
    let mut turn: Option<TurnStream<'_>> = None;
    let mut cancel: Option<CancellationToken> = None;
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        terminal.draw(|frame| ui::draw(frame, &app))?;
        tokio::select! {
            _ = tick.tick(), if app.turn_active => {}
            ev = input_rx.recv() => {
                let Some(ev) = ev else { break };
                match app.on_event(&ev) {
                    Action::Quit => break,
                    Action::CancelTurn => {
                        if let Some(token) = &cancel {
                            token.cancel();
                        }
                        if app.driver != PassDriver::Idle {
                            app.pass_abort("tour annulé — passe interrompue");
                        }
                    }
                    Action::Submit(text) => {
                        app.push_user(&text);
                        let started = send_turn(
                            terminal,
                            &mut app,
                            agent,
                            &session_config,
                            &text,
                            &mut turn,
                            &mut cancel,
                        )
                        .await?;
                        if !started && app.driver != PassDriver::Idle {
                            app.pass_abort("échec du démarrage du tour — passe interrompue");
                        }
                    }
                    Action::StartPass => app.start_pass(),
                    Action::GateApprove => {
                        if let Some(prompt) = app.gate_approve() {
                            app.push_system("Exec : envoi de la SPEC à l'agent");
                            let started = send_turn(
                                terminal,
                                &mut app,
                                agent,
                                &session_config,
                                &prompt,
                                &mut turn,
                                &mut cancel,
                            )
                            .await?;
                            if !started {
                                app.pass_abort("échec du démarrage du tour — passe interrompue");
                            }
                        }
                    }
                    Action::GateReject => app.gate_reject(),
                    Action::ToolApprove => {
                        if let Some(req) = app.take_tool_approval() {
                            agent.handle_confirmation(req.id, PermissionConfirmation {
                                principal_type: PrincipalType::Tool,
                                permission: Permission::AllowOnce,
                            }).await;
                            app.push_system(&format!("✓ {} autorisé", req.tool_name));
                        }
                    }
                    Action::ToolDeny => {
                        if let Some(req) = app.take_tool_approval() {
                            agent.handle_confirmation(req.id, PermissionConfirmation {
                                principal_type: PrincipalType::Tool,
                                permission: Permission::DenyOnce,
                            }).await;
                            app.push_system(&format!("✗ {} refusé", req.tool_name));
                        }
                    }
                    Action::Help => push_welcome(&mut app),
                    Action::Cost => {
                        let report = cost_report(&session_manager, session_id).await;
                        app.push_system_lines(report);
                    }
                    Action::Docker => {
                        let report = docker_report();
                        app.push_system_lines(report);
                    }
                    Action::None => {}
                }
            }
            item = next_turn_event(&mut turn), if turn.is_some() => {
                match item {
                    Some(Ok(ev)) => app.apply_agent_event(&ev),
                    Some(Err(e)) => {
                        app.push_system(&format!("erreur: {e}"));
                        turn = None;
                        cancel = None;
                        app.finish_turn();
                        app.git_status = refresh_git_status();
                        if app.driver != PassDriver::Idle {
                            app.pass_abort("erreur pendant la passe — passe interrompue");
                        }
                    }
                    None => {
                        turn = None;
                        cancel = None;
                        app.finish_turn();
                        app.git_status = refresh_git_status();
                        if let Some(prompt) = app.turn_end() {
                            let started = send_turn(
                                terminal,
                                &mut app,
                                agent,
                                &session_config,
                                &prompt,
                                &mut turn,
                                &mut cancel,
                            )
                            .await?;
                            if !started {
                                app.pass_abort("échec du démarrage du tour — passe interrompue");
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn send_turn<'a>(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    agent: &'a Agent,
    session_config: &SessionConfig,
    prompt: &str,
    turn: &mut Option<TurnStream<'a>>,
    cancel: &mut Option<CancellationToken>,
) -> Result<bool> {
    app.status = "démarrage du tour…".to_string();
    terminal.draw(|frame| ui::draw(frame, app))?;
    let token = CancellationToken::new();
    match start_turn(agent, session_config, prompt, &token).await {
        Ok(stream) => {
            app.begin_turn();
            app.status.clear();
            *turn = Some(stream);
            *cancel = Some(token);
            Ok(true)
        }
        Err(e) => {
            app.status.clear();
            app.push_system(&format!("erreur: {e}"));
            Ok(false)
        }
    }
}

async fn start_turn<'a>(
    agent: &'a Agent,
    session_config: &SessionConfig,
    text: &str,
    cancel: &CancellationToken,
) -> anyhow::Result<TurnStream<'a>> {
    let message = Message::user().with_text(text);
    agent
        .reply(message, session_config.clone(), Some(cancel.clone()))
        .await
}

fn resolve_spec(spec_path: Option<PathBuf>) -> Result<Option<SpecDoc>> {
    if let Some(path) = spec_path {
        return SpecDoc::load(&path)
            .map(Some)
            .with_context(|| format!("--spec {}", path.display()));
    }
    let default = PathBuf::from("SPEC.md");
    Ok(default
        .exists()
        .then(|| SpecDoc::load(&default).ok())
        .flatten())
}

fn seed_chat(app: &mut App, conversation: &kaji::conversation::Conversation) {
    for message in conversation.messages() {
        app.apply_agent_event(&AgentEvent::Message(message.clone()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_git(dir: &std::path::Path, args: &[&str]) {
        let output = kaji::subprocess::git_command()
            .current_dir(dir)
            .args(args)
            .output()
            .expect("git available for tests");
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn git_summary_reports_branch_and_dirty_count_in_a_temp_repo() {
        let dir = tempfile::tempdir().unwrap();
        run_git(dir.path(), &["init", "-q"]);
        run_git(dir.path(), &["config", "user.email", "test@example.com"]);
        run_git(dir.path(), &["config", "user.name", "test"]);
        std::fs::write(dir.path().join("a.txt"), "one").unwrap();
        run_git(dir.path(), &["add", "a.txt"]);
        run_git(dir.path(), &["commit", "-q", "-m", "init"]);

        std::fs::write(dir.path().join("a.txt"), "two").unwrap();

        let summary = git_summary(dir.path()).expect("should detect the git repo");
        assert!(
            summary.ends_with(" ±1"),
            "expected one dirty file, got {summary:?}"
        );
    }

    #[test]
    fn git_summary_returns_none_outside_a_repo() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(git_summary(dir.path()), None);
    }

    #[test]
    fn resolve_spec_errors_when_explicit_flag_path_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.md");

        let err = resolve_spec(Some(missing.clone())).expect_err("missing --spec must error");

        assert!(err.to_string().contains(&missing.display().to_string()));
    }

    #[test]
    fn resolve_spec_loads_explicit_flag_path_successfully() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("SPEC.md");
        std::fs::write(&path, "# Demo\ncorps").unwrap();

        let spec = resolve_spec(Some(path))
            .expect("existing --spec must load")
            .expect("Some(path) must resolve to Some(SpecDoc)");

        assert_eq!(spec.title, "Demo");
        assert_eq!(spec.body, "# Demo\ncorps");
    }
}
