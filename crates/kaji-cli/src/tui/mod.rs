pub mod app;
pub mod ui;

use anyhow::Result;
use app::{Action, App, PassDriver};
use futures::stream::BoxStream;
use futures::StreamExt;
use kaji::agents::{Agent, AgentEvent, SessionConfig};
use kaji::conversation::message::{Message, MessageContentBlock};
use kaji_core::sdd::SpecDoc;
use ratatui::crossterm::event::{self, Event};
use rmcp::model::Role;
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
    let (input_tx, input_rx) = mpsc::channel::<Event>(64);
    std::thread::spawn(move || input_thread(input_tx));
    let mut terminal = ratatui::init();
    let result = event_loop(
        &mut terminal,
        &agent,
        &session_id,
        conversation,
        spec_path,
        input_rx,
    )
    .await;
    ratatui::restore();
    result
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

async fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    agent: &Agent,
    session_id: &str,
    conversation: kaji::conversation::Conversation,
    spec_path: Option<PathBuf>,
    mut input_rx: mpsc::Receiver<Event>,
) -> Result<()> {
    let mut app = App::new(resolve_spec(spec_path));
    seed_chat(&mut app, &conversation);
    let session_config = SessionConfig {
        id: session_id.to_string(),
        schedule_id: None,
        max_turns: None,
        retry_config: None,
    };
    let mut turn: Option<TurnStream<'_>> = None;
    let mut cancel: Option<CancellationToken> = None;

    loop {
        terminal.draw(|frame| ui::draw(frame, &app))?;
        tokio::select! {
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
                        app.turn_active = false;
                        if app.driver != PassDriver::Idle {
                            app.pass_abort("erreur pendant la passe — passe interrompue");
                        }
                    }
                    None => {
                        turn = None;
                        cancel = None;
                        app.turn_active = false;
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
            app.turn_active = true;
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
        if message.role == Role::User {
            for block in &message.content {
                if let MessageContentBlock::Text(text) = block {
                    app.push_user(&text.text);
                }
            }
        } else {
            app.apply_agent_event(&AgentEvent::Message(message.clone()));
        }
    }
}
