use kaji::agents::AgentEvent;
use kaji::conversation::message::{Message, MessageContentBlock};
use kaji_core::sdd::{SddPass, SpecDoc};
use ratatui::crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use rmcp::model::Role;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PassDriver {
    Idle,
    AwaitingGate,
    Executing,
    Validating,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sender {
    User,
    Agent,
    System,
}

#[derive(Debug, Clone)]
pub struct ChatLine {
    pub sender: Sender,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    None,
    Submit(String),
    CancelTurn,
    Quit,
    StartPass,
    GateApprove,
    GateReject,
}

pub struct App {
    pub input: String,
    pub chat: Vec<ChatLine>,
    pub status: String,
    pub turn_active: bool,
    pub spec: Option<SpecDoc>,
    pub pass: SddPass,
    pub gate_open: bool,
    pub driver: PassDriver,
    validate_buffer: String,
    last_agent_msg_id: Option<String>,
}

impl App {
    pub fn new(spec: Option<SpecDoc>) -> Self {
        Self {
            input: String::new(),
            chat: Vec::new(),
            status: String::new(),
            turn_active: false,
            spec,
            pass: SddPass::new(),
            gate_open: false,
            driver: PassDriver::Idle,
            validate_buffer: String::new(),
            last_agent_msg_id: None,
        }
    }

    pub fn start_pass(&mut self) {
        let Some(spec) = self.spec.as_ref() else {
            self.push_system("aucune SPEC chargée — /sdd nécessite un fichier SPEC.md");
            return;
        };
        if spec.is_empty() {
            self.push_system("SPEC vide — rien à exécuter");
            return;
        }
        if self.pass.is_running() {
            self.push_system("passe déjà en cours");
            return;
        }
        if self.pass.is_complete() || self.pass.drifted() {
            self.pass = SddPass::new();
        }
        let title = spec.title.clone();
        self.pass.start();
        self.push_system(&format!("Intent : {title}"));
        self.pass.advance();
        self.pass.advance();
        self.gate_open = true;
        self.driver = PassDriver::AwaitingGate;
    }

    pub fn gate_approve(&mut self) -> Option<String> {
        let body = self.spec.as_ref()?.body.clone();
        self.gate_open = false;
        self.pass.advance();
        self.driver = PassDriver::Executing;
        Some(format!(
            "Exécute la SPEC suivante. Réponds directement, sans sortir du périmètre.\n\n{body}"
        ))
    }

    pub fn pass_abort(&mut self, reason: &str) {
        if self.pass.is_running() {
            self.pass.fail_current();
        }
        self.driver = PassDriver::Idle;
        self.gate_open = false;
        self.validate_buffer.clear();
        self.push_system(reason);
    }

    pub fn gate_reject(&mut self) {
        self.gate_open = false;
        self.pass.fail_current();
        self.driver = PassDriver::Idle;
        self.push_system("gate refusée — passe interrompue");
    }

    pub fn turn_end(&mut self) -> Option<String> {
        self.turn_active = false;
        match self.driver {
            PassDriver::Executing => {
                let body = self.spec.as_ref()?.body.clone();
                self.pass.advance();
                self.driver = PassDriver::Validating;
                self.validate_buffer.clear();
                Some(format!(
                    "Vérifie que ta réponse précédente respecte la SPEC ci-dessous. Première ligne : exactement `VERDICT: VALIDE` ou `VERDICT: DRIFT`, puis justifie en une phrase.\n\n{body}"
                ))
            }
            PassDriver::Validating => {
                self.pass.advance();
                if self
                    .validate_buffer
                    .to_uppercase()
                    .contains("VERDICT: DRIFT")
                {
                    self.pass.fail_current();
                    self.push_system("⚠ drift détecté — spec non verrouillée");
                } else {
                    self.pass.advance();
                    self.push_system("✓ passe SDD complète — spec verrouillée");
                }
                self.driver = PassDriver::Idle;
                None
            }
            _ => None,
        }
    }

    pub fn on_event(&mut self, ev: &Event) -> Action {
        let Event::Key(key) = ev else {
            return Action::None;
        };
        if key.kind != KeyEventKind::Press {
            return Action::None;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Action::Quit;
        }
        if self.gate_open {
            return match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.gate_open = false;
                    Action::GateApprove
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    self.gate_open = false;
                    Action::GateReject
                }
                _ => Action::None,
            };
        }
        match key.code {
            KeyCode::Char(c) => {
                self.input.push(c);
                Action::None
            }
            KeyCode::Backspace => {
                self.input.pop();
                Action::None
            }
            KeyCode::Esc if self.turn_active => Action::CancelTurn,
            KeyCode::Enter if self.turn_active => {
                self.push_system("tour en cours — Esc pour annuler d'abord");
                Action::None
            }
            KeyCode::Enter => {
                let text = std::mem::take(&mut self.input);
                let text = text.trim().to_string();
                if text.is_empty() {
                    Action::None
                } else if text == "/sdd" {
                    Action::StartPass
                } else if text == "/quit" {
                    Action::Quit
                } else {
                    Action::Submit(text)
                }
            }
            _ => Action::None,
        }
    }

    pub fn push_user(&mut self, text: &str) {
        self.chat.push(ChatLine {
            sender: Sender::User,
            text: text.to_string(),
        });
        self.last_agent_msg_id = None;
    }

    pub fn push_system(&mut self, text: &str) {
        self.chat.push(ChatLine {
            sender: Sender::System,
            text: text.to_string(),
        });
        self.last_agent_msg_id = None;
    }

    pub fn apply_agent_event(&mut self, ev: &AgentEvent) {
        match ev {
            AgentEvent::Message(message) => self.apply_message(message),
            AgentEvent::HistoryReplaced(_) => self.push_system("— historique compacté —"),
            _ => {}
        }
    }

    fn apply_message(&mut self, message: &Message) {
        if message.role != Role::Assistant {
            return;
        }
        for block in &message.content {
            match block {
                MessageContentBlock::Text(text) => {
                    self.merge_agent_text(&message.id, &text.text);
                    if self.driver == PassDriver::Validating {
                        self.validate_buffer.push_str(&text.text);
                    }
                }
                MessageContentBlock::ToolRequest(req) => {
                    let name = req
                        .tool_call
                        .as_ref()
                        .map(|call| call.name.as_ref())
                        .unwrap_or("outil");
                    self.push_system(&format!("⚙ {name}"));
                }
                MessageContentBlock::ToolResponse(_) => self.push_system("✓ outil terminé"),
                _ => {}
            }
        }
    }

    fn merge_agent_text(&mut self, message_id: &Option<String>, text: &str) {
        if message_id.is_some() && *message_id == self.last_agent_msg_id {
            if let Some(last) = self.chat.last_mut() {
                last.text.push_str(text);
                return;
            }
        }
        self.chat.push(ChatLine {
            sender: Sender::Agent,
            text: text.to_string(),
        });
        self.last_agent_msg_id = message_id.clone();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaji::conversation::message::Message;
    use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: ratatui::crossterm::event::KeyEventState::NONE,
        })
    }

    #[test]
    fn typing_fills_input_and_enter_submits() {
        let mut app = App::new(None);
        app.on_event(&key(KeyCode::Char('h')));
        app.on_event(&key(KeyCode::Char('i')));
        assert_eq!(app.input, "hi");
        let action = app.on_event(&key(KeyCode::Enter));
        assert_eq!(action, Action::Submit("hi".to_string()));
        assert!(app.input.is_empty());
    }

    #[test]
    fn backspace_edits_and_empty_enter_is_noop() {
        let mut app = App::new(None);
        app.on_event(&key(KeyCode::Char('a')));
        app.on_event(&key(KeyCode::Backspace));
        assert_eq!(app.on_event(&key(KeyCode::Enter)), Action::None);
    }

    #[test]
    fn slash_sdd_submits_start_pass() {
        let mut app = App::new(None);
        for c in "/sdd".chars() {
            app.on_event(&key(KeyCode::Char(c)));
        }
        assert_eq!(app.on_event(&key(KeyCode::Enter)), Action::StartPass);
    }

    #[test]
    fn esc_cancels_running_turn_and_ctrl_c_quits() {
        let mut app = App::new(None);
        app.turn_active = true;
        assert_eq!(app.on_event(&key(KeyCode::Esc)), Action::CancelTurn);
        let ctrl_c = Event::Key(KeyEvent {
            code: KeyCode::Char('c'),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: ratatui::crossterm::event::KeyEventState::NONE,
        });
        assert_eq!(app.on_event(&ctrl_c), Action::Quit);
    }

    #[test]
    fn enter_during_active_turn_does_not_submit() {
        let mut app = App::new(None);
        app.turn_active = true;
        app.on_event(&key(KeyCode::Char('h')));
        app.on_event(&key(KeyCode::Char('i')));
        let action = app.on_event(&key(KeyCode::Enter));
        assert_eq!(action, Action::None);
        assert_eq!(app.input, "hi");
        assert!(app
            .chat
            .iter()
            .any(|l| l.text.contains("tour en cours") && l.text.contains("Esc")));
    }

    #[test]
    fn gate_mode_maps_y_and_n() {
        let mut app = App::new(None);
        app.gate_open = true;
        assert_eq!(app.on_event(&key(KeyCode::Char('y'))), Action::GateApprove);
        app.gate_open = true;
        assert_eq!(app.on_event(&key(KeyCode::Char('n'))), Action::GateReject);
    }

    #[test]
    fn agent_text_chunks_with_same_id_merge_into_one_line() {
        let mut app = App::new(None);
        let mut m1 = Message::assistant().with_text("Bon");
        m1.id = Some("msg-1".to_string());
        let mut m2 = Message::assistant().with_text("jour");
        m2.id = Some("msg-1".to_string());
        app.apply_agent_event(&kaji::agents::AgentEvent::Message(m1));
        app.apply_agent_event(&kaji::agents::AgentEvent::Message(m2));
        let agent_lines: Vec<_> = app
            .chat
            .iter()
            .filter(|l| matches!(l.sender, Sender::Agent))
            .collect();
        assert_eq!(agent_lines.len(), 1);
        assert_eq!(agent_lines[0].text, "Bonjour");
    }

    #[test]
    fn history_replaced_adds_system_notice() {
        let mut app = App::new(None);
        app.apply_agent_event(&kaji::agents::AgentEvent::HistoryReplaced(
            kaji::conversation::Conversation::default(),
        ));
        assert!(app.chat.iter().any(|l| l.text.contains("compact")));
    }

    fn spec() -> SpecDoc {
        SpecDoc::parse(std::path::PathBuf::from("SPEC.md"), "# Demo\nfaire X")
    }

    fn agent_says(app: &mut App, id: &str, text: &str) {
        let mut m = Message::assistant().with_text(text);
        m.id = Some(id.to_string());
        app.apply_agent_event(&kaji::agents::AgentEvent::Message(m));
    }

    #[test]
    fn start_pass_without_spec_reports_error() {
        let mut app = App::new(None);
        app.start_pass();
        assert!(!app.pass.is_running());
        assert!(app.chat.iter().any(|l| matches!(l.sender, Sender::System)));
    }

    #[test]
    fn happy_path_valide_locks_the_spec() {
        let mut app = App::new(Some(spec()));
        app.start_pass();
        assert!(app.gate_open);
        assert_eq!(app.pass.current(), Some(kaji_core::sdd::SddStage::Gate));

        let exec_prompt = app.gate_approve().expect("prompt exec");
        assert!(exec_prompt.contains("faire X"));
        app.turn_active = true;
        agent_says(&mut app, "m1", "c'est fait");
        let validate_prompt = app.turn_end().expect("prompt validate");
        assert!(validate_prompt.contains("VERDICT"));

        app.turn_active = true;
        agent_says(&mut app, "m2", "VERDICT: VALIDE — conforme");
        assert!(app.turn_end().is_none());
        assert!(app.pass.is_complete());
        assert!(!app.pass.drifted());
    }

    #[test]
    fn drift_verdict_fails_drift_lock() {
        let mut app = App::new(Some(spec()));
        app.start_pass();
        app.gate_approve();
        agent_says(&mut app, "m1", "fait autre chose");
        app.turn_end();
        agent_says(&mut app, "m2", "VERDICT: DRIFT — hors spec");
        app.turn_end();
        assert!(app.pass.drifted());
    }

    #[test]
    fn gate_reject_aborts_the_pass() {
        let mut app = App::new(Some(spec()));
        app.start_pass();
        app.gate_reject();
        assert!(app.pass.drifted());
        assert!(!app.pass.is_running());
    }

    #[test]
    fn restart_after_terminated_pass_resets_stages() {
        let mut app = App::new(Some(spec()));
        app.start_pass();
        app.gate_approve();
        agent_says(&mut app, "m1", "fait autre chose");
        app.turn_end();
        agent_says(&mut app, "m2", "VERDICT: DRIFT — hors spec");
        app.turn_end();
        assert!(app.pass.drifted());

        app.start_pass();
        assert!(app.gate_open);
        assert_eq!(app.pass.current(), Some(kaji_core::sdd::SddStage::Gate));
        assert!(!app
            .pass
            .stages()
            .iter()
            .any(|(_, status)| *status == kaji_core::sdd::StageStatus::Failed));
    }

    #[test]
    fn pass_abort_from_executing_resets_driver_and_fails_pass() {
        let mut app = App::new(Some(spec()));
        app.start_pass();
        app.gate_approve();
        assert_eq!(app.driver, PassDriver::Executing);

        app.pass_abort("échec du démarrage du tour — passe interrompue");

        assert_eq!(app.driver, PassDriver::Idle);
        assert!(app.pass.drifted());
        assert!(app.validate_buffer.is_empty());
        assert!(app
            .chat
            .iter()
            .any(|l| l.text.contains("échec du démarrage")));
    }

    #[test]
    fn esc_during_pass_aborts_it() {
        let mut app = App::new(Some(spec()));
        app.start_pass();
        app.gate_approve();
        assert_eq!(app.driver, PassDriver::Executing);

        // Esc → Action::CancelTurn : la boucle annule le token puis appelle
        // pass_abort (driver != Idle). Le stream cancelled se termine ensuite
        // proprement (None), et la boucle appelle turn_end() sans effet.
        app.pass_abort("tour annulé — passe interrompue");
        assert!(app.turn_end().is_none());

        assert_eq!(app.driver, PassDriver::Idle);
        assert!(app.pass.drifted());
        assert!(!app.pass.is_complete());
    }

    #[test]
    fn stream_error_during_validating_aborts_pass() {
        let mut app = App::new(Some(spec()));
        app.start_pass();
        app.gate_approve();
        agent_says(&mut app, "m1", "c'est fait");
        app.turn_end();
        assert_eq!(app.driver, PassDriver::Validating);
        agent_says(&mut app, "m2", "début de verdict tronqué");

        // Some(Err(e)) mid-stream → la boucle appelle pass_abort (driver != Idle).
        app.pass_abort("erreur pendant la passe — passe interrompue");

        assert_eq!(app.driver, PassDriver::Idle);
        assert!(app.validate_buffer.is_empty());
        assert!(app.pass.drifted());
    }

    #[test]
    fn push_system_between_same_id_chunks_keeps_them_separate() {
        let mut app = App::new(None);
        agent_says(&mut app, "m1", "Bon");
        app.push_system("⚙ outil");
        agent_says(&mut app, "m1", "jour");

        let agent_lines: Vec<_> = app
            .chat
            .iter()
            .filter(|l| matches!(l.sender, Sender::Agent))
            .collect();
        assert_eq!(agent_lines.len(), 2);
        assert_eq!(agent_lines[0].text, "Bon");
        assert_eq!(agent_lines[1].text, "jour");
        assert!(app
            .chat
            .iter()
            .any(|l| matches!(l.sender, Sender::System) && l.text.contains("outil")));
    }
}
