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
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub async fn run(
    agent: Agent,
    session_id: String,
    conversation: kaji::conversation::Conversation,
    spec: Option<SpecDoc>,
) -> Result<()> {
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
    app.push_system("/think (ou F3) affiche/masque le raisonnement du modèle (思考中)");
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
        Ok(windows) => {
            let mut lines = report::cost_table_lines(
                &windows,
                &provider,
                &model,
                budget_from_env("KAJI_BUDGET_5H"),
                budget_from_env("KAJI_BUDGET_7J"),
            );
            if let Some(line) = report::condense_line(&kaji::context_mgmt::condense::totals()) {
                lines.push(line);
            }
            lines
        }
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
    maybe_push_welcome(&mut app);
    let session_manager = SessionManager::instance();
    let session_config = SessionConfig {
        id: session_id.to_string(),
        schedule_id: None,
        max_turns: None,
        retry_config: None,
    };
    let mut turn: Option<TurnStream<'_>> = None;
    let mut cancel: Option<CancellationToken> = None;
    let mut pending: Option<Pin<Box<dyn Future<Output = anyhow::Result<TurnStream<'_>>> + '_>>> =
        None;
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        terminal.draw(|frame| ui::draw(frame, &app))?;
        tokio::select! {
            _ = tick.tick(), if app.turn_active || app.turn_pending => {}
            ev = input_rx.recv() => {
                let Some(ev) = ev else { break };
                match app.on_event(&ev) {
                    Action::Quit => break,
                    Action::CancelTurn => {
                        if let Some(token) = &cancel {
                            token.cancel();
                        }
                        if app.turn_pending {
                            // Dropping the setup future is the interruption — nothing
                            // else observes it, so there is no completion event to
                            // wait for the way a running turn's stream provides one.
                            pending = None;
                            cancel = None;
                            app.turn_pending = false;
                            app.status.clear();
                            app.push_system(
                                "démarrage du tour annulé — le message envoyé peut déjà avoir été enregistré côté session",
                            );
                        }
                        if app.driver != PassDriver::Idle {
                            app.pass_abort("tour annulé — passe interrompue");
                        }
                    }
                    Action::Submit(text) => {
                        app.push_user(&text);
                        pending = Some(begin_setup(&mut app, agent, &session_config, &text, &mut cancel));
                    }
                    Action::StartPass => app.start_pass(),
                    Action::GateApprove => {
                        if let Some(prompt) = app.gate_approve() {
                            app.push_system("Exec : envoi de la SPEC à l'agent");
                            pending = Some(begin_setup(&mut app, agent, &session_config, &prompt, &mut cancel));
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
            // The setup future (Agent::reply through its first yield: hooks,
            // add_message, tokenizer…) is polled by this arm of the same
            // select! that polls input_rx — every internal .await it hits
            // hands control back to the other arms, so input stays live for
            // the whole setup instead of freezing the loop for its duration.
            res = async { pending.as_mut().unwrap().await }, if pending.is_some() => {
                pending = None;
                let started = install_turn(&mut app, &mut turn, &mut cancel, res);
                if !started && app.driver != PassDriver::Idle {
                    app.pass_abort("échec du démarrage du tour — passe interrompue");
                }
            }
            item = next_turn_event(&mut turn), if turn.is_some() => {
                match item {
                    Some(Ok(ev)) => app.apply_agent_event(&ev),
                    Some(Err(e)) => {
                        app.push_system(&format!("erreur: {e}"));
                        turn = None;
                        cancel = None;
                        teardown_turn(&mut app);
                        if app.driver != PassDriver::Idle {
                            app.pass_abort("erreur pendant la passe — passe interrompue");
                        }
                    }
                    None => {
                        turn = None;
                        cancel = None;
                        teardown_turn(&mut app);
                        if let Some(prompt) = app.turn_end() {
                            pending = Some(begin_setup(&mut app, agent, &session_config, &prompt, &mut cancel));
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

/// Builds the `Agent::reply` future WITHOUT awaiting it, so `event_loop` can
/// store it as `pending` and poll it from inside its own `select!` — the
/// borrow checker forces this split: a single async fn holding `&mut turn`
/// across the setup await would collide with the other `select!` arms that
/// also need `turn` every iteration `pending` is alive. The cancellation
/// token is created here and written into `cancel` immediately (before any
/// `.await`), not after setup completes, so Esc can reach it right away.
fn begin_setup<'a>(
    app: &mut App,
    agent: &'a Agent,
    session_config: &SessionConfig,
    prompt: &str,
    cancel: &mut Option<CancellationToken>,
) -> Pin<Box<dyn Future<Output = anyhow::Result<TurnStream<'a>>> + 'a>> {
    app.status = "démarrage du tour…".to_string();
    app.turn_pending = true;
    app.reset_turn_visibility();
    let token = CancellationToken::new();
    let message = Message::user().with_text(prompt);
    let fut = agent.reply(message, session_config.clone(), Some(token.clone()));
    *cancel = Some(token);
    Box::pin(fut)
}

/// Consumes the resolved setup future: on success, stores the stream and
/// hands off to `begin_turn`; on failure, resets to a clean idle state
/// (mirrors what `Action::CancelTurn` does when setup is interrupted before
/// it resolves).
fn install_turn<'a>(
    app: &mut App,
    turn: &mut Option<TurnStream<'a>>,
    cancel: &mut Option<CancellationToken>,
    result: anyhow::Result<TurnStream<'a>>,
) -> bool {
    match result {
        Ok(stream) => {
            app.begin_turn();
            app.status.clear();
            *turn = Some(stream);
            true
        }
        Err(e) => {
            app.turn_pending = false;
            app.status.clear();
            *cancel = None;
            app.push_system(&format!("erreur: {e}"));
            false
        }
    }
}

/// Shared cleanup once a turn's event stream ends — cleanly, by error, or
/// by Esc cancellation reaching this arm as the stream's trailing `None`.
/// Clears the turn clock/token bookkeeping, refreshes the git summary, and
/// closes any tool line still awaiting its response (Esc mid-tool-call, or
/// the stream ending with a tool pending) so the ⚙ spinner doesn't stay
/// frozen forever. `close_orphaned_tool_requests` only touches still-pending
/// entries, so calling it here is safe even when every tool already
/// completed normally.
fn teardown_turn(app: &mut App) {
    app.finish_turn();
    app.close_orphaned_tool_requests();
    app.git_status = refresh_git_status();
}

pub fn resolve_spec(spec_path: Option<PathBuf>) -> Result<Option<SpecDoc>> {
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
    app.close_orphaned_tool_requests();
}

/// Skips the first-run welcome banner once a `--resume`d conversation has
/// replayed messages into the chat — repeating "bienvenue dans kaji" after
/// pages of history reads as a bug, not onboarding.
fn maybe_push_welcome(app: &mut App) {
    if app.chat.is_empty() {
        push_welcome(app);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use app::Sender;
    use kaji::conversation::Conversation;

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

    #[test]
    fn seed_chat_replays_persisted_messages_into_chat_lines() {
        let mut app = App::new(None);
        let conversation = Conversation::new_unvalidated([
            Message::user().with_text("salut"),
            Message::assistant().with_text("bonjour"),
        ]);

        seed_chat(&mut app, &conversation);

        assert_eq!(app.chat.len(), 2);
        assert_eq!(app.chat[0].sender, Sender::User);
        assert_eq!(app.chat[0].text, "salut");
        assert_eq!(app.chat[1].sender, Sender::Agent);
        assert_eq!(app.chat[1].text, "bonjour");
    }

    /// `show_thinking` defaults off — a persisted `Thinking` block replayed
    /// via `--resume` must be dropped the same way it would be live,
    /// instead of surfacing reasoning the user never opted into seeing.
    #[test]
    fn seed_chat_drops_persisted_thinking_blocks_when_show_thinking_is_off_by_default() {
        let mut app = App::new(None);
        let mut thinking_msg = Message::assistant().with_thinking("raisonnement persisté", "sig");
        thinking_msg.id = Some("m1".to_string());
        let conversation = Conversation::new_unvalidated([thinking_msg]);

        assert!(!app.show_thinking);
        seed_chat(&mut app, &conversation);

        assert!(!app
            .chat
            .iter()
            .any(|l| matches!(l.sender, Sender::Thinking)));
    }

    #[test]
    fn seed_chat_replays_tool_request_and_response_pair() {
        let mut app = App::new(None);
        let conversation = Conversation::new_unvalidated([
            Message::assistant()
                .with_tool_request("t1", Ok(rmcp::model::CallToolRequestParams::new("shell"))),
            Message::user()
                .with_tool_response("t1", Ok(rmcp::model::CallToolResult::success(vec![]))),
        ]);

        seed_chat(&mut app, &conversation);

        assert!(app
            .chat
            .iter()
            .any(|l| l.text.contains('✓') && l.text.contains("shell")));
        assert!(!app.chat.iter().any(|l| l.tool.is_some()));
    }

    #[test]
    fn seed_chat_closes_unmatched_tool_request_as_interrupted() {
        let mut app = App::new(None);
        let conversation = Conversation::new_unvalidated([Message::assistant()
            .with_tool_request("t1", Ok(rmcp::model::CallToolRequestParams::new("shell")))]);

        seed_chat(&mut app, &conversation);

        assert!(!app.chat.iter().any(|l| l.tool.is_some()));
        assert!(app
            .chat
            .iter()
            .any(|l| l.text.contains("interrompu") && l.text.contains("shell")));
    }

    /// Finding 1: a session interrupted mid-approval persists its
    /// `ActionRequired`/`ToolConfirmation` block (agent.rs adds it before
    /// yielding). Replaying it via `--resume` must not reopen the modal —
    /// the confirmation channel behind it is dead, so `y`/`n` would go
    /// nowhere and swallow all input.
    #[test]
    fn seed_chat_clears_stale_tool_approval_left_by_an_interrupted_resume() {
        let mut app = App::new(None);
        let conversation = Conversation::new_unvalidated([Message::assistant()
            .with_action_required(
                "req-1".to_string(),
                "shell".to_string(),
                Default::default(),
                Some("exécuter `rm -rf /tmp/x` ?".to_string()),
            )]);

        seed_chat(&mut app, &conversation);

        assert!(app.tool_approval.is_none());
    }

    /// Finding 2: after Esc cancels a running turn mid-tool-call (or the
    /// stream ends with a tool pending), the ⚙ spinner must not stay frozen
    /// forever — `teardown_turn` is the shared cleanup every stream-end site
    /// in `event_loop` calls, and it must close orphaned tool lines the same
    /// way `close_orphaned_tool_requests` already does for `--resume`.
    #[test]
    fn teardown_turn_closes_orphaned_tool_line_left_pending_by_a_cancelled_or_errored_stream() {
        let mut app = App::new(None);
        let req_msg = Message::assistant()
            .with_tool_request("t1", Ok(rmcp::model::CallToolRequestParams::new("shell")));
        app.apply_agent_event(&AgentEvent::Message(req_msg));
        assert!(
            app.chat.iter().any(|l| l.tool.is_some()),
            "tool line starts pending"
        );

        teardown_turn(&mut app);

        assert!(
            !app.chat.iter().any(|l| l.tool.is_some()),
            "spinner must not stay frozen after teardown"
        );
        assert!(app.chat.iter().any(|l| l.text.contains('✗')
            && l.text.contains("shell")
            && l.text.contains("interrompu")));
    }

    #[test]
    fn maybe_push_welcome_shows_banner_on_empty_chat() {
        let mut app = App::new(None);

        maybe_push_welcome(&mut app);

        assert!(!app.chat.is_empty());
    }

    #[test]
    fn maybe_push_welcome_stays_silent_after_replayed_history() {
        let mut app = App::new(None);
        let conversation = Conversation::new_unvalidated([Message::user().with_text("salut")]);
        seed_chat(&mut app, &conversation);
        let count_before = app.chat.len();

        maybe_push_welcome(&mut app);

        assert_eq!(app.chat.len(), count_before);
    }
}
