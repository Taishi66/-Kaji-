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
use kaji::checkpoint::{CheckpointId, CheckpointStore};
use kaji::checkpoint_restore::{restore_checkpoint, RestoreOutcome};
use kaji::config::Config;
use kaji::conversation::message::Message;
use kaji::permission::permission_confirmation::PrincipalType;
use kaji::permission::{Permission, PermissionConfirmation};
use kaji::session::session_manager::{InterruptedTurn, SessionEvent};
use kaji::session::SessionManager;
use kaji_core::sdd::SpecDoc;
use ratatui::crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event};
use ratatui::crossterm::execute;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use std::future::Future;
use std::io::stdout;
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
    resume: bool,
) -> Result<()> {
    let header = build_header(&session_id);
    let (input_tx, input_rx) = mpsc::channel::<Event>(64);
    std::thread::spawn(move || input_thread(input_tx));
    let mut terminal = ratatui::init();
    let mouse = mouse_enabled();
    if mouse {
        let _ = execute!(stdout(), EnableMouseCapture);
        install_mouse_panic_hook();
    }
    let result = event_loop(
        &mut terminal,
        &agent,
        &session_id,
        conversation,
        spec,
        header,
        input_rx,
        resume,
    )
    .await;
    if mouse {
        let _ = execute!(stdout(), DisableMouseCapture);
    }
    ratatui::restore();
    result
}

/// `ratatui::init()` already installed a panic hook that restores raw
/// mode/alt-screen, but not mouse capture — a panic while the mouse is
/// captured would otherwise leave the user's shell eating raw mouse escape
/// sequences after the crash. Chains onto the existing hook (installed
/// before this runs, and while still in alt-screen) rather than replacing
/// it, so both cleanups happen. The nominal `DisableMouseCapture` in `run`
/// still runs on the non-panic path — calling it twice is a harmless no-op.
fn install_mouse_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(stdout(), DisableMouseCapture);
        previous(info);
    }));
}

/// `KAJI_MOUSE` kill-switch — mirrors the `condense::enabled` idiom: absent
/// or unrecognized values default to on, `0|false|FALSE|no` turns it off.
/// Off keeps the legacy line-scroll arrow behavior (`App::mouse_enabled`)
/// and skips `EnableMouseCapture`/`DisableMouseCapture` entirely so native
/// terminal text selection (Option/Shift+drag) still works.
fn mouse_enabled() -> bool {
    !std::env::var("KAJI_MOUSE")
        .map(|v| matches!(v.as_str(), "0" | "false" | "FALSE" | "no"))
        .unwrap_or(false)
}

fn build_header(session_id: &str) -> String {
    let config = Config::global();
    let provider = config
        .get_kaji_provider()
        .unwrap_or_else(|_| "?".to_string());
    let model = config.get_kaji_model().unwrap_or_else(|_| "?".to_string());
    format!("{session_id} · {provider}/{model}")
}

/// Each welcome/help section renders as ONE `ChatLine` (via
/// `App::push_system_lines`), never one `ChatLine` per row: `ui::draw_chat`
/// appends a blank line after every `ChatLine`, so one row per push used to
/// blow a blank line in between every single command and nav hint. Grouping
/// rows into a block keeps that blank line where the approved mockup wants
/// it — between sections, not inside them.
///
/// `emphasized` selects the content register: `false` is the startup banner
/// (dim ambiance); `true` is `/help` invoked on-demand, which must read as a
/// normal answer instead of background noise. Section titles (`commandes`,
/// `navigation`) always use `theme::title()` (or patiné) in both registers —
/// only the row content switches between `theme::dim()` and `theme::text()`.
///
/// `App::push_system_lines` splices the `· ` system marker onto the first
/// line of each block only (`ui::push_rendered_lines`), landing it on the
/// welcome banner line and on each section's title row — the same spot
/// `/cost`/`/docker` already put it, not something new here.
fn push_welcome(app: &mut App, emphasized: bool) {
    let content_style = if emphasized {
        theme::text()
    } else {
        theme::dim()
    };

    app.push_system_lines(vec![Line::from(Span::styled(
        "鍛冶 bienvenue dans kaji — tape ton message puis Entrée",
        content_style,
    ))]);
    app.push_system_lines(commands_section(content_style));
    app.push_system_lines(navigation_section(app.mouse_enabled, content_style));
}

fn commands_section(content_style: Style) -> Vec<Line<'static>> {
    let name_width = crate::tui::app::COMMANDS
        .iter()
        .map(|cmd| cmd.name.chars().count())
        .max()
        .unwrap_or(0);
    let mut lines = vec![Line::from(Span::styled("commandes", theme::title()))];
    for cmd in crate::tui::app::COMMANDS {
        lines.push(Line::from(Span::styled(
            format!(
                "  {:<name_width$}   {}",
                cmd.name,
                welcome_command_desc(cmd)
            ),
            content_style,
        )));
    }
    lines
}

/// The command palette (`ui::draw_palette`) and
/// `push_welcome_lists_every_command_from_the_table` both read
/// `Command::desc` straight off `COMMANDS` — that table stays the single
/// source of command copy. A few descriptions overflow the welcome's
/// aligned column layout (env var asides, "affiche/masque" duplication), so
/// this shortens just those for the welcome/help block; anything not listed
/// here falls back to `cmd.desc` unchanged.
fn welcome_command_desc(cmd: &crate::tui::app::Command) -> &'static str {
    match cmd.name {
        "/sdd" => "démarre une passe SDD (SPEC.md ou --spec)",
        "/spec" => "(F2) panneau SPEC on/off",
        "/think" => "(F3) raisonnement du modèle (思考中)",
        "/cost" => "usage tokens/coût (session, 5 h, 7 j)",
        "/docker" => "conteneurs en cours",
        _ => cmd.desc,
    }
}

/// Souris OFF (`KAJI_MOUSE=0`): the wheel isn't captured and ↑/↓ go back to
/// legacy line-scroll (`App::mouse_enabled` guard in `on_event`) instead of
/// prompt-history recall — advertising either would describe controls that
/// don't work. Keeps the pre-mouse-support sentences verbatim (commit
/// 26bd297c8) instead of the aligned key/description rows below, since they
/// were never key/desc pairs to begin with.
fn navigation_section(mouse_enabled: bool, content_style: Style) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(Span::styled("navigation", theme::title()))];
    if mouse_enabled {
        let rows: [(&str, &str); 6] = [
            ("molette", "défile le chat (3 lignes/cran)"),
            ("PageUp/PageDown", "défile par page · Home/End"),
            ("Ctrl+↑/↓", "saute au tour précédent/suivant"),
            ("↑/↓", "historique de prompts"),
            ("Esc", "interrompt · Ctrl+C quitte"),
            ("Option+glisser", "sélectionner du texte"),
        ];
        let key_width = rows
            .iter()
            .map(|(key, _)| key.chars().count())
            .max()
            .unwrap_or(0);
        for (key, desc) in rows {
            lines.push(Line::from(Span::styled(
                format!("  {key:<key_width$}   {desc}"),
                content_style,
            )));
        }
    } else {
        for text in [
            "PageUp/PageDown/Home/End font défiler le chat",
            "Ctrl+↑/↓ saute au tour précédent/suivant",
            "Esc interrompt · Ctrl+C quitte",
        ] {
            lines.push(Line::from(Span::styled(text, content_style)));
        }
    }
    lines
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

/// Builds the `/checkpoints` listing from the session's full event log —
/// pure (no `SessionManager`) so the formatting is unit-testable on its own,
/// mirroring `interrupted_turn_line`. Each line carries the
/// `checkpoint_id` — it is the handle `/restore <id>` takes, so a listing
/// without it would make restore un-typable. Pre-restore snapshots
/// (`captured: "pre_restore"`, journaled by `restore_checkpoint`) are marked
/// as such to distinguish them from the per-turn ones. The payload carries
/// no prompt text, so the preview prefers the sibling `turn_start` event's
/// `query_preview` (same `turn_seq`) — falling back to the checkpoint's own
/// `boundary_message_id`, then a placeholder, if that event is missing
/// (e.g. an old/replayed log).
fn checkpoints_lines(events: &[SessionEvent]) -> Vec<Line<'static>> {
    events
        .iter()
        .filter(|event| event.kind == "checkpoint")
        .filter_map(|checkpoint| {
            let payload: serde_json::Value = serde_json::from_str(&checkpoint.payload_json).ok()?;
            let id = payload
                .get("checkpoint_id")
                .and_then(|v| v.as_str())
                .unwrap_or("?")
                .to_string();
            let marker = if payload.get("captured").and_then(|v| v.as_str()) == Some("pre_restore")
            {
                " · pre-restore"
            } else {
                ""
            };
            let preview = events
                .iter()
                .find(|event| event.kind == "turn_start" && event.turn_seq == checkpoint.turn_seq)
                .and_then(|turn_start| {
                    serde_json::from_str::<serde_json::Value>(&turn_start.payload_json).ok()
                })
                .and_then(|v| {
                    v.get("query_preview")
                        .and_then(|q| q.as_str())
                        .map(str::to_string)
                })
                .or_else(|| {
                    payload
                        .get("boundary_message_id")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                })
                .unwrap_or_else(|| "(sans aperçu)".to_string());
            let preview = crate::tui::ui::sanitize_for_display(&preview);
            Some(Line::from(Span::styled(
                format!("tour {} · {id}{marker} · {preview}", checkpoint.turn_seq),
                theme::system(),
            )))
        })
        .collect()
}

/// Drives the coupled restore (the session project's tree + this session's
/// conversation) through to completion on the store the caller already has.
///
/// `store` is the agent's own wired store (`Agent::checkpoint_store`, pinned
/// to `session.working_dir`) — never one derived here from
/// `std::env::current_dir()`: a resumed session may run from a different
/// directory, and a cwd-derived store keys a different on-disk store than
/// the one holding this session's snapshots. See
/// `perform_restore_uses_the_passed_store_not_the_process_cwd`.
///
/// `restore_checkpoint` is itself `async` and does its blocking git I/O via
/// `tokio::task::block_in_place` internally (requires the multi-thread
/// runtime `main.rs` builds — see `Builder::new_multi_thread`), so this just
/// awaits it directly on `event_loop`'s existing runtime; no extra
/// `spawn`/`block_in_place` wrapping needed here.
async fn perform_restore(
    store: &CheckpointStore,
    session_manager: &SessionManager,
    session_id: &str,
    checkpoint_id: &str,
) -> Result<RestoreOutcome> {
    restore_checkpoint(
        store,
        session_manager,
        session_id,
        &CheckpointId(checkpoint_id.to_string()),
    )
    .await
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
    resume: bool,
) -> Result<()> {
    let mut app = App::new(spec);
    app.header = header;
    app.git_status = refresh_git_status();
    app.mouse_enabled = mouse_enabled();
    seed_chat(&mut app, &conversation);
    maybe_push_welcome(&mut app);
    let session_manager = SessionManager::instance();
    if resume {
        apply_interrupted_turn_marker(
            &mut app,
            session_manager.last_turn_is_interrupted(session_id).await,
        );
    }
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
                    Action::Help => push_welcome(&mut app, true),
                    Action::Cost => {
                        let report = cost_report(&session_manager, session_id).await;
                        app.push_system_lines(report);
                    }
                    Action::Docker => {
                        let report = docker_report();
                        app.push_system_lines(report);
                    }
                    Action::Checkpoints => {
                        match session_manager.events_for_session(session_id).await {
                            Ok(events) => {
                                let lines = checkpoints_lines(&events);
                                if lines.is_empty() {
                                    app.push_system("aucun checkpoint");
                                } else {
                                    app.push_system_lines(lines);
                                }
                            }
                            Err(e) => app.push_system(&format!("erreur /checkpoints : {e}")),
                        }
                    }
                    // Only opens the confirmation modal — spec §3: "jamais
                    // automatique", the actual store/session mutation only
                    // ever runs from `Action::RestoreConfirm` below.
                    Action::Restore(id) => {
                        let is_net = match session_manager.events_for_session(session_id).await {
                            Ok(events) => is_pre_restore_checkpoint(&events, &id),
                            Err(e) => {
                                app.push_system(&format!("erreur /restore : {e}"));
                                false
                            }
                        };
                        app.open_restore_confirm(id, is_net);
                    }
                    Action::RestoreConfirm => {
                        if let Some(id) = app.take_pending_restore() {
                            // The agent's own store, pinned to
                            // `session.working_dir` — see `perform_restore`.
                            match agent.checkpoint_store() {
                                Some(store) => {
                                    match perform_restore(&store, &session_manager, session_id, &id)
                                        .await
                                    {
                                        Ok(outcome) => {
                                            // The session's real conversation
                                            // (truncated for a coupled restore)
                                            // is what the screen must show now.
                                            if let Ok(session) = session_manager
                                                .get_session(session_id, true)
                                                .await
                                            {
                                                if let Some(current) = session.conversation {
                                                    app.reseed_chat(&current);
                                                }
                                            }
                                            if outcome.files_only {
                                                app.push_system(&format!(
                                                    "⚠ filet de sécurité restauré (tour {}) — arbre de travail rembobiné, conversation laissée telle quelle (messages supprimés irrécupérables)",
                                                    outcome.restored_turn
                                                ));
                                            } else {
                                                app.push_system(&format!(
                                                    "⚠ restauré au tour {} — arbre et conversation alignés",
                                                    outcome.restored_turn
                                                ));
                                            }
                                        }
                                        Err(e) => app.push_system(&format!("erreur restore : {e}")),
                                    }
                                }
                                None => app.push_system(
                                    "restore indisponible : aucun store de checkpoints pour cette session",
                                ),
                            }
                        }
                    }
                    Action::RestoreCancel => app.push_system("restore annulé"),
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

/// `true` when `id` is the checkpoint event of a pre-restore safety net
/// (`captured == "pre_restore"`). Such checkpoints are files-only by
/// construction — `restore_checkpoint` rewinds the tree and leaves the
/// conversation untouched, so the confirm modal and the success message
/// must both say so instead of promising a full rewind.
fn is_pre_restore_checkpoint(events: &[SessionEvent], id: &str) -> bool {
    events.iter().any(|event| {
        if event.kind != "checkpoint" {
            return false;
        }
        let Ok(payload) = serde_json::from_str::<serde_json::Value>(&event.payload_json) else {
            return false;
        };
        payload.get("checkpoint_id").and_then(|v| v.as_str()) == Some(id)
            && payload.get("captured").and_then(|v| v.as_str()) == Some("pre_restore")
    })
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
        push_welcome(app, false);
    }
}

/// Formats the system-line text for a detected interrupted turn — kept pure
/// (no `App`/`SessionManager`) so the wording is unit-testable on its own.
fn interrupted_turn_line(it: &InterruptedTurn) -> String {
    let mut line = format!(
        "⚠ tour interrompu au resume — {} événements journalisés non terminés",
        it.event_count
    );
    if let Some(preview) = &it.query_preview {
        // `\n` is preserved by `sanitize_for_display` for the modal's multi-line
        // rendering; this is a single-line system message, so fold it into a
        // visible marker first — otherwise it (like other unsanitized control
        // chars) can break the line or reintroduce the masking the anti-masquage
        // fix on the approval modal closed.
        let preview = crate::tui::ui::sanitize_for_display(&preview.replace('\n', "␊"));
        line.push_str(&format!(" · dernier prompt : \"{preview}\""));
    }
    line
}

/// Applies the outcome of `SessionManager::last_turn_is_interrupted` to the
/// chat: pushes a system line iff the last turn was left open, stays silent
/// on a clean close, and never blocks the resume on a read error — logged,
/// not surfaced, per the spec ("ne jamais bloquer l'ouverture"). Takes the
/// already-resolved `Result` rather than the session/id so the plumbing
/// (this function + `interrupted_turn_line`) is testable without a live
/// `SessionManager`.
fn apply_interrupted_turn_marker(app: &mut App, interrupted: Result<Option<InterruptedTurn>>) {
    match interrupted {
        Ok(Some(it)) => app.push_system(&interrupted_turn_line(&it)),
        Ok(None) => {}
        Err(e) => tracing::warn!("échec de la détection de tour interrompu au resume: {e}"),
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

    fn line_text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    /// BARRIER — a restore must operate on the store the session's snapshots
    /// were written to, never one derived from the process's current
    /// directory: a resumed session may legitimately run from elsewhere
    /// (`handle_resumed_session_workdir` lets the user decline switching
    /// back), and a cwd-derived store keys a different on-disk store — the
    /// session's own checkpoints become unrestorable and the pre-restore
    /// snapshot `git add -A`s an unrelated directory. The test's own cwd is
    /// the kaji repo, deliberately *not* the project being restored.
    #[tokio::test(flavor = "multi_thread")]
    async fn perform_restore_uses_the_passed_store_not_the_process_cwd() {
        use kaji::conversation::message::Message;
        use kaji::session::session_manager::SessionType;

        let data_root = tempfile::tempdir().unwrap();
        let project_root = tempfile::tempdir().unwrap();
        let session_root = tempfile::tempdir().unwrap();
        let _guard = env_lock::lock_env([(
            "KAJI_PATH_ROOT",
            Some(data_root.path().to_str().expect("utf8 temp path")),
        )]);
        let project = project_root.path();
        let store = CheckpointStore::for_project(project).expect("store for the session project");
        let session_manager = SessionManager::new(session_root.path().to_path_buf());
        let session = session_manager
            .create_session(
                project.to_path_buf(),
                "restore-cwd".to_string(),
                SessionType::User,
                Default::default(),
            )
            .await
            .unwrap();

        session_manager
            .add_message(&session.id, &Message::user().with_text("t1").with_id("m1"))
            .await
            .unwrap();
        std::fs::write(project.join("a.txt"), "v1").unwrap();
        let (checkpoint_id, tree) = store.snapshot("turn-1").unwrap();
        session_manager
            .append_event(
                &session.id,
                1,
                "checkpoint",
                &serde_json::json!({
                    "checkpoint_id": checkpoint_id.0,
                    "tree_sha": tree,
                    "captured": "pre_turn",
                    "boundary_message_id": "m1",
                })
                .to_string(),
            )
            .await
            .unwrap();
        std::fs::write(project.join("a.txt"), "v2").unwrap();

        let outcome = perform_restore(&store, &session_manager, &session.id, &checkpoint_id.0)
            .await
            .expect("restore must find the checkpoint in the session's own store");

        assert_eq!(outcome.restored_turn, 1);
        assert_eq!(
            std::fs::read_to_string(project.join("a.txt")).unwrap(),
            "v1",
            "le work-tree de la session est restauré, pas celui du cwd du process"
        );
    }

    #[test]
    fn checkpoints_lines_show_the_restorable_id_and_prompt_preview() {
        let events = vec![
            SessionEvent {
                id: 1,
                turn_seq: 1,
                ts_ms: 0,
                kind: "turn_start".into(),
                payload_json: r#"{"query_preview":"corrige le bug"}"#.into(),
            },
            SessionEvent {
                id: 2,
                turn_seq: 1,
                ts_ms: 0,
                kind: "checkpoint".into(),
                payload_json: r#"{"checkpoint_id":"a1b2c3d4e5f6","tree_sha":"t","captured":"pre_turn","boundary_message_id":"m1"}"#.into(),
            },
        ];

        let lines = checkpoints_lines(&events);

        assert_eq!(lines.len(), 1);
        let text = line_text(&lines[0]);
        assert!(
            text.contains("a1b2c3d4e5f6"),
            "sans l'id dans la ligne, /restore <id> est impossible à saisir : {text}"
        );
        assert!(text.contains("corrige le bug"), "aperçu du prompt : {text}");
    }

    #[test]
    fn checkpoints_lines_mark_pre_restore_snapshots() {
        let events = vec![SessionEvent {
            id: 3,
            turn_seq: 2,
            ts_ms: 0,
            kind: "checkpoint".into(),
            payload_json: r#"{"checkpoint_id":"fedcba987654","tree_sha":"t","captured":"pre_restore","boundary_message_id":"m9"}"#.into(),
        }];

        let lines = checkpoints_lines(&events);

        assert_eq!(lines.len(), 1);
        let text = line_text(&lines[0]);
        assert!(
            text.contains("pre-restore"),
            "un snapshot pre-restore doit être distinguable d'un pre-turn : {text}"
        );
        assert!(text.contains("fedcba987654"), "id restaurable : {text}");
    }

    #[test]
    fn is_pre_restore_checkpoint_only_matches_pre_restore_nets() {
        let events = vec![
            SessionEvent {
                id: 1,
                turn_seq: 1,
                ts_ms: 0,
                kind: "checkpoint".into(),
                payload_json: r#"{"checkpoint_id":"a1b2c3d4e5f6","tree_sha":"t","captured":"pre_restore","boundary_message_id":"m9"}"#.into(),
            },
            SessionEvent {
                id: 2,
                turn_seq: 1,
                ts_ms: 0,
                kind: "checkpoint".into(),
                payload_json: r#"{"checkpoint_id":"bbbbbbbbbbbb","tree_sha":"t","captured":"pre_turn"}"#.into(),
            },
        ];

        assert!(
            is_pre_restore_checkpoint(&events, "a1b2c3d4e5f6"),
            "le filet pre-restore doit être identifié comme fichiers-seule"
        );
        assert!(
            !is_pre_restore_checkpoint(&events, "bbbbbbbbbbbb"),
            "un checkpoint pre-turn n'est pas un filet"
        );
        assert!(
            !is_pre_restore_checkpoint(&events, "inconnu"),
            "un id absent ne doit pas matcher"
        );
        assert!(
            !is_pre_restore_checkpoint(
                &[SessionEvent {
                    id: 3,
                    turn_seq: 1,
                    ts_ms: 0,
                    kind: "checkpoint".into(),
                    payload_json: "pas du json".into(),
                }],
                "a1b2c3d4e5f6"
            ),
            "un payload illisible ne doit pas être un filet"
        );
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

    #[tokio::test]
    async fn resume_surfaces_an_interrupted_turn_marker() {
        use kaji::config::KajiMode;
        use kaji::session::SessionType;

        let temp_dir = tempfile::tempdir().unwrap();
        let sm = SessionManager::new(temp_dir.path().to_path_buf());
        let sid = sm
            .create_session(
                PathBuf::from("/tmp"),
                "s".to_string(),
                SessionType::User,
                KajiMode::default(),
            )
            .await
            .unwrap()
            .id;
        sm.append_event(&sid, 1, "turn_start", r#"{"query_preview":"q1"}"#)
            .await
            .unwrap();
        sm.append_event(&sid, 1, "message", "{}").await.unwrap();

        let mut app = App::new(None);
        apply_interrupted_turn_marker(&mut app, sm.last_turn_is_interrupted(&sid).await);

        assert!(app
            .chat
            .iter()
            .any(|l| l.sender == Sender::System && l.text.contains("tour interrompu")));
    }

    #[tokio::test]
    async fn resume_stays_silent_when_last_turn_closed_cleanly() {
        use kaji::config::KajiMode;
        use kaji::session::SessionType;

        let temp_dir = tempfile::tempdir().unwrap();
        let sm = SessionManager::new(temp_dir.path().to_path_buf());
        let sid = sm
            .create_session(
                PathBuf::from("/tmp"),
                "s".to_string(),
                SessionType::User,
                KajiMode::default(),
            )
            .await
            .unwrap()
            .id;
        sm.append_event(&sid, 1, "turn_start", r#"{"query_preview":"q1"}"#)
            .await
            .unwrap();
        sm.append_event(&sid, 1, "turn_end", "{}").await.unwrap();

        let mut app = App::new(None);
        apply_interrupted_turn_marker(&mut app, sm.last_turn_is_interrupted(&sid).await);

        assert!(!app.chat.iter().any(|l| l.text.contains("tour interrompu")));
    }

    #[test]
    fn interrupted_turn_line_includes_event_count_and_query_preview() {
        let it = InterruptedTurn {
            turn_seq: 2,
            event_count: 3,
            query_preview: Some("q2".to_string()),
        };

        let line = interrupted_turn_line(&it);

        assert!(line.contains("tour interrompu"));
        assert!(line.contains('3'));
        assert!(line.contains("q2"));
    }

    #[test]
    fn interrupted_turn_line_sanitizes_control_chars_in_preview() {
        let it = InterruptedTurn {
            turn_seq: 2,
            event_count: 1,
            query_preview: Some("line one\nline two\x1bmalicious".to_string()),
        };

        let line = interrupted_turn_line(&it);

        assert!(
            !line.contains('\n'),
            "a raw newline must not reach the single-line system message: {line:?}"
        );
        assert!(
            !line.contains('\x1b'),
            "a raw ESC must not reach the terminal unsanitized: {line:?}"
        );
        assert!(line.contains("line one"));
        assert!(line.contains("line two"));
        assert!(line.contains("malicious"));
    }

    #[test]
    fn apply_interrupted_turn_marker_stays_silent_on_read_error() {
        let mut app = App::new(None);

        apply_interrupted_turn_marker(&mut app, Err(anyhow::anyhow!("boom")));

        assert!(app.chat.is_empty());
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

    fn welcome_text(app: &App) -> String {
        app.chat
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn push_welcome_mentions_wheel_and_history_arrows_when_mouse_is_enabled() {
        let mut app = App::new(None);
        app.mouse_enabled = true;

        push_welcome(&mut app, false);

        let text = welcome_text(&app);
        assert!(text.contains("molette"));
        assert!(text.contains("↑/↓"));
        assert!(text.contains("historique de prompts"));
    }

    /// Souris OFF (`KAJI_MOUSE=0`) — the arrows go back to line-scrolling
    /// the chat (`App::mouse_enabled` guard in `on_event`) and prompt
    /// history is unreachable from the keyboard, so advertising the wheel
    /// or ↑/↓ recall would describe controls that don't work.
    #[test]
    fn push_welcome_falls_back_to_legacy_scroll_hint_when_mouse_is_disabled() {
        let mut app = App::new(None);
        app.mouse_enabled = false;

        push_welcome(&mut app, false);

        let text = welcome_text(&app);
        assert!(!text.contains("molette"));
        assert!(!text.contains("↑/↓ rappelle"));
        assert!(text.contains("PageUp/PageDown/Home/End"));
        assert!(text.contains("Ctrl+↑/↓"));
    }

    #[test]
    fn push_welcome_lists_every_command_from_the_table() {
        let mut app = App::new(None);
        push_welcome(&mut app, false);
        let text = welcome_text(&app);
        for cmd in crate::tui::app::COMMANDS {
            assert!(
                text.contains(cmd.name),
                "{} absent du welcome/help",
                cmd.name
            );
        }
    }

    /// Renders `app` against a fixed-size `TestBackend` and returns each row
    /// as a plain string, one cell per character — wide-glyph continuation
    /// cells come back empty from `symbol()` and fall back to a space here,
    /// which keeps column offsets in the returned strings matching terminal
    /// columns 1:1.
    fn rendered_rows(app: &App) -> Vec<String> {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let backend = TestBackend::new(120, 60);
        let mut terminal = Terminal::new(backend).expect("test backend terminal");
        terminal
            .draw(|frame| ui::draw(frame, app))
            .expect("draw must succeed against a TestBackend");
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().chars().next().unwrap_or(' '))
                    .collect::<String>()
            })
            .collect()
    }

    fn welcome_line_fg(app: &App, needle: &str) -> ratatui::style::Color {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let backend = TestBackend::new(120, 60);
        let mut terminal = Terminal::new(backend).expect("test backend terminal");
        terminal
            .draw(|frame| ui::draw(frame, app))
            .expect("draw must succeed against a TestBackend");
        let buffer = terminal.backend().buffer();
        let target = needle.chars().next().expect("non-empty needle");

        for (y, row) in rendered_rows(app).iter().enumerate() {
            if !row.contains(needle) {
                continue;
            }
            for x in 0..buffer.area.width {
                if buffer[(x, y as u16)].symbol() == target.to_string() {
                    return buffer[(x, y as u16)].fg;
                }
            }
        }
        panic!("row containing {needle:?} not found in rendered buffer");
    }

    /// The old design pushed one `ChatLine` per row, so `draw_chat`'s
    /// per-`ChatLine` trailing blank line landed after every single command
    /// and nav hint — flat and over-aired. Grouping rows into one
    /// `ChatLine` per section (see `push_welcome`) means that blank line
    /// only falls between sections now; this locks that shape in.
    #[test]
    fn welcome_renders_as_compact_sections() {
        let mut app = App::new(None);
        app.mouse_enabled = true;

        push_welcome(&mut app, false);

        let rows = rendered_rows(&app);
        let commands_row = rows
            .iter()
            .position(|r| r.contains("commandes"))
            .expect("commandes section header must render");
        let navigation_row = rows
            .iter()
            .position(|r| r.contains("navigation"))
            .expect("navigation section header must render");
        assert!(
            navigation_row > commands_row,
            "navigation section must render after commandes"
        );

        for (name, next_name) in [
            ("/sdd", "/spec"),
            ("/spec", "/think"),
            ("/think", "/cost"),
            ("/cost", "/docker"),
            ("/docker", "/checkpoints"),
            ("/checkpoints", "/help"),
            ("/help", "/quit"),
        ] {
            let row = rows
                .iter()
                .position(|r| r.contains(name))
                .unwrap_or_else(|| panic!("{name} row missing from welcome"));
            assert!(
                rows[row + 1].contains(next_name),
                "expected {next_name} on the row right after {name} (no blank line \
                 between commands), got {:?}",
                rows[row + 1]
            );
        }
    }

    #[test]
    fn welcome_command_names_are_column_aligned() {
        let mut app = App::new(None);

        push_welcome(&mut app, false);

        let rows = rendered_rows(&app);
        let sdd_row = rows
            .iter()
            .find(|r| r.contains("/sdd"))
            .expect("/sdd row must render");
        let docker_row = rows
            .iter()
            .find(|r| r.contains("/docker"))
            .expect("/docker row must render");

        let sdd_desc_col = sdd_row.find("démarre").expect("/sdd description text");
        let docker_desc_col = docker_row
            .find("conteneurs")
            .expect("/docker description text");
        assert_eq!(
            sdd_desc_col, docker_desc_col,
            "command descriptions must start at the same column regardless of name length"
        );
    }

    #[test]
    fn help_command_renders_in_normal_text_style() {
        let mut app = App::new(None);

        push_welcome(&mut app, true);

        assert_eq!(
            welcome_line_fg(&app, "bienvenue"),
            theme::ENCRE,
            "/help must render like a normal answer, not the dim welcome ambiance"
        );
    }

    #[test]
    fn startup_welcome_stays_dim() {
        let mut app = App::new(None);

        push_welcome(&mut app, false);

        assert_eq!(
            welcome_line_fg(&app, "bienvenue"),
            ratatui::style::Color::DarkGray,
            "the startup banner must keep its dim ambiance style"
        );
    }
}
