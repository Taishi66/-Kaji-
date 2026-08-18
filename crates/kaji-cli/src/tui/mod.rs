pub mod app;
pub mod diff;
pub mod explorer;
pub mod fuzzy;
pub mod gitstatus;
pub mod markdown;
pub mod mentions;
pub mod report;
pub mod theme;
pub mod ui;
pub mod viewer;

use anyhow::{Context, Result};
use app::{Action, App, PassDriver, RoledLine, RoledSpan, ToolApprovalRequest};
use futures::StreamExt;
use futures::stream::BoxStream;
use kaji::agents::{Agent, AgentEvent, SessionConfig};
use kaji::checkpoint::{CheckpointId, CheckpointStore};
use kaji::checkpoint_restore::{RestoreOutcome, restore_checkpoint};
use kaji::config::{Config, KajiMode};
use kaji::conversation::message::Message;
use kaji::permission::permission_confirmation::PrincipalType;
use kaji::permission::{Permission, PermissionConfirmation};
use kaji::session::SessionManager;
use kaji::session::session_manager::{InterruptedTurn, SessionEvent};
use kaji_core::sdd::SpecDoc;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event,
};
use ratatui::crossterm::execute;
use std::future::Future;
use std::io::stdout;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::Duration;
use theme::SpanRole;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// How often the status bar re-reads the repository. Slow enough that a
/// terminal left open costs two short `git` calls every 5 s, fast enough that
/// a commit made in another window shows up on its own.
const GIT_REFRESH_INTERVAL: Duration = Duration::from_secs(5);

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
    // Bracketed paste (item 4 ante): without it a pasted path arrives as a
    // burst of `Char` events and nothing can tell it apart from typing.
    let _ = execute!(stdout(), EnableBracketedPaste);
    if mouse {
        let _ = execute!(stdout(), EnableMouseCapture);
    }
    install_terminal_panic_hook(mouse);
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
    let _ = execute!(stdout(), DisableBracketedPaste);
    ratatui::restore();
    result
}

/// `ratatui::init()` already installed a panic hook that restores raw
/// mode/alt-screen, but neither mouse capture nor bracketed paste — a panic
/// while either is on would otherwise leave the user's shell eating raw
/// escape sequences after the crash. Chains onto the existing hook
/// (installed before this runs, and while still in alt-screen) rather than
/// replacing it, so both cleanups happen. The nominal disables in `run`
/// still run on the non-panic path — calling them twice is a harmless no-op.
fn install_terminal_panic_hook(mouse: bool) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if mouse {
            let _ = execute!(stdout(), DisableMouseCapture);
        }
        let _ = execute!(stdout(), DisableBracketedPaste);
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

/// Thème de démarrage : `KAJI_THEME` — `Config::get_param` lit déjà la
/// variable d'environnement avant le fichier de config, la précédence env >
/// config n'a donc pas à être refaite ici. Une valeur inconnue laisse le thème
/// par défaut (`zen`, déjà actif au lancement), jamais un lancement qui échoue.
/// L'avertissement éventuel est rendu par l'appelant après
/// `maybe_push_welcome`, sinon il rendrait le chat non vide et masquerait la
/// bannière d'accueil.
///
/// Le thème actif au lancement ne fige plus rien : les blocs pré-rendus
/// portent un rôle, résolu au draw (`ui::push_rendered_lines`).
fn apply_startup_theme() -> Option<String> {
    let requested = Config::global().get_param::<String>("KAJI_THEME").ok()?;
    let err = theme::set_active(&requested).err()?;
    let fallback = theme::resolve_theme(Some(&requested), None);
    Some(format!("{err} — {fallback} appliqué"))
}

/// A session-wide or permanent grant must say what it just covered, in the
/// same form the permission list stores — "autorisé" alone would hide that the
/// answer widened `cargo test -p x` into every `cargo test`.
fn tool_answer_note(req: &ToolApprovalRequest, permission: &Permission) -> String {
    match permission {
        Permission::AllowSession => format!(
            "✓ {} autorisé pour la session : {}",
            req.tool_name,
            req.grant_label()
        ),
        Permission::AlwaysAllow => format!(
            "✓ {} toujours autorisé : {}",
            req.tool_name,
            req.grant_label()
        ),
        other if other.allows_execution() => format!("✓ {} autorisé", req.tool_name),
        _ => format!("✗ {} refusé", req.tool_name),
    }
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
/// `navigation`) always take the `Title` role (or patiné) in both registers —
/// only the row content switches between `Dim` and `Text`.
///
/// `App::push_system_lines` splices the `· ` system marker onto the first
/// line of each block only (`ui::push_rendered_lines`), landing it on the
/// welcome banner line and on each section's title row — the same spot
/// `/cost`/`/docker` already put it, not something new here.
fn push_welcome(app: &mut App, emphasized: bool) {
    let content_role = if emphasized {
        SpanRole::Text
    } else {
        SpanRole::Dim
    };

    app.push_system_lines(vec![vec![RoledSpan::new(
        "鍛冶 bienvenue dans kaji — tape ton message puis Entrée",
        content_role,
    )]]);
    app.push_system_lines(commands_section(content_role));
    app.push_system_lines(navigation_section(app.mouse_enabled, content_role));
}

fn commands_section(content_role: SpanRole) -> Vec<RoledLine> {
    let name_width = crate::tui::app::COMMANDS
        .iter()
        .map(|cmd| cmd.name.chars().count())
        .max()
        .unwrap_or(0);
    let mut lines = vec![vec![RoledSpan::title("commandes")]];
    for cmd in crate::tui::app::COMMANDS {
        lines.push(vec![RoledSpan::new(
            format!(
                "  {:<name_width$}   {}",
                cmd.name,
                welcome_command_desc(cmd)
            ),
            content_role,
        )]);
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
        "/goal" => "boucle évaluée vers un but (<condition> | clear)",
        "/files" => "(Ctrl+P) recherche floue de fichiers",
        "/explorer" => "(Ctrl+E) explorateur de fichiers",
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
fn navigation_section(mouse_enabled: bool, content_role: SpanRole) -> Vec<RoledLine> {
    let mut lines = vec![vec![RoledSpan::title("navigation")]];
    if mouse_enabled {
        let rows: [(&str, &str); 11] = [
            ("molette", "défile le chat (3 lignes/cran)"),
            ("PageUp/PageDown", "défile par page · Home/End"),
            ("Ctrl+↑/↓", "saute au tour précédent/suivant"),
            ("↑/↓", "historique de prompts"),
            ("Ctrl+P", "recherche floue de fichiers (/files)"),
            ("Ctrl+E", "explorateur de fichiers (/explorer)"),
            (
                "Ctrl+O",
                "change de volet (composer → explorateur → lecteur)",
            ),
            (
                "Ctrl+S",
                "steer : envoie les messages en file au tour en cours",
            ),
            ("Shift+Tab", "change le mode (approve → smart → auto)"),
            ("Esc", "interrompt · Ctrl+C quitte"),
            ("Option+glisser", "sélectionner du texte"),
        ];
        let key_width = rows
            .iter()
            .map(|(key, _)| key.chars().count())
            .max()
            .unwrap_or(0);
        for (key, desc) in rows {
            lines.push(vec![RoledSpan::new(
                format!("  {key:<key_width$}   {desc}"),
                content_role,
            )]);
        }
    } else {
        for text in [
            "PageUp/PageDown/Home/End font défiler le chat",
            "Ctrl+↑/↓ saute au tour précédent/suivant",
            "Ctrl+P recherche floue de fichiers (/files)",
            "Ctrl+E explorateur de fichiers (/explorer)",
            "Ctrl+O change de volet (composer → explorateur → lecteur)",
            "Ctrl+S steer : envoie les messages en file au tour en cours",
            "Shift+Tab change le mode (approve → smart → auto)",
            "Esc interrompt · Ctrl+C quitte",
        ] {
            lines.push(vec![RoledSpan::new(text, content_role)]);
        }
    }
    lines
}

fn budget_from_env(var: &str) -> Option<report::Budget> {
    std::env::var(var)
        .ok()
        .and_then(|v| report::parse_budget(&v))
}

async fn cost_report(session_manager: &SessionManager, session_id: &str) -> Vec<RoledLine> {
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
        Err(e) => vec![vec![RoledSpan::dim(format!("erreur /cost : {e}"))]],
    }
}

/// `working_dir` comes from the session itself — the same source the turn
/// uses (`Agent::reply` → `session.working_dir`) — never the process cwd,
/// which a resumed session may no longer be running from.
async fn context_report_lines(
    agent: &Agent,
    session_manager: &SessionManager,
    session_id: &str,
) -> Vec<RoledLine> {
    let config = Config::global();
    let provider = config
        .get_kaji_provider()
        .unwrap_or_else(|_| "?".to_string());
    let model = config.get_kaji_model().unwrap_or_else(|_| "?".to_string());

    let report = match session_manager.get_session(session_id, false).await {
        Ok(session) => {
            agent
                .context_report(session_id, session.working_dir.as_path())
                .await
        }
        Err(e) => Err(e),
    };

    match report {
        Ok(breakdown) => report::context_table_lines(&breakdown, &provider, &model),
        Err(e) => vec![vec![RoledSpan::dim(format!("erreur /context : {e}"))]],
    }
}

fn docker_report() -> Vec<RoledLine> {
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
            vec![vec![RoledSpan::dim(format!(
                "docker indisponible — {first_line}"
            ))]]
        }
        Err(e) => vec![vec![RoledSpan::dim(format!("docker indisponible — {e}"))]],
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
fn checkpoints_lines(events: &[SessionEvent]) -> Vec<RoledLine> {
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
            Some(vec![RoledSpan::system(format!(
                "tour {} · {id}{marker} · {preview}",
                checkpoint.turn_seq
            ))])
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

/// Every relative path the TUI resolves — @-mention index, pasted paths, the
/// `write` preview of the approval modal — must share the base the tools
/// themselves write against (`Agent::reply` → `session.working_dir`). A resumed
/// session only *proposes* the chdir (`session/builder.rs`), so the process cwd
/// is a different tree as soon as the user declines; it stays the fallback for
/// a session that can't be read.
/// `checkpoints désactivés — <raison>` when `wire_checkpoint_store` declined
/// to snapshot this session's working directory (see
/// `kaji::checkpoint::eligibility`), `None` otherwise. An absent store with no
/// reason stays silent: that is the ordinary shape of every non-TUI caller,
/// not a refusal that needs explaining.
fn checkpoints_disabled_line(has_store: bool, reason: Option<&str>) -> Option<String> {
    if has_store {
        return None;
    }
    reason.map(|reason| format!("checkpoints désactivés — {reason}"))
}

async fn session_working_dir(session_manager: &SessionManager, session_id: &str) -> PathBuf {
    match session_manager.get_session(session_id, false).await {
        Ok(session) => session.working_dir,
        Err(_) => std::env::current_dir().unwrap_or_default(),
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
    app.mouse_enabled = mouse_enabled();
    app.kaji_mode = agent.kaji_mode().await;
    let session_manager = SessionManager::instance();
    let working_dir = session_working_dir(&session_manager, session_id).await;
    app.set_working_dir(working_dir.clone());
    app.request_git_refresh();
    let theme_warning = apply_startup_theme();
    seed_chat(&mut app, &conversation);
    maybe_push_welcome(&mut app);
    if let Some(warning) = theme_warning {
        app.push_system(&warning);
    }
    let disabled_checkpoints = checkpoints_disabled_line(
        agent.checkpoint_store().is_some(),
        agent.checkpoint_disabled_reason(),
    );
    if let Some(line) = &disabled_checkpoints {
        app.push_system(&format!("{line} ({})", working_dir.display()));
    }
    if resume {
        apply_interrupted_turn_marker(
            &mut app,
            session_manager.last_turn_is_interrupted(session_id).await,
        );
        apply_interrupted_goal_marker(&mut app, &session_manager, session_id).await;
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
    let (suggestion_tx, mut suggestion_rx) = mpsc::channel::<String>(1);
    let (index_tx, mut index_rx) = mpsc::channel::<mentions::MentionIndex>(1);
    let (git_tx, mut git_rx) = mpsc::channel::<Option<gitstatus::GitStatus>>(1);
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The status bar's own beat, armed even when nothing is running: the
    // working tree changes under a `git` run in another terminal too, and a
    // read that is already in flight is never doubled (`request_git_refresh`).
    let mut git_tick = tokio::time::interval(GIT_REFRESH_INTERVAL);
    git_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        terminal.draw(|frame| ui::draw(frame, &app))?;
        tokio::select! {
            _ = tick.tick(), if app.turn_active || app.turn_pending => {}
            _ = git_tick.tick() => app.request_git_refresh(),
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
                        app.goal_abort("目標 tour annulé — but interrompu");
                    }
                    Action::SteerNow => {
                        // Live guidance (item 2 ante): interrupt the running
                        // turn — the queued message then auto-submits on the
                        // turn's natural teardown path below, exactly like the
                        // auto-flush at turn end.
                        if let Some(token) = &cancel {
                            token.cancel();
                        }
                        if app.turn_pending {
                            pending = None;
                            cancel = None;
                            app.turn_pending = false;
                            app.status.clear();
                        }
                        if app.driver != PassDriver::Idle {
                            app.pass_abort("tour steeré — passe interrompue");
                        }
                        app.goal_abort("目標 tour steeré — but interrompu");
                        if !app.turn_active {
                            // No stream is running to drain the queue on
                            // teardown (cancel of a pending setup falls here) —
                            // flush the queue straight into a fresh turn.
                            flush_steer_queue(
                                &mut app,
                                agent,
                                &session_config,
                                &mut pending,
                                &mut cancel,
                                &working_dir,
                            );
                        }
                        app.push_system(&format!(
                            "{} steering — message injecté comme guidance",
                            theme::STEER_GLYPH
                        ));
                    }
                    Action::Submit(text) => {
                        app.push_user(&text);
                        let expanded = mentions::expand_mentions(&text, &working_dir);
                        pending = Some(begin_setup(&mut app, agent, &session_config, &expanded, &mut cancel));
                    }
                    Action::StartPass => app.start_pass(),
                    // Goal session (item 5 ante): the first work turn starts
                    // here, every following one from `turn_end` — the same
                    // chaining the SDD pass uses.
                    Action::GoalSet(condition) => {
                        if let Some(prompt) = goal_work_prompt(&mut app, &condition, &working_dir) {
                            pending = Some(begin_setup(&mut app, agent, &session_config, &prompt, &mut cancel));
                        }
                    }
                    Action::GoalStatus => app.push_goal_status(),
                    // The goal itself is already stopped (`App::goal_clear`);
                    // what's left is the turn it was driving, cancelled here
                    // exactly as Esc would.
                    Action::GoalClear => {
                        if let Some(token) = &cancel {
                            token.cancel();
                        }
                        if app.turn_pending {
                            pending = None;
                            cancel = None;
                            app.turn_pending = false;
                            app.status.clear();
                        }
                    }
                    Action::GateApprove => {
                        if let Some(prompt) = app.gate_approve() {
                            app.push_system("Exec : envoi de la SPEC à l'agent");
                            pending = Some(begin_setup(&mut app, agent, &session_config, &prompt, &mut cancel));
                        }
                    }
                    Action::GateReject => app.gate_reject(),
                    Action::ToolAnswer(permission) => {
                        if let Some(req) = app.take_tool_approval() {
                            let note = tool_answer_note(&req, &permission);
                            agent.handle_confirmation(req.id, PermissionConfirmation {
                                principal_type: PrincipalType::Tool,
                                permission,
                            }).await;
                            app.push_system(&note);
                        }
                    }
                    // `App` already switched its own badge — this applies the
                    // switch to the running session, and only then persists it
                    // globally. `Auto` stays session-scoped (yolo mirror): a
                    // ramp to full autonomy must not outlive the session that
                    // asked for it.
                    Action::Mode(mode) => {
                        if let Err(e) = agent.update_kaji_mode(mode, session_id).await {
                            app.kaji_mode = agent.kaji_mode().await;
                            app.push_system(&format!("mode non appliqué : {e}"));
                        } else if matches!(mode, KajiMode::Approve | KajiMode::SmartApprove) {
                            if let Err(e) = Config::global().set_kaji_mode(mode) {
                                app.push_system(&format!("mode appliqué mais non enregistré : {e}"));
                            }
                        }
                    }
                    Action::Help => push_welcome(&mut app, true),
                    Action::Theme(name) => {
                        if let Err(e) = Config::global().set_param("KAJI_THEME", &name) {
                            app.push_system(&format!("thème appliqué mais non enregistré : {e}"));
                        }
                    }
                    Action::Cost => {
                        let report = cost_report(&session_manager, session_id).await;
                        app.push_system_lines(report);
                    }
                    Action::Context => {
                        let report = context_report_lines(agent, &session_manager, session_id).await;
                        app.push_system_lines(report);
                    }
                    Action::Docker => {
                        let report = docker_report();
                        app.push_system_lines(report);
                    }
                    Action::Checkpoints => match &disabled_checkpoints {
                        // Sans store, le journal ne contient aucun checkpoint :
                        // « aucun checkpoint » se lirait comme un historique
                        // vide plutôt que comme une fonctionnalité coupée.
                        Some(line) => app.push_system(line),
                        None => match session_manager.events_for_session(session_id).await {
                            Ok(events) => {
                                let lines = checkpoints_lines(&events);
                                if lines.is_empty() {
                                    app.push_system("aucun checkpoint");
                                } else {
                                    app.push_system_lines(lines);
                                }
                            }
                            Err(e) => app.push_system(&format!("erreur /checkpoints : {e}")),
                        },
                    },
                    // Only opens the confirmation modal — spec §3: "jamais
                    // automatique", the actual store/session mutation only
                    // ever runs from `Action::RestoreConfirm` below.
                    Action::Restore(id) => {
                        if let Some(line) = &disabled_checkpoints {
                            // Ouvrir la modale mènerait à un refus après
                            // confirmation, pour un id qui n'a jamais pu exister.
                            app.push_system(line);
                        } else {
                            let is_net =
                                match session_manager.events_for_session(session_id).await {
                                    Ok(events) => is_pre_restore_checkpoint(&events, &id),
                                    Err(e) => {
                                        app.push_system(&format!("erreur /restore : {e}"));
                                        false
                                    }
                                };
                            app.open_restore_confirm(id, is_net);
                        }
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
                // @-mention index (item 4 ante): the walk runs on a blocking
                // task, never here — this select! also owns `terminal.draw`
                // and the running turn's stream, so a project-sized walk
                // inline would freeze the screen mid-keystroke.
                if let Some(root) = app.take_mention_index_request() {
                    let tx = index_tx.clone();
                    tokio::task::spawn_blocking(move || {
                        let _ = tx.blocking_send(mentions::MentionIndex::build(root));
                    });
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
                if !started {
                    if app.driver != PassDriver::Idle {
                        app.pass_abort("échec du démarrage du tour — passe interrompue");
                    }
                    app.goal_abort("目標 échec du démarrage du tour — but interrompu");
                }
            }
            item = next_turn_event(&mut turn), if turn.is_some() => {
                match item {
                    Some(Ok(ev)) => app.apply_agent_event(&ev),
                    Some(Err(e)) => {
                        app.push_error(&format!("{e}"));
                        turn = None;
                        cancel = None;
                        teardown_turn(&mut app);
                        if app.driver != PassDriver::Idle {
                            app.pass_abort("erreur pendant la passe — passe interrompue");
                        }
                        app.goal_abort("目標 erreur pendant le tour — but interrompu");
                    }
                    None => {
                        turn = None;
                        cancel = None;
                        teardown_turn(&mut app);
                        if let Some(prompt) = app.turn_end() {
                            pending = Some(begin_setup(&mut app, agent, &session_config, &prompt, &mut cancel));
                        } else if flush_steer_queue(
                            &mut app,
                            agent,
                            &session_config,
                            &mut pending,
                            &mut cancel,
                            &working_dir,
                        ) {
                            // Queued steering message auto-submitted as a new
                            // turn (item 2 ante "do nothing — queued messages
                            // are submitted automatically once the turn ends").
                        } else {
                            app.suggestion_loading = true;
                            suggest_next_prompt(agent, session_id, &conversation, &suggestion_tx).await;
                        }
                    }
                }
            }
            // Next-prompt ghost (item 7): the generation task resolves here,
            // off the input/turn critical path. A stale suggestion that
            // outlived an input edit is cleared by `exit_history_navigation`,
            // so a fresh turn can't be polluted — the draw reads `app.state`.
            maybe = suggestion_rx.recv(), if !app.turn_pending && !app.turn_active => {
                if let Some(text) = maybe {
                    app.suggestion = Some(text);
                    app.suggestion_loading = false;
                } else {
                    app.suggestion_loading = false;
                }
            }
            // Freshly built index snapshot: swaps in and re-runs the current
            // fragment, so a completion typed before the walk finished lights
            // up on arrival instead of waiting for the next keystroke.
            maybe = index_rx.recv() => {
                if let Some(index) = maybe {
                    app.on_mention_index_ready(index);
                }
            }
            maybe = git_rx.recv() => {
                if let Some(status) = maybe {
                    app.on_git_status(status);
                }
            }
        }
        // Status bar (task 15): same hand-off as the @-mention index, drained
        // here rather than in the input arm because the tick and the end of a
        // turn arm it too.
        if let Some(dir) = app.take_git_refresh_request() {
            let tx = git_tx.clone();
            tokio::task::spawn_blocking(move || {
                let _ = tx.blocking_send(gitstatus::read(&dir));
            });
        }
        // Single drain point for the goal event log: every arm above can end
        // a goal (a verdict, Esc, a stream error), and journaling from here
        // keeps the write off the keystroke path in all of them.
        persist_goal_events(&session_manager, session_id, &mut app).await;
    }
    Ok(())
}

/// The first work prompt carries the condition verbatim, so an `@path` typed
/// in `/goal` must expand exactly as it does in a submitted message. Only this
/// first prompt: the turns that follow share the same session history, and
/// re-expanding the condition every iteration would attach the same file over
/// and over. The goal keeps the raw condition — it is what `/goal` reports and
/// what the evaluator is asked about.
fn goal_work_prompt(app: &mut App, condition: &str, working_dir: &Path) -> Option<String> {
    let prompt = app.goal_set(condition, goal_max_iterations())?;
    Some(mentions::expand_mentions(&prompt, working_dir))
}

/// `KAJI_GOAL_MAX_ITERATIONS` — backstop of the unsupervised loop, distinct
/// from the session's `max_turns`. Absent or unusable values fall back to the
/// core default rather than disarming it.
fn goal_max_iterations() -> usize {
    kaji_core::goal::max_iterations(std::env::var("KAJI_GOAL_MAX_ITERATIONS").ok().as_deref())
}

/// Journals the goal events `App` queued, on the turn they belong to: the
/// event log's last `turn_seq`, as `journal_pre_restore` does for a
/// checkpoint. A failed append never interrupts the session.
async fn persist_goal_events(session_manager: &SessionManager, session_id: &str, app: &mut App) {
    let events = app.take_goal_events();
    if events.is_empty() {
        return;
    }
    let turn_seq = session_manager
        .next_turn_seq(session_id)
        .await
        .map(|seq| seq - 1)
        .unwrap_or(0);
    for (kind, payload) in events {
        if let Err(error) = session_manager
            .append_event(session_id, turn_seq, kind, &payload)
            .await
        {
            tracing::warn!(?error, "event log append failed for goal event");
        }
    }
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
/// Clears the turn clock/token bookkeeping (which also arms a git read for the
/// status bar) and closes any tool line still awaiting its response (Esc
/// mid-tool-call, or the stream ending with a tool pending) so the 工 spinner
/// doesn't stay frozen forever. `close_orphaned_tool_requests` only touches
/// still-pending entries, so calling it here is safe even when every tool
/// already completed normally.
fn teardown_turn(app: &mut App) {
    app.finish_turn();
    app.close_orphaned_tool_requests();
}

/// Submits the next queued steering message as a fresh turn (item 2 ante).
/// Returns `true` when a message was consumed and a setup future armed, so
/// the caller knows the queue is non-empty at this point. Used both by the
/// turn-end auto-flush and by `Ctrl+S` when no stream is running to drain.
fn flush_steer_queue<'a>(
    app: &mut App,
    agent: &'a Agent,
    session_config: &SessionConfig,
    pending: &mut Option<Pin<Box<dyn Future<Output = anyhow::Result<TurnStream<'a>>> + 'a>>>,
    cancel: &mut Option<CancellationToken>,
    working_dir: &Path,
) -> bool {
    let Some(text) = app.next_steer() else {
        return false;
    };
    app.push_user(&text);
    let expanded = mentions::expand_mentions(&text, working_dir);
    *pending = Some(begin_setup(app, agent, session_config, &expanded, cancel));
    true
}

/// Next-prompt ghost (item 7): spawns a best-effort, off-critical-path task
/// that asks the active provider for a short "what to do next" suggestion
/// after a turn ends cleanly, delivering it over `suggestion_tx`. Strictly
/// optional: a missing provider, a slow call, or a generation error silently
/// produce no ghost (the channel just never yields). Uses a small window of
/// the recent user-visible conversation as context, never the whole transcript.
async fn suggest_next_prompt(
    agent: &Agent,
    session_id: &str,
    conversation: &kaji::conversation::Conversation,
    suggestion_tx: &mpsc::Sender<String>,
) {
    // Resolve these here — `agent` borrows the event loop, so nothing tied to
    // its lifetime can move into the spawned task. The Arc clone and model
    // config are 'static, rendezvous the network call to the task below.
    let Ok(provider) = agent.provider().await else {
        return;
    };
    let Ok(model_config) = agent.model_config_for_session(session_id).await else {
        return;
    };
    let context = conversation
        .messages()
        .iter()
        .filter(|m| m.is_user_visible())
        .rev()
        .take(4)
        .map(|m| m.as_concat_text())
        .collect::<Vec<_>>()
        .join("\n\n");
    let tx = suggestion_tx.clone();
    tokio::task::spawn(async move {
        let system = "Tu es kaji, un agent de terminal. Après cet échange, propose un seul prochain prompt (une phrase, action concrète, sans préfixe ni citation) que l'utilisateur voudrait probablement envoyer.";
        let messages = vec![Message::user().with_text(if context.trim().is_empty() {
            "L'échange est vide — suggère un point de départ pour une nouvelle session."
        } else {
            context.trim()
        })];
        if let Ok(Ok((response, _))) = tokio::time::timeout(
            Duration::from_secs(10),
            provider.complete(&model_config, system, &messages, &[]),
        )
        .await
        {
            let text = response
                .user_visible_content()
                .as_concat_text()
                .trim()
                .to_string();
            if !text.is_empty() {
                let _ = tx.send(text).await;
            }
        }
    });
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

/// A goal session whose last journaled event isn't `goal_end` was cut short
/// (crash, kill, closed terminal). The resume says so and stops there: a loop
/// that relaunches itself on its own after a crash is exactly what the
/// iteration cap exists to prevent.
fn interrupted_goal_line(events: &[SessionEvent]) -> Option<String> {
    let last = events
        .iter()
        .rfind(|event| event.kind.starts_with("goal_"))?;
    if last.kind == "goal_end" {
        return None;
    }
    let payload: serde_json::Value = serde_json::from_str(&last.payload_json).ok()?;
    let condition = goal_condition(events)?;
    let iteration = payload
        .get("iteration")
        .and_then(|v| v.as_i64())
        .unwrap_or(1);
    let max_iterations = goal_start_payload(events)?
        .get("max_iterations")
        .and_then(|v| v.as_i64())
        .unwrap_or(kaji_core::goal::DEFAULT_MAX_ITERATIONS as i64);
    Some(format!(
        "⚠ goal interrompu : {condition} (it {iteration}/{max_iterations}) — `/goal <condition>` pour relancer"
    ))
}

fn goal_start_payload(events: &[SessionEvent]) -> Option<serde_json::Value> {
    let last_start = events.iter().rfind(|event| event.kind == "goal_start")?;
    serde_json::from_str(&last_start.payload_json).ok()
}

fn goal_condition(events: &[SessionEvent]) -> Option<String> {
    goal_start_payload(events)?
        .get("condition")
        .and_then(|v| v.as_str())
        .map(|c| crate::tui::ui::sanitize_for_display(&c.replace('\n', "␊")))
}

/// Same contract as `apply_interrupted_turn_marker`: a read error is logged,
/// never turned into a blocked resume.
async fn apply_interrupted_goal_marker(
    app: &mut App,
    session_manager: &SessionManager,
    session_id: &str,
) {
    match session_manager.events_for_session(session_id).await {
        Ok(events) => {
            if let Some(line) = interrupted_goal_line(&events) {
                app.push_system(&line);
            }
        }
        Err(e) => tracing::warn!("échec de la détection de goal interrompu au resume: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use app::Sender;
    use kaji::conversation::Conversation;

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

    fn line_text(line: &RoledLine) -> String {
        line.iter().map(|span| span.text.as_str()).collect()
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

    /// La ligne sert au boot, à `/checkpoints` et à `/restore` : muette dès
    /// qu'un store existe, et muette aussi sans raison (un store absent sans
    /// refus explicite n'a rien à annoncer).
    #[test]
    fn checkpoints_disabled_line_speaks_only_for_an_explicit_refusal() {
        assert_eq!(
            checkpoints_disabled_line(false, Some("hors dépôt git")).as_deref(),
            Some("checkpoints désactivés — hors dépôt git")
        );
        assert_eq!(checkpoints_disabled_line(false, None), None);
        assert_eq!(
            checkpoints_disabled_line(true, Some("répertoire home")),
            None
        );
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

        assert!(
            !app.chat
                .iter()
                .any(|l| matches!(l.sender, Sender::Thinking))
        );
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

        assert!(
            app.chat
                .iter()
                .any(|l| l.text.contains('✓') && l.text.contains("shell"))
        );
        assert!(!app.chat.iter().any(|l| l.tool.is_some()));
    }

    #[test]
    fn seed_chat_closes_unmatched_tool_request_as_interrupted() {
        let mut app = App::new(None);
        let conversation = Conversation::new_unvalidated([Message::assistant()
            .with_tool_request("t1", Ok(rmcp::model::CallToolRequestParams::new("shell")))]);

        seed_chat(&mut app, &conversation);

        assert!(!app.chat.iter().any(|l| l.tool.is_some()));
        assert!(
            app.chat
                .iter()
                .any(|l| l.text.contains("interrompu") && l.text.contains("shell"))
        );
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
    /// stream ends with a tool pending), the 工 spinner must not stay frozen
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

        assert!(
            app.chat
                .iter()
                .any(|l| l.sender == Sender::System && l.text.contains("tour interrompu"))
        );
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

    /// ⛔ BARRIÈRE — un goal coupé net (crash, kill) est signalé au resume,
    /// jamais relancé tout seul.
    #[tokio::test]
    async fn resume_warns_about_a_goal_left_without_its_end_event() {
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
        sm.append_event(
            &sid,
            1,
            "goal_start",
            r#"{"condition":"les tests passent","max_iterations":10}"#,
        )
        .await
        .unwrap();
        sm.append_event(
            &sid,
            1,
            "goal_iteration",
            r#"{"iteration":2,"verdict":"continue","feedback":"il reste X"}"#,
        )
        .await
        .unwrap();

        let mut app = App::new(None);
        apply_interrupted_goal_marker(&mut app, &sm, &sid).await;

        let line = app
            .chat
            .iter()
            .find(|l| l.text.contains("goal interrompu"))
            .expect("une ligne d'avertissement");
        assert!(line.text.contains("les tests passent"), "{}", line.text);
        assert!(line.text.contains("2/10"), "{}", line.text);
        assert!(line.text.contains("/goal <condition>"), "{}", line.text);
    }

    #[tokio::test]
    async fn resume_stays_silent_when_the_goal_ended_cleanly() {
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
        sm.append_event(
            &sid,
            1,
            "goal_start",
            r#"{"condition":"c","max_iterations":10}"#,
        )
        .await
        .unwrap();
        sm.append_event(&sid, 1, "goal_end", r#"{"outcome":"met","iteration":1}"#)
            .await
            .unwrap();

        let mut app = App::new(None);
        apply_interrupted_goal_marker(&mut app, &sm, &sid).await;

        assert!(!app.chat.iter().any(|l| l.text.contains("goal interrompu")));
    }

    #[tokio::test]
    async fn goal_events_are_journaled_on_the_current_turn() {
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
        sm.append_event(&sid, 1, "turn_start", "{}").await.unwrap();

        let mut app = App::new(None);
        app.goal_set("les tests passent", 4);
        persist_goal_events(&sm, &sid, &mut app).await;

        let events = sm.events_for_session(&sid).await.unwrap();
        let start = events
            .iter()
            .find(|e| e.kind == "goal_start")
            .expect("un goal_start journalisé");
        assert_eq!(start.turn_seq, 1, "le tour courant, pas le suivant");
        assert!(start.payload_json.contains("les tests passent"));
        assert!(app.take_goal_events().is_empty(), "la file est drainée");
    }

    #[test]
    fn goal_max_iterations_reads_the_env_and_falls_back_on_the_default() {
        {
            let _guard = env_lock::lock_env([("KAJI_GOAL_MAX_ITERATIONS", Some("3"))]);
            assert_eq!(goal_max_iterations(), 3);
        }
        let _guard = env_lock::lock_env([("KAJI_GOAL_MAX_ITERATIONS", None::<&str>)]);
        assert_eq!(
            goal_max_iterations(),
            kaji_core::goal::DEFAULT_MAX_ITERATIONS
        );
    }

    #[test]
    fn apply_interrupted_turn_marker_stays_silent_on_read_error() {
        let mut app = App::new(None);

        apply_interrupted_turn_marker(&mut app, Err(anyhow::anyhow!("boom")));

        assert!(app.chat.is_empty());
    }

    /// `seed_chat` ne stocke plus qu'un rôle : le thème actif au replay ne
    /// fige rien, la couleur est résolue au draw (`ui::push_rendered_lines`).
    #[test]
    fn a_replayed_error_line_keeps_its_role_whatever_the_theme_at_seed_time() {
        let _theme = theme::test_guard();

        theme::set_active("zen").unwrap();
        let zen = seeded_error_role();
        theme::set_active("nord").unwrap();
        let nord = seeded_error_role();

        assert_eq!(zen, Some(SpanRole::Error));
        assert_eq!(zen, nord);
    }

    fn seeded_error_role() -> Option<SpanRole> {
        use kaji::conversation::message::MessageErrorKind;

        let mut app = App::new(None);
        let conversation = Conversation::new_unvalidated([
            Message::assistant().with_error(MessageErrorKind::Other, "boom")
        ]);
        seed_chat(&mut app, &conversation);
        app.chat
            .iter()
            .find_map(|line| Some(line.rendered.as_ref()?.first()?.first()?.role))
    }

    #[tokio::test]
    async fn the_working_dir_comes_from_the_session_not_the_process_cwd() {
        use kaji::config::KajiMode;
        use kaji::session::SessionType;

        let temp_dir = tempfile::tempdir().unwrap();
        let project = temp_dir.path().join("projet");
        std::fs::create_dir_all(&project).unwrap();
        let sm = SessionManager::new(temp_dir.path().to_path_buf());
        let sid = sm
            .create_session(
                project.clone(),
                "s".to_string(),
                SessionType::User,
                KajiMode::default(),
            )
            .await
            .unwrap()
            .id;

        assert_eq!(session_working_dir(&sm, &sid).await, project);
    }

    /// `/goal corriger @notes.md` must reach the agent with the file attached,
    /// exactly like the same text submitted as a message — while the goal
    /// itself keeps the raw condition it reports and evaluates.
    #[test]
    fn goal_expands_the_mentions_of_its_condition_into_the_first_work_prompt() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("notes.md"), "corps du fichier").unwrap();
        let mut app = App::new(None);

        let prompt = goal_work_prompt(&mut app, "corriger @notes.md", dir.path())
            .expect("prompt de travail");

        assert!(prompt.contains("corps du fichier"), "{prompt}");
        assert_eq!(
            app.goal.as_ref().expect("un but").condition,
            "corriger @notes.md"
        );
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
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

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
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

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
            ("/sdd", "/goal"),
            ("/goal", "/files"),
            ("/files", "/explorer"),
            ("/explorer", "/spec"),
            ("/spec", "/think"),
            ("/think", "/cost"),
            ("/cost", "/context"),
            ("/context", "/docker"),
            ("/docker", "/checkpoints"),
            ("/checkpoints", "/theme"),
            ("/theme", "/help"),
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
        let _theme = theme::test_guard();
        let mut app = App::new(None);

        push_welcome(&mut app, true);

        assert_eq!(
            welcome_line_fg(&app, "bienvenue"),
            theme::text_color(),
            "/help must render like a normal answer, not the dim welcome ambiance"
        );
    }

    #[test]
    fn startup_welcome_stays_dim() {
        let _theme = theme::test_guard();
        let mut app = App::new(None);

        push_welcome(&mut app, false);

        assert_eq!(
            welcome_line_fg(&app, "bienvenue"),
            ratatui::style::Color::DarkGray,
            "the startup banner must keep its dim ambiance style"
        );
    }
}
