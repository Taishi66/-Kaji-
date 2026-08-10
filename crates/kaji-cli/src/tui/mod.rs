pub mod app;
pub mod markdown;
pub mod theme;
pub mod ui;

use anyhow::Result;
use app::{Action, App, PassDriver};
use futures::stream::BoxStream;
use futures::StreamExt;
use kaji::agents::{Agent, AgentEvent, SessionConfig};
use kaji::config::Config;
use kaji::conversation::message::Message;
use kaji::permission::permission_confirmation::PrincipalType;
use kaji::permission::{Permission, PermissionConfirmation};
use kaji::session::{SessionManager, UsageAggregate, UsageWindows};
use kaji_core::sdd::SpecDoc;
use ratatui::crossterm::event::{self, Event};
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
    let header = build_header(&session_id);
    let (input_tx, input_rx) = mpsc::channel::<Event>(64);
    std::thread::spawn(move || input_thread(input_tx));
    let mut terminal = ratatui::init();
    let result = event_loop(
        &mut terminal,
        &agent,
        &session_id,
        conversation,
        spec_path,
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
    app.push_system("/cost affiche l'usage tokens/coût (session, 5 h, 7 j)");
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

fn format_usage_row(label: &str, agg: &UsageAggregate) -> String {
    let cost = agg.cost.map(|c| format!(" · ${c:.2}")).unwrap_or_default();
    format!(
        "- {label} : ↑{} ↓{} ({} tokens){cost}",
        agg.input_tokens, agg.output_tokens, agg.total_tokens
    )
}

fn format_cost_block(windows: &UsageWindows, provider: &str, model: &str) -> String {
    let mut lines = vec![
        format!("**/cost** — provider `{provider}/{model}`"),
        String::new(),
        format_usage_row("session courante", &windows.session),
        format_usage_row("5 h", &windows.last_5h),
        format_usage_row("7 j", &windows.last_7d),
    ];
    let cost_unknown_everywhere = windows.session.cost.is_none()
        && windows.last_5h.cost.is_none()
        && windows.last_7d.cost.is_none();
    if cost_unknown_everywhere {
        lines.push(String::new());
        lines.push("coût : n/a (provider sans tarification — tokens seulement)".to_string());
    }
    lines.join("\n")
}

async fn cost_report(session_manager: &SessionManager, session_id: &str) -> String {
    let config = Config::global();
    let provider = config
        .get_kaji_provider()
        .unwrap_or_else(|_| "?".to_string());
    let model = config.get_kaji_model().unwrap_or_else(|_| "?".to_string());
    match session_manager.usage_windows(session_id).await {
        Ok(windows) => format_cost_block(&windows, &provider, &model),
        Err(e) => format!("erreur /cost : {e}"),
    }
}

/// Parse la sortie de `docker ps --format "{{.Names}}\t{{.Image}}\t{{.Status}}\t{{.Ports}}"`
/// en tableau markdown Names/Image/Status/Ports.
fn format_docker_table(raw_output: &str) -> String {
    let rows: Vec<[&str; 4]> = raw_output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| {
            let mut cols = line.split('\t');
            Some([
                cols.next()?,
                cols.next()?,
                cols.next()?,
                cols.next().unwrap_or(""),
            ])
        })
        .collect();

    if rows.is_empty() {
        return "docker : aucun conteneur en cours".to_string();
    }

    let mut out = String::from("| Names | Image | Status | Ports |\n|---|---|---|---|");
    for row in rows {
        out.push('\n');
        out.push_str(&format!(
            "| {} | {} | {} | {} |",
            row[0], row[1], row[2], row[3]
        ));
    }
    out
}

fn docker_report() -> String {
    let output = std::process::Command::new("docker")
        .args([
            "ps",
            "--format",
            "{{.Names}}\t{{.Image}}\t{{.Status}}\t{{.Ports}}",
        ])
        .output();

    match output {
        Ok(out) if out.status.success() => {
            format_docker_table(&String::from_utf8_lossy(&out.stdout))
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let first_line = stderr.lines().next().unwrap_or("erreur inconnue");
            format!("docker indisponible — {first_line}")
        }
        Err(e) => format!("docker indisponible — {e}"),
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
    spec_path: Option<PathBuf>,
    header: String,
    mut input_rx: mpsc::Receiver<Event>,
) -> Result<()> {
    let mut app = App::new(resolve_spec(spec_path));
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
                        app.push_system_markdown(&report);
                    }
                    Action::Docker => {
                        let report = docker_report();
                        app.push_system_markdown(&report);
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

fn resolve_spec(spec_path: Option<PathBuf>) -> Option<SpecDoc> {
    let path = spec_path.or_else(|| {
        let default = PathBuf::from("SPEC.md");
        default.exists().then_some(default)
    })?;
    SpecDoc::load(&path).ok()
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
    fn format_docker_table_renders_markdown_table_from_fixture() {
        let fixture =
            "web\tnginx:latest\tUp 3 hours\t0.0.0.0:80->80/tcp\ndb\tpostgres:16\tUp 3 hours\t";
        let table = format_docker_table(fixture);
        assert!(table.starts_with("| Names | Image | Status | Ports |\n|---|---|---|---|"));
        assert!(table.contains("| web | nginx:latest | Up 3 hours | 0.0.0.0:80->80/tcp |"));
        assert!(table.contains("| db | postgres:16 | Up 3 hours |  |"));
    }

    #[test]
    fn format_docker_table_empty_output_reports_no_containers() {
        assert_eq!(format_docker_table(""), "docker : aucun conteneur en cours");
        assert_eq!(
            format_docker_table("\n\n"),
            "docker : aucun conteneur en cours"
        );
    }

    fn aggregate(input: i64, output: i64, cost: Option<f64>) -> UsageAggregate {
        UsageAggregate {
            input_tokens: input,
            output_tokens: output,
            total_tokens: input + output,
            cost,
        }
    }

    #[test]
    fn format_cost_block_shows_per_window_cost_when_known() {
        let windows = UsageWindows {
            session: aggregate(100, 20, Some(0.10)),
            last_5h: aggregate(1000, 200, Some(1.50)),
            last_7d: aggregate(9000, 2000, Some(12.00)),
        };
        let report = format_cost_block(&windows, "anthropic", "claude-sonnet");
        assert!(report.contains("anthropic/claude-sonnet"));
        assert!(report.contains("session courante : ↑100 ↓20 (120 tokens) · $0.10"));
        assert!(report.contains("5 h : ↑1000 ↓200 (1200 tokens) · $1.50"));
        assert!(report.contains("7 j : ↑9000 ↓2000 (11000 tokens) · $12.00"));
        assert!(!report.contains("n/a"));
    }

    #[test]
    fn format_cost_block_reports_na_when_no_window_has_a_cost() {
        let windows = UsageWindows {
            session: aggregate(100, 20, None),
            last_5h: aggregate(1000, 200, None),
            last_7d: aggregate(9000, 2000, None),
        };
        let report = format_cost_block(&windows, "ollama", "llama3");
        assert!(report.contains("coût : n/a (provider sans tarification — tokens seulement)"));
        assert!(!report.contains('$'));
    }
}
