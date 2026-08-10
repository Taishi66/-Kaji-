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
use kaji_core::sdd::SpecDoc;
use ratatui::crossterm::event::{self, Event};
use std::path::PathBuf;
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
    app.push_system("PageUp/PageDown/Home/End font défiler le chat");
    app.push_system("Esc interrompt · Ctrl+C quitte");
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
    seed_chat(&mut app, &conversation);
    push_welcome(&mut app);
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
                        if app.driver != PassDriver::Idle {
                            app.pass_abort("erreur pendant la passe — passe interrompue");
                        }
                    }
                    None => {
                        turn = None;
                        cancel = None;
                        app.finish_turn();
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
