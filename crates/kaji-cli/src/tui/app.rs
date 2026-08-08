use kaji::agents::AgentEvent;
use kaji::conversation::message::{Message, MessageContentBlock};
use kaji_core::sdd::{SddPass, SpecDoc};
use ratatui::crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use rmcp::model::Role;

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
            last_agent_msg_id: None,
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
                MessageContentBlock::Text(text) => self.merge_agent_text(&message.id, &text.text),
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
}
