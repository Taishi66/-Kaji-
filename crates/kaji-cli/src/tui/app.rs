use kaji::agents::AgentEvent;
use kaji::conversation::message::{ActionRequiredData, Message, MessageContentBlock};
use kaji::providers::base::ProviderUsage;
use kaji_core::sdd::{SddPass, SpecDoc};
use ratatui::crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind};
use ratatui::text::Line;
use rmcp::model::Role;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::time::{Duration, Instant};

const SCROLL_PAGE: u16 = 10;
const SCROLL_WHEEL: u16 = 3;

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
    /// Accumulated `Thinking` content blocks for the turn — only ever
    /// created while `show_thinking` is on (dropped silently otherwise).
    Thinking,
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

pub struct Command {
    pub name: &'static str,
    pub desc: &'static str,
    run: fn(&mut App) -> Action,
}

impl Command {
    pub fn run(&self, app: &mut App) -> Action {
        (self.run)(app)
    }
}

pub const COMMANDS: &[Command] = &[
    Command {
        name: "/sdd",
        desc: "démarre une passe SDD (SPEC.md auto-détecté ou --spec <fichier>)",
        run: |_| Action::StartPass,
    },
    Command {
        name: "/spec",
        desc: "(ou F2) affiche/masque le panneau SPEC",
        run: |app| {
            app.toggle_spec_panel();
            Action::None
        },
    },
    Command {
        name: "/think",
        desc: "(ou F3) affiche/masque le raisonnement du modèle (思考中)",
        run: |app| {
            app.toggle_thinking();
            Action::None
        },
    },
    Command {
        name: "/cost",
        desc: "affiche l'usage tokens/coût (session, 5 h, 7 j) — budgets optionnels via KAJI_BUDGET_5H / KAJI_BUDGET_7J",
        run: |_| Action::Cost,
    },
    Command {
        name: "/docker",
        desc: "liste les conteneurs en cours",
        run: |_| Action::Docker,
    },
    Command {
        name: "/help",
        desc: "réaffiche l'aide",
        run: |_| Action::Help,
    },
    Command {
        name: "/quit",
        desc: "quitte kaji",
        run: |_| Action::Quit,
    },
];

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
    /// Set on the turn's first visible `Text` chunk (thinking doesn't
    /// count) — reset per turn by `reset_turn_visibility`. Drives the
    /// loader zen: it disappears the moment there is something to read.
    pub turn_has_visible_output: bool,
    /// Set the first time a `Thinking` block renders a chat line this turn
    /// (`merge_agent_thinking`) — unlike `turn_thinking_visible`, this does
    /// NOT depend on the current value of `show_thinking`: once a 思 line
    /// is on screen it stays on screen even if `/think`/F3 toggles off
    /// mid-turn, so the loader must keep treating the turn as "already has
    /// visible content" regardless of the toggle. Reset per turn by
    /// `reset_turn_visibility`.
    turn_thinking_shown: bool,
    /// Togglable via `/think` and F3 — default off (zen). When on, streamed
    /// `Thinking` blocks render dimmed/italic with the `思` prefix instead
    /// of being dropped.
    pub show_thinking: bool,
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
    /// Real overflow (wrapped rows beyond the viewport) measured by
    /// `draw_chat` at render time — `max_scroll` reads this instead of
    /// counting raw `\n` lines, which undercounts once markdown expansion
    /// and terminal-width wrapping are in play. `Cell` because `draw_chat`
    /// only borrows `&App`; the UI loop is single-threaded so `Cell<u16>`
    /// (Send, not Sync) is sufficient — no `Sync` bound is needed here.
    pub chat_overflow: Cell<u16>,
    /// Row (same coordinate space as `chat_overflow`/`scroll_offset`) at
    /// which each `Sender::User` chat line starts, measured by `draw_chat`
    /// at render time — cleared and repopulated every draw. Powers
    /// Ctrl+↑/↓ turn jumps (`jump_prev_turn`/`jump_next_turn`).
    pub user_turn_rows: RefCell<Vec<u16>>,
    /// Every non-empty Enter submission, oldest first — in-memory only
    /// (not persisted across sessions). Recalled by ↑/↓ when the mouse is
    /// enabled (native wheel scroll takes over the plain arrows).
    prompt_history: Vec<String>,
    /// `Some(idx)` while ↑/↓ is browsing `prompt_history` — `None` means the
    /// input holds a fresh (non-recalled) draft. Any edit (typing,
    /// backspace) exits browsing by resetting this to `None` while leaving
    /// `input` untouched.
    history_index: Option<usize>,
    /// Index into `palette_matches()` while the command palette is open.
    /// Cyclic ↑/↓ navigation only; reset to 0 by `reset_palette_selection`
    /// whenever `input` changes so a narrower filter always starts back at
    /// its first (and often only) match.
    pub palette_selected: usize,
    /// Set once in `run()` from the `KAJI_MOUSE` kill-switch — defaults to
    /// `false` here so the ~60 existing `App::new` call sites (mostly
    /// tests) keep the legacy arrow-scroll behavior unless the caller
    /// explicitly opts in.
    pub mouse_enabled: bool,
    validate_buffer: String,
    last_agent_msg_id: Option<String>,
    /// Same merge-chain invariant as `last_agent_msg_id`, tracked
    /// separately since a streamed message can interleave `Thinking` and
    /// `Text`/`ToolRequest` blocks under the same id — merging thinking
    /// chunks into whichever chat line happens to be last would corrupt
    /// unrelated lines. Reset alongside `last_agent_msg_id` by
    /// `reset_agent_merge_ids`, and per-turn by `reset_turn_visibility`.
    last_thinking_msg_id: Option<String>,
    /// Chat index of the agent text line currently open for streamed
    /// chunks (T4, ninja cursor) — materializes the same merge-chain
    /// invariant as `last_agent_msg_id` rather than re-deriving it from
    /// `chat.last()`, so it stays correct even when a new turn starts with
    /// no intervening chat line to otherwise break the chain (the SDD
    /// gate→exec→validate auto-chain). Kept in sync at the same call sites
    /// as `last_agent_msg_id`/`last_thinking_msg_id`: set on every text
    /// push/merge, cleared by `reset_agent_merge_ids`, by
    /// `merge_agent_thinking` (thinking always breaks the text chain), and
    /// by `reset_turn_visibility` at the start of every turn.
    agent_stream_idx: Option<usize>,
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
            turn_has_visible_output: false,
            turn_thinking_shown: false,
            show_thinking: false,
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
            chat_overflow: Cell::new(0),
            user_turn_rows: RefCell::new(Vec::new()),
            prompt_history: Vec::new(),
            history_index: None,
            palette_selected: 0,
            mouse_enabled: false,
            validate_buffer: String::new(),
            last_agent_msg_id: None,
            last_thinking_msg_id: None,
            agent_stream_idx: None,
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

    /// Called from every turn-begin path (`begin_setup`, covering Submit,
    /// GateApprove, and the chained exec→validate turn) — arms the loader
    /// zen and the thinking-merge chain for the new turn.
    pub fn reset_turn_visibility(&mut self) {
        self.turn_has_visible_output = false;
        self.turn_thinking_shown = false;
        self.last_thinking_msg_id = None;
        self.agent_stream_idx = None;
    }

    pub fn toggle_thinking(&mut self) {
        self.show_thinking = !self.show_thinking;
        let msg = if self.show_thinking {
            "思考中 affiché — /think ou F3 pour masquer"
        } else {
            "思考中 masqué — /think ou F3 pour afficher"
        };
        self.push_system(msg);
    }

    /// True once a `Thinking` block has rendered a chat line for the
    /// current turn — the loader's exception clause ("pas de loader quand
    /// `show_thinking` ON et que du thinking s'affiche déjà"). Deliberately
    /// independent of the *current* `show_thinking` value: once a 思 line
    /// is visible, toggling the setting off mid-turn must not bring the
    /// loader back underneath it.
    pub fn turn_thinking_visible(&self) -> bool {
        self.turn_thinking_shown
    }

    /// Loader zen visibility: a turn is in flight and nothing readable has
    /// arrived for it yet (thinking counts only while it's actually being
    /// shown).
    pub fn show_loader(&self) -> bool {
        (self.turn_pending || self.turn_active)
            && !self.turn_has_visible_output
            && !self.turn_thinking_visible()
    }

    /// Chat index of the agent text line the ninja cursor (T4) should paint
    /// onto — `None` outside an active turn (even if `chat.last()` still
    /// holds a finished agent line from before), and `None` whenever the
    /// merge chain currently points at a `Thinking` line or has been broken
    /// by a tool/system/user line. Mutually exclusive with `show_loader`:
    /// both key off the same first-visible-text-chunk transition, one
    /// turning off exactly when the other turns on.
    pub fn streaming_agent_line(&self) -> Option<usize> {
        if self.turn_active {
            self.agent_stream_idx
        } else {
            None
        }
    }

    fn max_scroll(&self) -> u16 {
        self.chat_overflow.get()
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

    pub fn delete_last_word(&mut self) {
        self.exit_history_navigation();
        self.reset_palette_selection();
        let trimmed_len = self.input.trim_end().len();
        self.input.truncate(trimmed_len);
        match self
            .input
            .char_indices()
            .rev()
            .find(|(_, c)| c.is_whitespace())
        {
            Some((pos, c)) => self.input.truncate(pos + c.len_utf8()),
            None => self.input.clear(),
        }
    }

    pub fn scroll_page_up(&mut self) {
        self.scroll_offset = (self.scroll_offset + SCROLL_PAGE).min(self.max_scroll());
    }

    pub fn scroll_page_down(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_sub(SCROLL_PAGE);
    }

    pub fn scroll_line_up(&mut self) {
        self.scroll_offset = (self.scroll_offset + 1).min(self.max_scroll());
    }

    pub fn scroll_line_down(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_sub(1);
    }

    pub fn scroll_home(&mut self) {
        self.scroll_offset = self.max_scroll();
    }

    pub fn scroll_end(&mut self) {
        self.scroll_offset = 0;
    }

    pub fn scroll_wheel_up(&mut self) {
        self.scroll_offset = (self.scroll_offset + SCROLL_WHEEL).min(self.max_scroll());
    }

    pub fn scroll_wheel_down(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_sub(SCROLL_WHEEL);
    }

    /// Positions the start of the nearest user turn strictly above the
    /// current top row at the top of the viewport. `top` is the row
    /// currently at the top of the view — same derivation the renderer uses
    /// (`base_scroll − scroll_offset`). No-op if there is no user turn
    /// above (already at/past the first one).
    pub fn jump_prev_turn(&mut self) {
        let base = self.chat_overflow.get();
        let top = base.saturating_sub(self.scroll_offset);
        let target = self
            .user_turn_rows
            .borrow()
            .iter()
            .rev()
            .find(|&&row| row < top)
            .copied();
        if let Some(target) = target {
            self.scroll_offset = base.saturating_sub(target);
        }
    }

    /// Mirror of [`Self::jump_prev_turn`] for the nearest user turn strictly
    /// below the current top row.
    pub fn jump_next_turn(&mut self) {
        let base = self.chat_overflow.get();
        let top = base.saturating_sub(self.scroll_offset);
        let target = self
            .user_turn_rows
            .borrow()
            .iter()
            .find(|&&row| row > top)
            .copied();
        if let Some(target) = target {
            self.scroll_offset = base.saturating_sub(target);
        }
    }

    /// Exits prompt-history browsing (if active) without touching `input` —
    /// called by every input-editing key so typing/backspacing while
    /// recalling a prompt keeps the edit instead of snapping back.
    fn exit_history_navigation(&mut self) {
        self.history_index = None;
    }

    /// `HISTCONTROL=ignoredups`: an immediately-repeated submit (recalling
    /// a prompt with ↑ and resubmitting it unedited) doesn't grow the
    /// history with a duplicate right next to the original.
    fn push_history(&mut self, text: &str) {
        if self.prompt_history.last().map(String::as_str) != Some(text) {
            self.prompt_history.push(text.to_string());
        }
        self.history_index = None;
    }

    /// ↑ when the mouse is enabled: recalls the previous prompt. Only
    /// starts browsing from an empty input (an in-progress draft is never
    /// clobbered); once browsing, repeated presses walk further back,
    /// clamped at the oldest entry.
    pub fn history_prev(&mut self) {
        if self.history_index.is_none() && !self.input.is_empty() {
            return;
        }
        let next_idx = match self.history_index {
            Some(idx) => idx.saturating_sub(1),
            None => match self.prompt_history.len().checked_sub(1) {
                Some(idx) => idx,
                None => return,
            },
        };
        if let Some(text) = self.prompt_history.get(next_idx) {
            self.input = text.clone();
            self.history_index = Some(next_idx);
            self.reset_palette_selection();
        }
    }

    /// ↓ when the mouse is enabled: walks forward through history, clearing
    /// the input and exiting browsing once past the most recent entry. A
    /// no-op while not currently browsing.
    pub fn history_next(&mut self) {
        let Some(idx) = self.history_index else {
            return;
        };
        self.reset_palette_selection();
        if idx + 1 < self.prompt_history.len() {
            self.history_index = Some(idx + 1);
            self.input = self.prompt_history[idx + 1].clone();
        } else {
            self.history_index = None;
            self.input.clear();
        }
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

    /// A y/n modal (tool approval or gate) is on screen and owns the
    /// keyboard — the palette must not open underneath it, and the plain
    /// arrows must fall back to chat scroll instead of history recall (see
    /// `on_event`'s `modal_active` local).
    pub fn modal_active(&self) -> bool {
        self.tool_approval.is_some() || self.gate_open
    }

    /// Commands whose name starts with the current input — empty whenever
    /// `input` doesn't start with `/` (including the empty input), which is
    /// also what makes `palette_visible` false with nothing typed.
    pub fn palette_matches(&self) -> Vec<&'static Command> {
        if !self.input.starts_with('/') {
            return Vec::new();
        }
        COMMANDS
            .iter()
            .filter(|c| c.name.starts_with(self.input.as_str()))
            .collect()
    }

    /// The palette is on screen: no modal is stealing the keyboard, and the
    /// current prefix filter has at least one match (a filter with zero
    /// matches closes the palette rather than showing an empty box).
    pub fn palette_visible(&self) -> bool {
        !self.modal_active() && !self.palette_matches().is_empty()
    }

    /// Called wherever `input` mutates so a narrower/wider filter always
    /// resets the selection back to the first match instead of pointing at
    /// a now-unrelated item.
    fn reset_palette_selection(&mut self) {
        self.palette_selected = 0;
    }

    pub fn on_event(&mut self, ev: &Event) -> Action {
        if let Event::Mouse(mouse) = ev {
            match mouse.kind {
                MouseEventKind::ScrollUp => self.scroll_wheel_up(),
                MouseEventKind::ScrollDown => self.scroll_wheel_down(),
                _ => {}
            }
            return Action::None;
        }
        let Event::Key(key) = ev else {
            return Action::None;
        };
        if key.kind != KeyEventKind::Press {
            return Action::None;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Action::Quit;
        }
        // A y/n modal (tool approval or gate) swallows Enter/chars, but bare
        // ↑/↓ used to reach the history-recall branch below before the
        // modal guards further down even ran — recalling a prompt would
        // silently mutate `input` behind the modal. Ctrl+↑/↓ (turn jump)
        // and the mouse wheel are unaffected: only the plain-arrow
        // history/scroll choice depends on this.
        let modal_active = self.modal_active();
        match key.code {
            KeyCode::Up if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.jump_prev_turn();
                return Action::None;
            }
            KeyCode::Down if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.jump_next_turn();
                return Action::None;
            }
            KeyCode::F(2) => {
                self.toggle_spec_panel();
                return Action::None;
            }
            KeyCode::F(3) => {
                self.toggle_thinking();
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
            // With the mouse enabled, the native wheel (Event::Mouse above)
            // owns chat scrolling, so the bare arrows are freed up for
            // prompt-history recall instead. `KAJI_MOUSE=0` keeps the
            // legacy line-scroll behavior and leaves history unbound to the
            // arrows (documented degradation).
            KeyCode::Up => {
                if self.palette_visible() {
                    let n = self.palette_matches().len();
                    self.palette_selected = (self.palette_selected + n - 1) % n;
                } else if !modal_active && self.mouse_enabled {
                    self.history_prev();
                } else {
                    self.scroll_line_up();
                }
                return Action::None;
            }
            KeyCode::Down => {
                if self.palette_visible() {
                    let n = self.palette_matches().len();
                    self.palette_selected = (self.palette_selected + 1) % n;
                } else if !modal_active && self.mouse_enabled {
                    self.history_next();
                } else {
                    self.scroll_line_down();
                }
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
            KeyCode::Backspace
                if key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.delete_last_word();
                Action::None
            }
            KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.delete_last_word();
                Action::None
            }
            KeyCode::Char(c) => {
                self.exit_history_navigation();
                self.reset_palette_selection();
                self.input.push(c);
                Action::None
            }
            KeyCode::Backspace => {
                self.exit_history_navigation();
                self.reset_palette_selection();
                self.input.pop();
                Action::None
            }
            KeyCode::Tab if self.palette_visible() => {
                let matches = self.palette_matches();
                let name = matches[self.palette_selected.min(matches.len() - 1)].name;
                self.input = name.to_string();
                self.exit_history_navigation();
                self.reset_palette_selection();
                Action::None
            }
            KeyCode::Esc if self.palette_visible() => {
                self.input.clear();
                self.reset_palette_selection();
                Action::None
            }
            KeyCode::Esc if self.turn_active || self.turn_pending => Action::CancelTurn,
            KeyCode::Enter if self.turn_active || self.turn_pending => {
                self.push_system("tour en cours — Esc pour annuler d'abord");
                Action::None
            }
            KeyCode::Enter => {
                if self.palette_visible() {
                    let matches = self.palette_matches();
                    let cmd = matches[self.palette_selected.min(matches.len() - 1)];
                    self.input.clear();
                    self.reset_palette_selection();
                    self.push_history(cmd.name);
                    return cmd.run(self);
                }
                let text = std::mem::take(&mut self.input);
                let text = text.trim().to_string();
                if text.is_empty() {
                    Action::None
                } else {
                    self.push_history(&text);
                    if let Some(cmd) = COMMANDS.iter().find(|c| c.name == text) {
                        cmd.run(self)
                    } else {
                        Action::Submit(text)
                    }
                }
            }
            _ => Action::None,
        }
    }

    /// Breaks both streamed-merge chains (agent text and thinking) —
    /// called wherever a chat line that isn't a continuation of either is
    /// about to be pushed, so a later chunk sharing an old message id
    /// doesn't get appended onto an unrelated line.
    fn reset_agent_merge_ids(&mut self) {
        self.last_agent_msg_id = None;
        self.last_thinking_msg_id = None;
        self.agent_stream_idx = None;
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
        self.reset_agent_merge_ids();
    }

    pub fn push_system(&mut self, text: &str) {
        self.chat.push(ChatLine {
            sender: Sender::System,
            text: text.to_string(),
            tool: None,
            rendered: None,
        });
        self.reset_agent_merge_ids();
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
        self.reset_agent_merge_ids();
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
                    self.turn_has_visible_output = true;
                    self.merge_agent_text(&message.id, &text.text);
                    if self.driver == PassDriver::Validating {
                        self.validate_buffer.push_str(&text.text);
                    }
                }
                MessageContentBlock::Thinking(thinking) => {
                    if self.show_thinking {
                        self.merge_agent_thinking(&message.id, &thinking.thinking);
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
                    self.reset_agent_merge_ids();
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
                    self.reset_agent_merge_ids();
                }
                _ => {}
            }
        }
    }

    /// Closes any tool line still awaiting its response after a history
    /// replay (`--resume` seeding a `ToolRequest` whose `ToolResponse` was
    /// never persisted — the session was interrupted mid-call) so it reads
    /// as "interrupted" instead of a spinner that will never resolve. Also
    /// clears any stale `tool_approval` left by a replayed
    /// `ActionRequired`/`ToolConfirmation` block — same "session died
    /// mid-tool" contract: the confirmation channel behind it is dead, so
    /// leaving the approval modal open would swallow all input on resume.
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
        if let Some(req) = self.tool_approval.take() {
            self.push_system(&format!(
                "✗ {} — approbation abandonnée (session interrompue)",
                req.tool_name
            ));
        }
    }

    fn merge_agent_text(&mut self, message_id: &Option<String>, text: &str) {
        if message_id.is_some() && *message_id == self.last_agent_msg_id {
            if let Some(last) = self.chat.last_mut() {
                last.text.push_str(text);
                self.agent_stream_idx = Some(self.chat.len() - 1);
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
        // A same-id message can interleave Text and Thinking blocks — the
        // new line just pushed is now `chat.last()`, so any in-flight
        // thinking merge chain pointing at the same id would otherwise
        // append its next chunk onto THIS (visible, normal-style) line.
        self.last_thinking_msg_id = None;
        self.agent_stream_idx = Some(self.chat.len() - 1);
    }

    fn merge_agent_thinking(&mut self, message_id: &Option<String>, text: &str) {
        if message_id.is_some() && *message_id == self.last_thinking_msg_id {
            if let Some(last) = self.chat.last_mut() {
                last.text.push_str(text);
                return;
            }
        }
        self.chat.push(ChatLine {
            sender: Sender::Thinking,
            text: text.to_string(),
            tool: None,
            rendered: None,
        });
        self.last_thinking_msg_id = message_id.clone();
        self.turn_thinking_shown = true;
        // Mirror of the invalidation above: the pushed line is now
        // `chat.last()`, so the text merge chain must not append onto it —
        // and the ninja cursor (T4) must not paint onto a thinking line.
        self.last_agent_msg_id = None;
        self.agent_stream_idx = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaji::conversation::message::Message;
    use kaji::providers::base::Usage;
    use ratatui::crossterm::event::{
        Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
    };
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

    fn ctrl_key(code: KeyCode) -> Event {
        Event::Key(KeyEvent {
            code,
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: ratatui::crossterm::event::KeyEventState::NONE,
        })
    }

    fn mouse_event(kind: MouseEventKind) -> Event {
        Event::Mouse(MouseEvent {
            kind,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
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
    fn ctrl_backspace_deletes_previous_word() {
        let mut app = App::new(None);
        app.input = "hello world  ".to_string();
        let ctrl_backspace = Event::Key(KeyEvent {
            code: KeyCode::Backspace,
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: ratatui::crossterm::event::KeyEventState::NONE,
        });
        app.on_event(&ctrl_backspace);
        assert_eq!(app.input, "hello ");
        app.on_event(&ctrl_backspace);
        assert_eq!(app.input, "");
    }

    #[test]
    fn alt_backspace_and_ctrl_w_share_the_behavior() {
        let alt_backspace = Event::Key(KeyEvent {
            code: KeyCode::Backspace,
            modifiers: KeyModifiers::ALT,
            kind: KeyEventKind::Press,
            state: ratatui::crossterm::event::KeyEventState::NONE,
        });
        let ctrl_w = Event::Key(KeyEvent {
            code: KeyCode::Char('w'),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: ratatui::crossterm::event::KeyEventState::NONE,
        });

        let mut app_alt = App::new(None);
        app_alt.input = "hello world  ".to_string();
        app_alt.on_event(&alt_backspace);
        assert_eq!(app_alt.input, "hello ");

        let mut app_ctrl_w = App::new(None);
        app_ctrl_w.input = "hello world  ".to_string();
        app_ctrl_w.on_event(&ctrl_w);
        assert_eq!(app_ctrl_w.input, "hello ");
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

    fn open_tool_approval_modal(app: &mut App) {
        let msg = Message::assistant().with_action_required(
            "req-modal".to_string(),
            "shell".to_string(),
            Default::default(),
            None,
        );
        app.apply_agent_event(&AgentEvent::Message(msg));
        assert!(
            app.tool_approval.is_some(),
            "test setup: modal must be open"
        );
    }

    /// Bug: bare ↑/↓ were handled (and returned) before the `tool_approval`/
    /// `gate_open` guards further down, so while a y/n modal was open, ↑
    /// still ran `history_prev` and clobbered `input` with a recalled
    /// prompt — the modal swallows `y`/`n` but arrow-key input leaked past
    /// it into the draft the user can't even see behind the modal.
    #[test]
    fn plain_up_scrolls_instead_of_recalling_history_while_tool_approval_modal_is_open() {
        let mut app = App::new(None);
        app.mouse_enabled = true;
        app.on_event(&key(KeyCode::Char('a')));
        app.on_event(&key(KeyCode::Enter));
        assert!(app.input.is_empty());
        app.chat_overflow.set(30);

        open_tool_approval_modal(&mut app);

        app.on_event(&key(KeyCode::Up));
        assert_eq!(
            app.input, "",
            "modal must not let ↑ mutate input via history recall"
        );
        assert_eq!(
            app.history_index, None,
            "history browsing must not start while a modal is open"
        );
        assert_eq!(
            app.scroll_offset, 1,
            "↑ still scrolls the chat while a modal blocks history"
        );
    }

    #[test]
    fn plain_down_scrolls_instead_of_recalling_history_while_gate_modal_is_open() {
        let mut app = App::new(Some(spec()));
        app.mouse_enabled = true;
        app.on_event(&key(KeyCode::Char('a')));
        app.on_event(&key(KeyCode::Enter));
        app.chat_overflow.set(30);
        app.scroll_offset = 5;
        app.start_pass();
        assert!(app.gate_open);

        app.on_event(&key(KeyCode::Down));
        assert_eq!(app.input, "");
        assert_eq!(app.history_index, None);
        assert_eq!(
            app.scroll_offset, 4,
            "↓ still scrolls the chat while the gate modal blocks history"
        );
    }

    #[test]
    fn ctrl_arrows_still_jump_turns_while_a_modal_is_open() {
        let mut app = App::new(None);
        app.chat_overflow.set(30);
        *app.user_turn_rows.borrow_mut() = vec![2, 10, 25];
        open_tool_approval_modal(&mut app);

        app.on_event(&ctrl_key(KeyCode::Up));
        assert_eq!(
            app.scroll_offset, 5,
            "Ctrl+↑ keeps jumping turns even with a modal open"
        );
    }

    #[test]
    fn typing_slash_opens_the_palette_and_filters_by_prefix() {
        let mut app = App::new(None);
        app.on_event(&key(KeyCode::Char('/')));
        assert!(app.palette_visible());
        assert_eq!(app.palette_matches().len(), COMMANDS.len());
        app.on_event(&key(KeyCode::Char('s')));
        let names: Vec<_> = app.palette_matches().iter().map(|c| c.name).collect();
        assert_eq!(names, vec!["/sdd", "/spec"]);
        assert!(
            !App::new(None).palette_visible(),
            "input vide → pas de palette"
        );
    }

    #[test]
    fn palette_arrows_cycle_selection_and_reset_on_edit() {
        let mut app = App::new(None);
        app.on_event(&key(KeyCode::Char('/')));
        app.on_event(&key(KeyCode::Char('s')));
        assert_eq!(app.palette_selected, 0);
        app.on_event(&key(KeyCode::Down));
        assert_eq!(app.palette_selected, 1);
        app.on_event(&key(KeyCode::Down));
        assert_eq!(app.palette_selected, 0, "cyclique en bas");
        app.on_event(&key(KeyCode::Up));
        assert_eq!(app.palette_selected, 1, "cyclique en haut");
        app.on_event(&key(KeyCode::Char('d')));
        assert_eq!(
            app.palette_selected, 0,
            "l'édition resélectionne le premier"
        );
    }

    #[test]
    fn palette_enter_runs_the_selected_command_not_the_typed_text() {
        let mut app = App::new(None);
        // Recalling the executed command via ↑ below relies on prompt-history
        // recall, which is only bound to the bare arrows in mouse mode (see
        // the `KAJI_MOUSE` gating on the plain Up/Down arms) — without this,
        // Up would scroll the (empty) chat instead of recalling history.
        app.mouse_enabled = true;
        for c in "/th".chars() {
            app.on_event(&key(KeyCode::Char(c)));
        }
        assert_eq!(app.palette_matches()[0].name, "/think");
        let action = app.on_event(&key(KeyCode::Enter));
        assert_eq!(action, Action::None);
        assert!(app.show_thinking, "la sélection /think a bien été exécutée");
        assert!(app.input.is_empty());
        app.on_event(&key(KeyCode::Up));
        assert_eq!(
            app.input, "/think",
            "la commande exécutée entre dans l'historique"
        );
    }

    #[test]
    fn palette_enter_without_any_match_submits_the_text_as_is() {
        let mut app = App::new(None);
        for c in "/xyz".chars() {
            app.on_event(&key(KeyCode::Char(c)));
        }
        assert!(!app.palette_visible(), "aucun match → pas de palette");
        assert_eq!(
            app.on_event(&key(KeyCode::Enter)),
            Action::Submit("/xyz".to_string())
        );
    }

    #[test]
    fn palette_tab_completes_the_selected_name_without_running_it() {
        let mut app = App::new(None);
        for c in "/s".chars() {
            app.on_event(&key(KeyCode::Char(c)));
        }
        app.on_event(&key(KeyCode::Down));
        assert_eq!(app.on_event(&key(KeyCode::Tab)), Action::None);
        assert_eq!(app.input, "/spec");
        assert_eq!(app.driver, PassDriver::Idle, "rien n'a été exécuté");
    }

    #[test]
    fn palette_esc_clears_the_input_and_never_cancels_the_turn() {
        let mut app = App::new(None);
        app.turn_active = true;
        app.on_event(&key(KeyCode::Char('/')));
        assert_eq!(app.on_event(&key(KeyCode::Esc)), Action::None);
        assert!(app.input.is_empty());
        assert_eq!(
            app.on_event(&key(KeyCode::Esc)),
            Action::CancelTurn,
            "sans palette, Esc retrouve l'annulation du tour"
        );
    }

    #[test]
    fn palette_arrows_take_priority_over_history_but_ctrl_arrows_still_jump_turns() {
        let mut app = App::new(None);
        app.mouse_enabled = true;
        app.on_event(&key(KeyCode::Char('a')));
        app.on_event(&key(KeyCode::Enter));
        app.chat_overflow.set(30);
        *app.user_turn_rows.borrow_mut() = vec![2, 10, 25];
        app.on_event(&key(KeyCode::Char('/')));
        app.on_event(&key(KeyCode::Up));
        assert_eq!(
            app.input, "/",
            "↑ navigue la palette, ne rappelle pas l'historique"
        );
        assert_eq!(app.history_index, None);
        app.on_event(&ctrl_key(KeyCode::Up));
        assert_eq!(app.scroll_offset, 5, "Ctrl+↑ saute toujours les tours");
    }

    #[test]
    fn palette_is_inert_while_a_modal_is_open() {
        let mut app = App::new(None);
        open_tool_approval_modal(&mut app);
        app.input.push('/');
        assert!(!app.palette_visible());
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

    fn agent_thinks(app: &mut App, id: &str, text: &str) {
        let mut m = Message::assistant().with_thinking(text, "");
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
        app.chat_overflow.set(30);
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
        app.chat_overflow.set(30);
        assert_eq!(app.on_event(&key(KeyCode::PageUp)), Action::None);
        assert!(app.scroll_offset > 0);
        assert_eq!(app.on_event(&key(KeyCode::Home)), Action::None);
        assert!(app.scroll_offset > 0);
        assert_eq!(app.on_event(&key(KeyCode::End)), Action::None);
        assert_eq!(app.scroll_offset, 0);
    }

    #[test]
    fn arrow_keys_scroll_the_chat_line_by_line() {
        let mut app = App::new(None);
        app.chat_overflow.set(30);
        assert_eq!(app.on_event(&key(KeyCode::Up)), Action::None);
        assert_eq!(app.on_event(&key(KeyCode::Up)), Action::None);
        assert_eq!(app.scroll_offset, 2);
        assert_eq!(app.on_event(&key(KeyCode::Down)), Action::None);
        assert_eq!(app.scroll_offset, 1);
    }

    #[test]
    fn mouse_wheel_scrolls_three_lines() {
        let mut app = App::new(None);
        app.chat_overflow.set(10);

        app.on_event(&mouse_event(MouseEventKind::ScrollUp));
        assert_eq!(app.scroll_offset, 3);
        app.on_event(&mouse_event(MouseEventKind::ScrollUp));
        app.on_event(&mouse_event(MouseEventKind::ScrollUp));
        assert_eq!(app.scroll_offset, 9);

        // Bounded at chat_overflow — one more cran would overshoot by 2.
        app.on_event(&mouse_event(MouseEventKind::ScrollUp));
        assert_eq!(app.scroll_offset, 10);

        app.on_event(&mouse_event(MouseEventKind::ScrollDown));
        assert_eq!(app.scroll_offset, 7);
    }

    #[test]
    fn mouse_wheel_scroll_down_never_goes_negative() {
        let mut app = App::new(None);
        app.chat_overflow.set(10);
        app.scroll_offset = 1;

        app.on_event(&mouse_event(MouseEventKind::ScrollDown));
        assert_eq!(app.scroll_offset, 0);
    }

    #[test]
    fn mouse_wheel_is_a_noop_at_the_default_zero_overflow() {
        let mut app = App::new(None);
        assert_eq!(
            app.chat_overflow.get(),
            0,
            "fresh App starts with no overflow"
        );

        app.on_event(&mouse_event(MouseEventKind::ScrollUp));
        assert_eq!(app.scroll_offset, 0);
        app.on_event(&mouse_event(MouseEventKind::ScrollDown));
        assert_eq!(app.scroll_offset, 0);
    }

    #[test]
    fn ctrl_arrows_jump_between_user_turns() {
        let mut app = App::new(None);
        app.chat_overflow.set(30);
        *app.user_turn_rows.borrow_mut() = vec![2, 10, 25];

        app.on_event(&ctrl_key(KeyCode::Up));
        assert_eq!(
            app.scroll_offset, 5,
            "row 25 (the closest above the bottom) is aligned to the top: 30-25"
        );

        app.on_event(&ctrl_key(KeyCode::Up));
        assert_eq!(
            app.scroll_offset, 20,
            "next jump aligns row 10 to the top: 30-10"
        );

        app.on_event(&ctrl_key(KeyCode::Down));
        assert_eq!(
            app.scroll_offset, 5,
            "jumping back down realigns row 25: 30-25"
        );
    }

    #[test]
    fn ctrl_up_is_a_noop_once_at_the_topmost_user_turn() {
        let mut app = App::new(None);
        app.chat_overflow.set(30);
        *app.user_turn_rows.borrow_mut() = vec![2, 10, 25];
        app.scroll_offset = 30; // top row = 0, nothing above it

        app.on_event(&ctrl_key(KeyCode::Up));
        assert_eq!(app.scroll_offset, 30);
    }

    /// `jump_prev_turn` looks for a turn strictly above the current top row
    /// (`row < top`, not `row <= top`) — sitting exactly at the first
    /// turn's row must be a no-op rather than re-jumping to the same spot.
    /// Kills the `<` → `<=` mutant.
    #[test]
    fn ctrl_up_is_a_noop_when_top_row_exactly_matches_the_first_user_turn() {
        let mut app = App::new(None);
        app.chat_overflow.set(30);
        *app.user_turn_rows.borrow_mut() = vec![2, 10, 25];
        app.scroll_offset = 28; // top row = 30-28 = 2, exactly the first turn

        app.on_event(&ctrl_key(KeyCode::Up));
        assert_eq!(app.scroll_offset, 28);
    }

    #[test]
    fn ctrl_down_is_a_noop_when_no_turn_lies_below() {
        let mut app = App::new(None);
        app.chat_overflow.set(30);
        *app.user_turn_rows.borrow_mut() = vec![2, 10, 25];
        // top row = 30 (bottom of the chat), nothing recorded below it

        app.on_event(&ctrl_key(KeyCode::Down));
        assert_eq!(app.scroll_offset, 0);
    }

    #[test]
    fn ctrl_arrows_are_a_noop_with_no_recorded_user_turns() {
        let mut app = App::new(None);
        app.chat_overflow.set(30);

        app.on_event(&ctrl_key(KeyCode::Up));
        assert_eq!(app.scroll_offset, 0);
        app.on_event(&ctrl_key(KeyCode::Down));
        assert_eq!(app.scroll_offset, 0);
    }

    #[test]
    fn arrow_up_recalls_previous_prompt_when_input_empty() {
        let mut app = App::new(None);
        app.mouse_enabled = true;

        for c in "a".chars() {
            app.on_event(&key(KeyCode::Char(c)));
        }
        app.on_event(&key(KeyCode::Enter));
        for c in "b".chars() {
            app.on_event(&key(KeyCode::Char(c)));
        }
        app.on_event(&key(KeyCode::Enter));
        assert!(app.input.is_empty());

        app.on_event(&key(KeyCode::Up));
        assert_eq!(app.input, "b");
        app.on_event(&key(KeyCode::Up));
        assert_eq!(app.input, "a");
        app.on_event(&key(KeyCode::Down));
        assert_eq!(app.input, "b");
        app.on_event(&key(KeyCode::Down));
        assert_eq!(app.input, "", "past the latest entry clears the input");

        // Typing while browsing exits navigation but keeps the edit.
        app.on_event(&key(KeyCode::Up));
        assert_eq!(app.input, "b");
        app.on_event(&key(KeyCode::Char('!')));
        assert_eq!(app.input, "b!");
        app.on_event(&key(KeyCode::Up));
        assert_eq!(
            app.input, "b!",
            "non-empty input outside navigation does not recall history"
        );
    }

    /// `push_history` runs unconditionally on every non-empty submit
    /// (before the slash-command dispatch), so recalling a slash command
    /// must work exactly like recalling free text.
    #[test]
    fn arrow_up_recalls_a_slash_command() {
        let mut app = App::new(None);
        app.mouse_enabled = true;
        for c in "/help".chars() {
            app.on_event(&key(KeyCode::Char(c)));
        }
        assert_eq!(app.on_event(&key(KeyCode::Enter)), Action::Help);

        app.on_event(&key(KeyCode::Up));
        assert_eq!(app.input, "/help");
    }

    #[test]
    fn arrow_up_does_nothing_while_editing_a_fresh_non_empty_draft() {
        let mut app = App::new(None);
        app.mouse_enabled = true;
        app.on_event(&key(KeyCode::Char('a')));
        app.on_event(&key(KeyCode::Enter));

        app.on_event(&key(KeyCode::Char('x')));
        app.on_event(&key(KeyCode::Up));
        assert_eq!(app.input, "x");
    }

    #[test]
    fn backspace_during_history_navigation_exits_navigation_and_keeps_edit() {
        let mut app = App::new(None);
        app.mouse_enabled = true;
        app.on_event(&key(KeyCode::Char('a')));
        app.on_event(&key(KeyCode::Enter));

        app.on_event(&key(KeyCode::Up));
        assert_eq!(app.input, "a");
        app.on_event(&key(KeyCode::Backspace));
        assert_eq!(app.input, "");
        // No longer browsing — Up must not blow away the (empty) draft by
        // reusing whatever index browsing was left at; input-empty still
        // legitimately recalls though, so re-arm history and check it
        // starts from the latest entry again rather than a stale index.
        app.on_event(&key(KeyCode::Up));
        assert_eq!(app.input, "a");
    }

    /// HISTCONTROL=ignoredups: resubmitting a recalled prompt unedited (a
    /// common ↑, Enter loop while iterating on a message) must not grow the
    /// history with a duplicate entry right next to the original.
    #[test]
    fn push_history_dedups_consecutive_identical_submits() {
        let mut app = App::new(None);
        app.mouse_enabled = true;
        app.on_event(&key(KeyCode::Char('a')));
        app.on_event(&key(KeyCode::Enter));

        app.on_event(&key(KeyCode::Up));
        assert_eq!(app.input, "a");
        app.on_event(&key(KeyCode::Enter));

        assert_eq!(app.prompt_history, vec!["a".to_string()]);
    }

    #[test]
    fn push_history_keeps_non_consecutive_repeats() {
        let mut app = App::new(None);
        app.mouse_enabled = true;
        app.on_event(&key(KeyCode::Char('a')));
        app.on_event(&key(KeyCode::Enter));
        app.on_event(&key(KeyCode::Char('b')));
        app.on_event(&key(KeyCode::Enter));
        app.on_event(&key(KeyCode::Char('a')));
        app.on_event(&key(KeyCode::Enter));

        assert_eq!(
            app.prompt_history,
            vec!["a".to_string(), "b".to_string(), "a".to_string()],
            "only an immediately-adjacent repeat is deduped"
        );
    }

    #[test]
    fn plain_arrows_do_not_scroll_when_mouse_enabled() {
        let mut app = App::new(None);
        app.mouse_enabled = true;
        app.chat_overflow.set(30);

        app.on_event(&key(KeyCode::Up));
        app.on_event(&key(KeyCode::Up));
        assert_eq!(app.scroll_offset, 0);
        app.on_event(&key(KeyCode::Down));
        assert_eq!(app.scroll_offset, 0);
    }

    #[test]
    fn plain_arrows_still_scroll_when_mouse_disabled() {
        let mut app = App::new(None);
        assert!(!app.mouse_enabled, "mouse_enabled defaults to false");
        app.chat_overflow.set(30);

        app.on_event(&key(KeyCode::Up));
        assert_eq!(app.scroll_offset, 1);
    }

    #[test]
    fn scroll_is_bounded_by_measured_overflow_not_raw_line_count() {
        let mut app = App::new(None);
        app.push_system("one raw line");
        app.chat_overflow.set(25);
        app.scroll_page_up();
        app.scroll_page_up();
        app.scroll_page_up();
        assert_eq!(app.scroll_offset, 25);
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

    #[test]
    fn show_thinking_defaults_to_off() {
        let app = App::new(None);
        assert!(!app.show_thinking);
    }

    #[test]
    fn slash_think_toggles_thinking_and_pushes_system_message() {
        let mut app = App::new(None);
        for c in "/think".chars() {
            app.on_event(&key(KeyCode::Char(c)));
        }
        assert_eq!(app.on_event(&key(KeyCode::Enter)), Action::None);
        assert!(app.show_thinking);
        assert!(app
            .chat
            .iter()
            .any(|l| matches!(l.sender, Sender::System) && l.text.contains("思考中")));

        for c in "/think".chars() {
            app.on_event(&key(KeyCode::Char(c)));
        }
        app.on_event(&key(KeyCode::Enter));
        assert!(!app.show_thinking);
    }

    #[test]
    fn f3_toggles_thinking() {
        let mut app = App::new(None);
        assert!(!app.show_thinking);
        app.on_event(&key(KeyCode::F(3)));
        assert!(app.show_thinking);
        app.on_event(&key(KeyCode::F(3)));
        assert!(!app.show_thinking);
    }

    #[test]
    fn thinking_block_is_dropped_when_show_thinking_is_off() {
        let mut app = App::new(None);
        agent_thinks(&mut app, "m1", "raisonnement caché");
        assert!(!app
            .chat
            .iter()
            .any(|l| matches!(l.sender, Sender::Thinking)));
    }

    #[test]
    fn thinking_blocks_accumulate_into_one_chat_line_when_enabled() {
        let mut app = App::new(None);
        app.show_thinking = true;
        agent_thinks(&mut app, "m1", "étape un ");
        agent_thinks(&mut app, "m1", "étape deux");

        let thinking_lines: Vec<_> = app
            .chat
            .iter()
            .filter(|l| matches!(l.sender, Sender::Thinking))
            .collect();
        assert_eq!(thinking_lines.len(), 1);
        assert_eq!(thinking_lines[0].text, "étape un étape deux");
    }

    #[test]
    fn thinking_then_tool_then_thinking_same_id_keeps_lines_separate() {
        let mut app = App::new(None);
        app.show_thinking = true;
        agent_thinks(&mut app, "m1", "avant");
        let req_msg =
            Message::assistant().with_tool_request("t1", Ok(CallToolRequestParams::new("shell")));
        app.apply_agent_event(&AgentEvent::Message(req_msg));
        agent_thinks(&mut app, "m1", "après");

        let thinking_lines: Vec<_> = app
            .chat
            .iter()
            .filter(|l| matches!(l.sender, Sender::Thinking))
            .collect();
        assert_eq!(thinking_lines.len(), 2);
        assert_eq!(thinking_lines[0].text, "avant");
        assert_eq!(thinking_lines[1].text, "après");
    }

    /// Regression: `merge_agent_text`/`merge_agent_thinking` used to only
    /// invalidate their OWN merge-chain id on push, not the other one — so
    /// a same-id `Thinking → Text → Thinking` interleave left
    /// `last_thinking_msg_id` still pointing at the (now stale) first
    /// thinking chunk's id, and the 3rd chunk merged onto `chat.last_mut()`
    /// which by then was the TEXT line, leaking reasoning into the visible
    /// reply under the normal agent style instead of getting its own dim
    /// `思` line.
    #[test]
    fn thinking_text_thinking_same_id_keeps_three_separate_lines_with_correct_styles() {
        let mut app = App::new(None);
        app.show_thinking = true;
        agent_thinks(&mut app, "m1", "réflexion 1");
        agent_says(&mut app, "m1", "réponse");
        agent_thinks(&mut app, "m1", "réflexion 2");

        assert_eq!(app.chat.len(), 3);
        assert_eq!(app.chat[0].sender, Sender::Thinking);
        assert_eq!(app.chat[0].text, "réflexion 1");
        assert_eq!(app.chat[1].sender, Sender::Agent);
        assert_eq!(app.chat[1].text, "réponse");
        assert_eq!(app.chat[2].sender, Sender::Thinking);
        assert_eq!(app.chat[2].text, "réflexion 2");
    }

    /// Mirror of the above: `Text → Thinking → Text` under the same id must
    /// not let the 3rd chunk (visible reply) merge onto the thinking line —
    /// that would swallow part of the answer into the dim/italic register.
    #[test]
    fn text_thinking_text_same_id_keeps_three_separate_lines_with_correct_styles() {
        let mut app = App::new(None);
        app.show_thinking = true;
        agent_says(&mut app, "m1", "réponse 1");
        agent_thinks(&mut app, "m1", "réflexion");
        agent_says(&mut app, "m1", "réponse 2");

        assert_eq!(app.chat.len(), 3);
        assert_eq!(app.chat[0].sender, Sender::Agent);
        assert_eq!(app.chat[0].text, "réponse 1");
        assert_eq!(app.chat[1].sender, Sender::Thinking);
        assert_eq!(app.chat[1].text, "réflexion");
        assert_eq!(app.chat[2].sender, Sender::Agent);
        assert_eq!(app.chat[2].text, "réponse 2");
    }

    #[test]
    fn thinking_does_not_mark_turn_output_visible() {
        let mut app = App::new(None);
        app.show_thinking = true;
        agent_thinks(&mut app, "m1", "raisonnement");
        assert!(!app.turn_has_visible_output);
    }

    #[test]
    fn text_block_marks_turn_output_visible() {
        let mut app = App::new(None);
        assert!(!app.turn_has_visible_output);
        agent_says(&mut app, "m1", "bonjour");
        assert!(app.turn_has_visible_output);
    }

    #[test]
    fn reset_turn_visibility_clears_output_flag_and_thinking_merge_state() {
        let mut app = App::new(None);
        app.show_thinking = true;
        agent_says(&mut app, "m1", "bonjour");
        agent_thinks(&mut app, "m1", "raisonnement");
        assert!(app.turn_has_visible_output);
        assert!(app.turn_thinking_visible());

        app.reset_turn_visibility();
        assert!(!app.turn_has_visible_output);
        assert!(!app.turn_thinking_visible());
    }

    #[test]
    fn loader_visibility_matrix() {
        let mut app = App::new(None);
        assert!(!app.show_loader(), "idle: no turn in flight");

        app.turn_pending = true;
        assert!(
            app.show_loader(),
            "setup pending, nothing visible yet → loader"
        );

        app.turn_pending = false;
        app.turn_active = true;
        assert!(
            app.show_loader(),
            "turn active, nothing visible yet → loader"
        );

        app.turn_has_visible_output = true;
        assert!(
            !app.show_loader(),
            "first visible Text chunk hides the loader"
        );

        app.turn_has_visible_output = false;
        app.show_thinking = true;
        assert!(
            app.show_loader(),
            "thinking ON but nothing displayed yet → loader still shows"
        );

        agent_thinks(&mut app, "m1", "raisonnement…");
        assert!(
            !app.show_loader(),
            "thinking ON and already displaying this turn hides the loader"
        );
    }

    /// Regression: toggling `/think` OFF mid-turn, after a thinking line
    /// already rendered but before any visible `Text` chunk, used to bring
    /// the loader back — `turn_thinking_visible()` was gated on the
    /// *current* value of `show_thinking`, not on whether a thinking line
    /// is actually sitting in the chat. The ensō would reappear underneath
    /// an already-visible 思 line.
    #[test]
    fn loader_stays_hidden_after_toggling_thinking_off_mid_turn_once_thinking_was_shown() {
        let mut app = App::new(None);
        app.turn_active = true;
        app.show_thinking = true;
        agent_thinks(&mut app, "m1", "raisonnement");
        assert!(
            !app.show_loader(),
            "thinking visible this turn hides the loader"
        );

        app.show_thinking = false;
        assert!(
            !app.show_loader(),
            "loader must not reappear under an already-visible thinking line just because the toggle flipped off"
        );
    }

    #[test]
    fn streaming_agent_line_is_none_while_idle() {
        let app = App::new(None);
        assert_eq!(app.streaming_agent_line(), None);
    }

    #[test]
    fn streaming_agent_line_tracks_the_open_agent_text_chain() {
        let mut app = App::new(None);
        app.turn_active = true;
        agent_says(&mut app, "m1", "Bon");
        let idx = app
            .streaming_agent_line()
            .expect("first text chunk arms the ninja cursor");
        assert_eq!(idx, app.chat.len() - 1);

        agent_says(&mut app, "m1", "jour");
        assert_eq!(
            app.streaming_agent_line(),
            Some(idx),
            "a same-id chunk keeps pointing at the same merged line"
        );
    }

    #[test]
    fn streaming_agent_line_is_none_when_turn_is_not_active() {
        let mut app = App::new(None);
        agent_says(&mut app, "m1", "réponse");
        assert!(matches!(
            app.chat.last().map(|l| l.sender),
            Some(Sender::Agent)
        ));
        assert_eq!(
            app.streaming_agent_line(),
            None,
            "no ninja cursor outside an active turn, even with an agent line present"
        );
    }

    #[test]
    fn streaming_agent_line_breaks_when_a_tool_request_interrupts_the_chain() {
        let mut app = App::new(None);
        app.turn_active = true;
        agent_says(&mut app, "m1", "je lance un outil");
        assert!(app.streaming_agent_line().is_some());

        let req_msg =
            Message::assistant().with_tool_request("t1", Ok(CallToolRequestParams::new("shell")));
        app.apply_agent_event(&AgentEvent::Message(req_msg));

        assert_eq!(
            app.streaming_agent_line(),
            None,
            "a tool line breaks the streaming chain"
        );
    }

    #[test]
    fn streaming_agent_line_never_points_at_a_thinking_line() {
        let mut app = App::new(None);
        app.turn_active = true;
        app.show_thinking = true;
        agent_thinks(&mut app, "m1", "raisonnement");

        assert!(matches!(
            app.chat.last().map(|l| l.sender),
            Some(Sender::Thinking)
        ));
        assert_eq!(
            app.streaming_agent_line(),
            None,
            "thinking lines never carry the ninja cursor, only agent text"
        );
    }

    #[test]
    fn streaming_agent_line_resets_when_a_new_turn_begins_without_a_separating_chat_line() {
        let mut app = App::new(None);
        app.turn_active = true;
        agent_says(&mut app, "m1", "réponse finale");
        assert_eq!(app.streaming_agent_line(), Some(app.chat.len() - 1));

        // Mirrors the SDD gate→exec→validate auto-chain: `turn_end()` flips
        // `turn_active` off, and for the Executing→Validating branch the
        // next turn's `begin_setup` (via `reset_turn_visibility`) starts
        // with no intervening system/user chat line to otherwise break the
        // chain. A stale `agent_stream_idx` must not leak into the new turn.
        app.turn_active = false;
        app.reset_turn_visibility();
        app.turn_active = true;

        assert_eq!(
            app.streaming_agent_line(),
            None,
            "a fresh turn must not inherit the previous turn's open chain just because no chat line separated them"
        );
    }

    #[test]
    fn loader_and_streaming_blade_are_mutually_exclusive_across_turn_lifecycle() {
        let mut app = App::new(None);
        assert!(!(app.show_loader() && app.streaming_agent_line().is_some()));

        app.turn_pending = true;
        assert!(!(app.show_loader() && app.streaming_agent_line().is_some()));

        app.turn_pending = false;
        app.turn_active = true;
        assert!(!(app.show_loader() && app.streaming_agent_line().is_some()));

        agent_says(&mut app, "m1", "bonjour");
        assert!(
            app.streaming_agent_line().is_some() && !app.show_loader(),
            "first visible text hides the loader and arms the ninja cursor"
        );

        app.show_thinking = true;
        agent_thinks(&mut app, "m1", "raisonnement");
        assert!(
            app.streaming_agent_line().is_none(),
            "thinking never carries the blade, even mid-turn"
        );
        assert!(
            !app.show_loader(),
            "text was already shown this turn — no loader reappears under the thinking line either"
        );
    }

    #[test]
    fn every_command_in_the_table_dispatches_without_falling_through_to_submit() {
        for cmd in COMMANDS {
            let mut app = App::new(None);
            for c in cmd.name.chars() {
                app.on_event(&key(KeyCode::Char(c)));
            }
            let action = app.on_event(&key(KeyCode::Enter));
            assert!(
                !matches!(action, Action::Submit(_)),
                "{} doit être dispatché comme commande, pas soumis comme message",
                cmd.name
            );
        }
    }
}
