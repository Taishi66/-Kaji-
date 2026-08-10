use kaji::agents::AgentEvent;
use kaji::conversation::message::{ActionRequiredData, Message, MessageContentBlock};
use kaji::providers::base::ProviderUsage;
use kaji_core::sdd::{SddPass, SpecDoc};
use ratatui::crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::text::Line;
use rmcp::model::Role;
use std::collections::HashMap;
use std::time::{Duration, Instant};

const SCROLL_PAGE: u16 = 10;

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
pub struct ToolLineState {
    pub name: String,
    pub started: Instant,
}

#[derive(Debug, Clone)]
pub struct ChatLine {
    pub sender: Sender,
    pub text: String,
    pub tool: Option<ToolLineState>,
    /// System lines only: pre-rendered, per-role-styled lines (aligned
    /// tables) that bypass the plain dim-italic style entirely — used for
    /// on-demand report blocks (`/cost`, `/docker`). `text` still carries
    /// the flattened plain-text equivalent for scroll-height accounting.
    pub rendered: Option<Vec<Line<'static>>>,
}

#[derive(Debug, Clone)]
pub struct ToolApprovalRequest {
    pub id: String,
    pub tool_name: String,
    pub prompt: Option<String>,
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
    ToolApprove,
    ToolDeny,
    Help,
    Cost,
    Docker,
}

pub struct App {
    pub header: String,
    pub input: String,
    pub chat: Vec<ChatLine>,
    pub status: String,
    pub turn_active: bool,
    /// True while the setup future (`Agent::reply` up to its first yield) is
    /// being polled by the event loop's `select!` but hasn't resolved into a
    /// `TurnStream` yet — distinct from `turn_active`, which only becomes
    /// true once the stream exists. Input stays live during this window
    /// (option B); the guards below treat it like an in-flight turn.
    pub turn_pending: bool,
    pub turn_started: Option<Instant>,
    pub tokens_turn_in: i64,
    pub tokens_turn_out: i64,
    pub tokens_total_in: i64,
    pub tokens_total_out: i64,
    pub cost_turn: Option<f64>,
    pub cost_total: Option<f64>,
    pub git_status: Option<String>,
    pub spec: Option<SpecDoc>,
    pub pass: SddPass,
    pub gate_open: bool,
    pub tool_approval: Option<ToolApprovalRequest>,
    pub driver: PassDriver,
    pub scroll_offset: u16,
    validate_buffer: String,
    last_agent_msg_id: Option<String>,
    pending_tools: HashMap<String, usize>,
    spec_panel_forced: Option<bool>,
}

impl App {
    pub fn new(spec: Option<SpecDoc>) -> Self {
        Self {
            header: String::new(),
            input: String::new(),
            chat: Vec::new(),
            status: String::new(),
            turn_active: false,
            turn_pending: false,
            turn_started: None,
            tokens_turn_in: 0,
            tokens_turn_out: 0,
            tokens_total_in: 0,
            tokens_total_out: 0,
            cost_turn: None,
            cost_total: None,
            git_status: None,
            spec,
            pass: SddPass::new(),
            gate_open: false,
            tool_approval: None,
            driver: PassDriver::Idle,
            scroll_offset: 0,
            validate_buffer: String::new(),
            last_agent_msg_id: None,
            pending_tools: HashMap::new(),
            spec_panel_forced: None,
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
                let upper = self.validate_buffer.to_uppercase();
                if upper.contains("VERDICT: VALIDE") {
                    self.pass.advance();
                    self.push_system("✓ passe SDD complète — spec verrouillée");
                } else {
                    self.pass.fail_current();
                    if upper.contains("VERDICT: DRIFT") {
                        self.push_system("⚠ drift détecté — spec non verrouillée");
                    } else {
                        self.push_system("⚠ verdict absent ou imparsable — DRIFT par prudence");
                    }
                }
                self.driver = PassDriver::Idle;
                None
            }
            _ => None,
        }
    }

    /// Posé au démarrage effectif d'un tour (Submit/relance) : réarme le
    /// chrono et la tally de tokens propre à ce tour, et efface
    /// `turn_pending` — le setup asynchrone qui précédait vient de résoudre.
    pub fn begin_turn(&mut self) {
        self.turn_active = true;
        self.turn_pending = false;
        self.turn_started = Some(Instant::now());
        self.tokens_turn_in = 0;
        self.tokens_turn_out = 0;
        self.cost_turn = None;
    }

    /// Effacé à la fin du tour (stream terminé ou erreur) — laisse le
    /// cumul de session (tokens_total_*) intact.
    pub fn finish_turn(&mut self) {
        self.turn_active = false;
        self.turn_started = None;
    }

    fn total_chat_lines(&self) -> u16 {
        self.chat
            .iter()
            .map(|line| line.text.split('\n').count() as u16)
            .sum()
    }

    fn max_scroll(&self) -> u16 {
        self.total_chat_lines().saturating_sub(1)
    }

    pub fn input_cursor_chars(&self) -> u16 {
        self.input.chars().count() as u16
    }

    /// `.wrap()` et `scroll.x` sont mutuellement exclusifs sur `Paragraph`
    /// dans ratatui 0.30 (la branche wrap ignore `scroll.x`) — ne pas
    /// ajouter `.wrap()` au Paragraph de l'input sans retirer ce scroll.
    pub fn input_scroll_x(&self, visible_width: u16) -> u16 {
        self.input_cursor_chars()
            .saturating_sub(visible_width.saturating_sub(1))
    }

    pub fn scroll_page_up(&mut self) {
        self.scroll_offset = (self.scroll_offset + SCROLL_PAGE).min(self.max_scroll());
    }

    pub fn scroll_page_down(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_sub(SCROLL_PAGE);
    }

    pub fn scroll_home(&mut self) {
        self.scroll_offset = self.max_scroll();
    }

    pub fn scroll_end(&mut self) {
        self.scroll_offset = 0;
    }

    pub fn spec_panel_visible(&self) -> bool {
        self.spec_panel_forced
            .unwrap_or_else(|| self.spec.is_some() || self.pass.is_running())
    }

    pub fn toggle_spec_panel(&mut self) {
        self.spec_panel_forced = Some(!self.spec_panel_visible());
    }

    pub fn take_tool_approval(&mut self) -> Option<ToolApprovalRequest> {
        self.tool_approval.take()
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
        match key.code {
            KeyCode::F(2) => {
                self.toggle_spec_panel();
                return Action::None;
            }
            KeyCode::PageUp => {
                self.scroll_page_up();
                return Action::None;
            }
            KeyCode::PageDown => {
                self.scroll_page_down();
                return Action::None;
            }
            KeyCode::Home => {
                self.scroll_home();
                return Action::None;
            }
            KeyCode::End => {
                self.scroll_end();
                return Action::None;
            }
            _ => {}
        }
        if self.tool_approval.is_some() {
            return match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => Action::ToolApprove,
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => Action::ToolDeny,
                _ => Action::None,
            };
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
            KeyCode::Esc if self.turn_active || self.turn_pending => Action::CancelTurn,
            KeyCode::Enter if self.turn_active || self.turn_pending => {
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
                } else if text == "/help" {
                    Action::Help
                } else if text == "/spec" {
                    self.toggle_spec_panel();
                    Action::None
                } else if text == "/cost" {
                    Action::Cost
                } else if text == "/docker" {
                    Action::Docker
                } else {
                    Action::Submit(text)
                }
            }
            _ => Action::None,
        }
    }

    pub fn push_user(&mut self, text: &str) {
        // Some providers echo the triggering user message back through the
        // reply stream (e.g. built-in slash commands answered without a model
        // round-trip) — the TUI already renders it eagerly on submit, so drop
        // an immediate consecutive repeat instead of showing it twice.
        let is_echo = self
            .chat
            .last()
            .is_some_and(|line| line.sender == Sender::User && line.text == text);
        if is_echo {
            return;
        }
        self.chat.push(ChatLine {
            sender: Sender::User,
            text: text.to_string(),
            tool: None,
            rendered: None,
        });
        self.last_agent_msg_id = None;
    }

    pub fn push_system(&mut self, text: &str) {
        self.chat.push(ChatLine {
            sender: Sender::System,
            text: text.to_string(),
            tool: None,
            rendered: None,
        });
        self.last_agent_msg_id = None;
    }

    /// Same as [`Self::push_system`] but with pre-rendered, per-role-styled
    /// lines (aligned tables) — used for on-demand report blocks (`/cost`,
    /// `/docker`) that need theme styling the plain system register can't
    /// express.
    pub fn push_system_lines(&mut self, lines: Vec<Line<'static>>) {
        let text = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        self.chat.push(ChatLine {
            sender: Sender::System,
            text,
            tool: None,
            rendered: Some(lines),
        });
        self.last_agent_msg_id = None;
    }

    pub fn apply_agent_event(&mut self, ev: &AgentEvent) {
        match ev {
            AgentEvent::Message(message) => self.apply_message(message),
            AgentEvent::Usage(usage) => self.apply_usage(usage),
            // MessageUsage carries the same round-trip totals already seen via
            // Usage (both derive from the same provider response) — accumulating
            // both would double the token tally shown in the header.
            AgentEvent::MessageUsage { .. } => {}
            AgentEvent::HistoryReplaced(_) => self.push_system("— historique compacté —"),
            _ => {}
        }
    }

    fn apply_usage(&mut self, usage: &ProviderUsage) {
        let input = i64::from(usage.usage.input_tokens.unwrap_or(0));
        let output = i64::from(usage.usage.output_tokens.unwrap_or(0));
        self.tokens_turn_in += input;
        self.tokens_turn_out += output;
        self.tokens_total_in += input;
        self.tokens_total_out += output;
        if let Some(cost) = usage.cost {
            self.cost_turn = Some(self.cost_turn.unwrap_or(0.0) + cost);
            self.cost_total = Some(self.cost_total.unwrap_or(0.0) + cost);
        }
    }

    fn apply_message(&mut self, message: &Message) {
        if message.role == Role::Assistant {
            self.apply_assistant_message(message);
        } else if message.role == Role::User {
            self.apply_user_message(message);
        }
    }

    fn apply_assistant_message(&mut self, message: &Message) {
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
                        .unwrap_or("outil")
                        .to_string();
                    self.chat.push(ChatLine {
                        sender: Sender::System,
                        text: format!("⚙ {name}"),
                        tool: Some(ToolLineState {
                            name,
                            started: Instant::now(),
                        }),
                        rendered: None,
                    });
                    self.pending_tools
                        .insert(req.id.clone(), self.chat.len() - 1);
                    self.last_agent_msg_id = None;
                }
                MessageContentBlock::ActionRequired(action) => {
                    if let ActionRequiredData::ToolConfirmation {
                        id,
                        tool_name,
                        prompt,
                        ..
                    } = &action.data
                    {
                        self.tool_approval = Some(ToolApprovalRequest {
                            id: id.clone(),
                            tool_name: tool_name.clone(),
                            prompt: prompt.clone(),
                        });
                    }
                }
                _ => {}
            }
        }
    }

    fn apply_user_message(&mut self, message: &Message) {
        for block in &message.content {
            match block {
                MessageContentBlock::Text(text) => self.push_user(&text.text),
                MessageContentBlock::ToolResponse(resp) => {
                    match self.pending_tools.remove(&resp.id) {
                        Some(idx) => {
                            let (name, elapsed) = self.chat[idx]
                                .tool
                                .as_ref()
                                .map(|t| (t.name.clone(), t.started.elapsed()))
                                .unwrap_or_else(|| ("outil".to_string(), Duration::ZERO));
                            let symbol = if resp.tool_result.is_ok() {
                                "✓"
                            } else {
                                "✗"
                            };
                            self.chat[idx].text =
                                format!("{symbol} {name} ({:.1}s)", elapsed.as_secs_f64());
                            self.chat[idx].tool = None;
                        }
                        None => self.push_system("✓ outil terminé"),
                    }
                    self.last_agent_msg_id = None;
                }
                _ => {}
            }
        }
    }

    /// Closes any tool line still awaiting its response after a history
    /// replay (`--resume` seeding a `ToolRequest` whose `ToolResponse` was
    /// never persisted — the session was interrupted mid-call) so it reads
    /// as "interrupted" instead of a spinner that will never resolve.
    pub fn close_orphaned_tool_requests(&mut self) {
        for (_, idx) in self.pending_tools.drain() {
            let Some(line) = self.chat.get_mut(idx) else {
                continue;
            };
            let name = line
                .tool
                .as_ref()
                .map(|t| t.name.clone())
                .unwrap_or_else(|| "outil".to_string());
            line.text = format!("✗ {name} (interrompu)");
            line.tool = None;
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
            tool: None,
            rendered: None,
        });
        self.last_agent_msg_id = message_id.clone();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaji::conversation::message::Message;
    use kaji::providers::base::Usage;
    use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
    use ratatui::text::Span;
    use rmcp::model::{CallToolRequestParams, CallToolResult};

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
    fn esc_during_pending_setup_returns_cancel_turn() {
        let mut app = App::new(None);
        app.turn_pending = true;
        assert_eq!(app.on_event(&key(KeyCode::Esc)), Action::CancelTurn);
    }

    #[test]
    fn enter_during_pending_setup_does_not_submit() {
        let mut app = App::new(None);
        app.turn_pending = true;
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
    fn turn_pending_lifecycle() {
        let mut app = App::new(None);
        assert!(!app.turn_pending);

        app.turn_pending = true;
        app.begin_turn();
        assert!(
            !app.turn_pending,
            "begin_turn installs the turn — the setup it followed is no longer pending"
        );
        assert!(app.turn_active);
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
    fn verdict_absent_fails_closed_as_drift() {
        let mut app = App::new(Some(spec()));
        app.start_pass();
        app.gate_approve();
        agent_says(&mut app, "m1", "fait autre chose");
        app.turn_end();
        agent_says(&mut app, "m2", "réponse sans le token attendu");
        app.turn_end();
        assert!(app.pass.drifted());
        assert!(!app.pass.is_complete());
        assert!(app.chat.iter().any(|l| l.text.contains("verdict absent")));
    }

    #[test]
    fn verdict_empty_buffer_fails_closed_as_drift() {
        let mut app = App::new(Some(spec()));
        app.start_pass();
        app.gate_approve();
        agent_says(&mut app, "m1", "fait autre chose");
        app.turn_end();
        agent_says(&mut app, "m2", "");
        app.turn_end();
        assert!(app.pass.drifted());
        assert!(!app.pass.is_complete());
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

    #[test]
    fn scroll_page_up_and_down_are_bounded() {
        let mut app = App::new(None);
        for i in 0..30 {
            app.push_system(&format!("line {i}"));
        }
        assert_eq!(app.scroll_offset, 0);
        app.scroll_page_up();
        assert!(app.scroll_offset > 0);

        app.scroll_home();
        let top = app.scroll_offset;
        app.scroll_page_up();
        assert_eq!(app.scroll_offset, top, "clamped at the top");

        app.scroll_end();
        assert_eq!(app.scroll_offset, 0);
    }

    #[test]
    fn scroll_page_down_never_goes_negative() {
        let mut app = App::new(None);
        app.push_system("only one line");
        app.scroll_page_down();
        assert_eq!(app.scroll_offset, 0);
    }

    #[test]
    fn page_and_home_end_keys_drive_scroll_state() {
        let mut app = App::new(None);
        for i in 0..30 {
            app.push_system(&format!("line {i}"));
        }
        assert_eq!(app.on_event(&key(KeyCode::PageUp)), Action::None);
        assert!(app.scroll_offset > 0);
        assert_eq!(app.on_event(&key(KeyCode::Home)), Action::None);
        assert!(app.scroll_offset > 0);
        assert_eq!(app.on_event(&key(KeyCode::End)), Action::None);
        assert_eq!(app.scroll_offset, 0);
    }

    #[test]
    fn input_scroll_x_is_zero_while_input_fits_the_visible_width() {
        let mut app = App::new(None);
        app.input = "short".to_string();
        assert_eq!(app.input_scroll_x(20), 0);
    }

    #[test]
    fn input_scroll_x_follows_cursor_once_input_overflows_the_visible_width() {
        let mut app = App::new(None);
        app.input = "a".repeat(30);
        assert_eq!(app.input_scroll_x(10), 21);
    }

    #[test]
    fn input_scroll_x_never_panics_on_zero_width_area() {
        let mut app = App::new(None);
        app.input = "hello".to_string();
        assert_eq!(app.input_scroll_x(0), 5);
    }

    #[test]
    fn input_cursor_chars_counts_unicode_scalars_not_bytes() {
        let mut app = App::new(None);
        app.input = "héllo".to_string();
        assert_eq!(app.input_cursor_chars(), 5);
        assert_ne!(app.input_cursor_chars() as usize, app.input.len());
    }

    #[test]
    fn typing_long_input_keeps_scroll_offset_zero_at_or_below_width() {
        let mut app = App::new(None);
        let width = 10u16;
        for _ in 0..width - 1 {
            app.on_event(&key(KeyCode::Char('a')));
        }
        assert_eq!(app.input_scroll_x(width), 0);
        app.on_event(&key(KeyCode::Char('a')));
        assert!(app.input_scroll_x(width) > 0);
    }

    #[test]
    fn tool_request_then_response_updates_line_with_checkmark() {
        let mut app = App::new(None);
        let req_msg =
            Message::assistant().with_tool_request("t1", Ok(CallToolRequestParams::new("shell")));
        app.apply_agent_event(&AgentEvent::Message(req_msg));

        assert!(app.chat.iter().any(|l| l.text.contains("⚙ shell")));
        assert!(app.chat.iter().any(|l| l.tool.is_some()));

        let resp_msg =
            Message::user().with_tool_response("t1", Ok(CallToolResult::success(vec![])));
        app.apply_agent_event(&AgentEvent::Message(resp_msg));

        assert!(!app.chat.iter().any(|l| l.text.contains("⚙ shell")));
        assert!(app
            .chat
            .iter()
            .any(|l| l.text.contains('✓') && l.text.contains("shell")));
        assert!(!app.chat.iter().any(|l| l.tool.is_some()));
    }

    #[test]
    fn tool_response_with_error_result_marks_failure() {
        let mut app = App::new(None);
        let req_msg =
            Message::assistant().with_tool_request("t2", Ok(CallToolRequestParams::new("compile")));
        app.apply_agent_event(&AgentEvent::Message(req_msg));

        let resp_msg = Message::user().with_tool_response(
            "t2",
            Err(rmcp::model::ErrorData {
                code: rmcp::model::ErrorCode::INTERNAL_ERROR,
                message: std::borrow::Cow::from("boom"),
                data: None,
            }),
        );
        app.apply_agent_event(&AgentEvent::Message(resp_msg));

        assert!(app
            .chat
            .iter()
            .any(|l| l.text.contains('✗') && l.text.contains("compile")));
    }

    #[test]
    fn begin_turn_resets_turn_tokens_and_arms_the_clock() {
        let mut app = App::new(None);
        app.tokens_turn_in = 42;
        app.begin_turn();
        assert!(app.turn_active);
        assert!(app.turn_started.is_some());
        assert_eq!(app.tokens_turn_in, 0);
    }

    #[test]
    fn finish_turn_clears_active_flag_and_clock_but_keeps_totals() {
        let mut app = App::new(None);
        app.begin_turn();
        app.tokens_total_in = 100;
        app.finish_turn();
        assert!(!app.turn_active);
        assert!(app.turn_started.is_none());
        assert_eq!(app.tokens_total_in, 100);
    }

    #[test]
    fn usage_event_accumulates_turn_and_total_tokens() {
        let mut app = App::new(None);
        app.begin_turn();
        let usage = ProviderUsage::new(
            "test-model".to_string(),
            Usage::new(Some(10), Some(4), None),
        );
        app.apply_agent_event(&AgentEvent::Usage(usage.clone()));
        assert_eq!(app.tokens_turn_in, 10);
        assert_eq!(app.tokens_turn_out, 4);
        assert_eq!(app.tokens_total_in, 10);
        assert_eq!(app.tokens_total_out, 4);

        app.apply_agent_event(&AgentEvent::Usage(usage));
        assert_eq!(app.tokens_turn_in, 20);
        assert_eq!(app.tokens_total_in, 20);
    }

    #[test]
    fn usage_event_with_cost_accumulates_turn_and_session_cost() {
        use kaji::providers::base::CostSource;

        let mut app = App::new(None);
        assert_eq!(app.cost_total, None);
        app.begin_turn();
        let usage = ProviderUsage::new(
            "test-model".to_string(),
            Usage::new(Some(10), Some(4), None),
        )
        .with_cost(0.10, CostSource::Estimated);
        app.apply_agent_event(&AgentEvent::Usage(usage.clone()));
        assert_eq!(app.cost_turn, Some(0.10));
        assert_eq!(app.cost_total, Some(0.10));

        app.apply_agent_event(&AgentEvent::Usage(usage));
        assert_eq!(app.cost_turn, Some(0.20));
        assert_eq!(app.cost_total, Some(0.20));

        app.begin_turn();
        assert_eq!(app.cost_turn, None, "reset per turn");
        assert_eq!(app.cost_total, Some(0.20), "kept across turns");
    }

    #[test]
    fn usage_event_without_cost_leaves_cost_fields_none() {
        let mut app = App::new(None);
        app.begin_turn();
        let usage = ProviderUsage::new(
            "test-model".to_string(),
            Usage::new(Some(10), Some(4), None),
        );
        app.apply_agent_event(&AgentEvent::Usage(usage));
        assert_eq!(app.cost_turn, None);
        assert_eq!(app.cost_total, None);
    }

    #[test]
    fn message_usage_event_does_not_double_count_tokens() {
        let mut app = App::new(None);
        app.begin_turn();
        let usage = ProviderUsage::new(
            "test-model".to_string(),
            Usage::new(Some(10), Some(4), None),
        );
        app.apply_agent_event(&AgentEvent::Usage(usage));
        app.apply_agent_event(&AgentEvent::MessageUsage {
            message_id: Some("m1".to_string()),
            usage: kaji::conversation::message::MessageUsage::default(),
        });
        assert_eq!(app.tokens_turn_in, 10);
    }

    #[test]
    fn spec_panel_hidden_by_default_without_spec_or_pass() {
        let app = App::new(None);
        assert!(!app.spec_panel_visible());
    }

    #[test]
    fn spec_panel_visible_when_spec_loaded() {
        let app = App::new(Some(spec()));
        assert!(app.spec_panel_visible());
    }

    #[test]
    fn slash_spec_toggles_panel_regardless_of_state() {
        let mut app = App::new(None);
        assert!(!app.spec_panel_visible());
        for c in "/spec".chars() {
            app.on_event(&key(KeyCode::Char(c)));
        }
        app.on_event(&key(KeyCode::Enter));
        assert!(app.spec_panel_visible());

        for c in "/spec".chars() {
            app.on_event(&key(KeyCode::Char(c)));
        }
        app.on_event(&key(KeyCode::Enter));
        assert!(!app.spec_panel_visible());
    }

    #[test]
    fn f2_toggles_spec_panel() {
        let mut app = App::new(None);
        assert!(!app.spec_panel_visible());
        app.on_event(&key(KeyCode::F(2)));
        assert!(app.spec_panel_visible());
    }

    #[test]
    fn slash_help_returns_help_action() {
        let mut app = App::new(None);
        for c in "/help".chars() {
            app.on_event(&key(KeyCode::Char(c)));
        }
        assert_eq!(app.on_event(&key(KeyCode::Enter)), Action::Help);
    }

    #[test]
    fn slash_cost_returns_cost_action() {
        let mut app = App::new(None);
        for c in "/cost".chars() {
            app.on_event(&key(KeyCode::Char(c)));
        }
        assert_eq!(app.on_event(&key(KeyCode::Enter)), Action::Cost);
        assert!(app.input.is_empty());
    }

    #[test]
    fn slash_docker_returns_docker_action() {
        let mut app = App::new(None);
        for c in "/docker".chars() {
            app.on_event(&key(KeyCode::Char(c)));
        }
        assert_eq!(app.on_event(&key(KeyCode::Enter)), Action::Docker);
        assert!(app.input.is_empty());
    }

    #[test]
    fn push_system_lines_marks_the_chat_line_as_rendered_and_flattens_text() {
        let mut app = App::new(None);
        app.push_system_lines(vec![
            Line::from(Span::raw("/cost")),
            Line::from(Span::raw("session : 10")),
        ]);
        let line = app.chat.last().expect("chat line pushed");
        assert!(matches!(line.sender, Sender::System));
        assert!(line.rendered.is_some());
        assert_eq!(line.text, "/cost\nsession : 10");
    }

    #[test]
    fn action_required_tool_confirmation_opens_approval_and_y_n_answer_it() {
        let mut app = App::new(None);
        let msg = Message::assistant().with_action_required(
            "req-1".to_string(),
            "shell".to_string(),
            Default::default(),
            Some("exécuter `rm -rf /tmp/x` ?".to_string()),
        );
        app.apply_agent_event(&AgentEvent::Message(msg));

        assert!(app.tool_approval.is_some());
        assert_eq!(app.on_event(&key(KeyCode::Char('y'))), Action::ToolApprove);
        let taken = app
            .take_tool_approval()
            .expect("approval consumed by test, not event");
        assert_eq!(taken.id, "req-1");
        assert_eq!(taken.tool_name, "shell");
    }

    #[test]
    fn action_required_tool_confirmation_n_denies() {
        let mut app = App::new(None);
        let msg = Message::assistant().with_action_required(
            "req-2".to_string(),
            "shell".to_string(),
            Default::default(),
            None,
        );
        app.apply_agent_event(&AgentEvent::Message(msg));
        assert_eq!(app.on_event(&key(KeyCode::Char('n'))), Action::ToolDeny);
    }
}
