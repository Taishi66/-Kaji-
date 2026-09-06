use crate::tui::editors::{self, EditMode, EditorSpec, EditorState, Launch, LaunchContext};
use crate::tui::gitstatus::GitStatus;
use crate::tui::icons::IconSet;
use crate::tui::report;
use crate::tui::theme::SpanRole;
use crate::tui::ui::sanitize_for_display;
use crate::tui::{diff, forge, gitstatus, missioncontrol, theme};
use kaji::agents::{AgentEvent, SUBAGENT_TOOL_REQUEST_TYPE};
use kaji::config::KajiMode;
use kaji::conversation::message::{
    ActionRequiredData, Message, MessageContentBlock, SystemNotificationType,
};
use kaji::conversation::Conversation;
use kaji::permission::grants::{derive_grant_spec, is_shell_tool};
use kaji::permission::Permission;
use kaji::providers::base::ProviderUsage;
use kaji_core::goal::{self, GoalOutcome, GoalPhase, GoalState, GoalStep, Verdict};
use kaji_core::sdd::{SddPass, SpecDoc};
use ratatui::crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind,
};
use ratatui::layout::Rect;
use rmcp::model::{JsonObject, Role, ServerNotification};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io::Read;
use std::time::{Duration, Instant};

const SCROLL_PAGE: u16 = 10;
const SCROLL_WHEEL: u16 = 3;

/// Combien de temps le sceau garde son mot déplié — le temps de le lire au
/// démarrage et après un Shift+Tab, pas plus : la barre revient au silence.
const SEAL_UNFOLD: Duration = Duration::from_secs(4);

/// Rows the file finder ever paints, however many paths matched — the list is
/// scanned with the eyes, not scrolled through by the thousand.
const FINDER_MAX_RESULTS: usize = 200;

/// Which pane owns the keyboard. The viewer and the explorer (task 9) are
/// full-fledged focus targets rather than modal overlays: the chat keeps
/// streaming underneath, and only the key routing changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Focus {
    #[default]
    Composer,
    Viewer,
    Explorer,
    Forge,
}

/// Fuzzy file finder (`Ctrl+P`, `/files`) — telescope's gesture: a query line,
/// a ranked list, and three exits (open, attach, cancel).
#[derive(Debug, Default)]
pub struct FinderState {
    pub query: String,
    /// Insertion point in `query`, counted in chars — `draw_finder` puts the
    /// terminal cursor there and ←/→ move it.
    pub cursor: usize,
    pub selected: usize,
    /// Project-relative paths, best first, capped at [`FINDER_MAX_RESULTS`].
    pub results: Vec<String>,
}

/// Sélecteur de thème (`/theme`) — chaque déplacement applique la palette
/// pour de bon : l'écran entier est l'aperçu, il n'y a rien à prévisualiser à
/// côté. Les deux champs sont des rangs dans [`theme::THEMES`].
#[derive(Debug)]
pub struct ThemePicker {
    pub selected: usize,
    /// Palette active à l'ouverture : Esc la remet, et la liste la marque
    /// `(actuel)`.
    pub initial: usize,
}

/// Une ligne du sélecteur d'éditeur (`/editor`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditorRow {
    /// Un éditeur du catalogue trouvé sur le `PATH`.
    Detected(&'static EditorSpec),
    /// `$VISUAL`/`$EDITOR` : la choisir efface `KAJI_EDITOR` et rend la main à
    /// l'environnement.
    Env(String),
}

impl EditorRow {
    pub fn name(&self) -> &str {
        match self {
            Self::Detected(spec) => spec.id,
            Self::Env(_) => "(env)",
        }
    }

    /// Ce que la ligne montre à droite de son nom — la commande complète quand
    /// elle dit plus que l'identifiant, sinon rien.
    pub fn detail(&self) -> Option<String> {
        match self {
            Self::Detected(spec) => {
                let command = spec.command();
                (command != spec.id).then_some(command)
            }
            Self::Env(label) => Some(label.clone()),
        }
    }
}

/// Sélecteur d'éditeur (`/editor`) — contrairement au thème il n'y a rien à
/// prévisualiser : se déplacer ne change rien, seul Enter choisit.
#[derive(Debug)]
pub struct EditorPicker {
    pub selected: usize,
    pub rows: Vec<EditorRow>,
    /// Rang de la résolution courante, marqué `(actuel)`. `None` quand
    /// `KAJI_EDITOR` désigne une commande qui n'est dans aucune ligne.
    pub current: Option<usize>,
}

/// Hard ceiling on what the Tab detail panel reads back from disk to diff a
/// `write` against the file it would replace. The panel is a preview, not a
/// file viewer: past this the diff is cut rather than pulling a whole
/// multi-megabyte file into the draw path.
const DETAIL_READ_LIMIT: u64 = 256 * 1024;

/// Ceiling on the detail text handed to the renderer. `ui::truncate_for_modal`
/// clips it again to the modal's real geometry; this one keeps the string that
/// reaches it (and the wrap measurement it runs every frame) bounded.
const DETAIL_MAX_CHARS: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PassDriver {
    Idle,
    AwaitingGate,
    Executing,
    Validating,
}

/// Sibling of [`PassDriver`] for goal sessions (item 5 ante), derived from
/// `App::goal` rather than stored: the phase lives in the core `GoalState`,
/// and a second copy in the TUI would be one more thing to keep in sync.
/// `Idle` covers both "no goal" and "goal finished" — the two states in which
/// `turn_end` must not chain another turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoalDriver {
    Idle,
    Working,
    Evaluating,
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

/// Un span de bloc pré-rendu : son texte et son rôle sémantique, jamais un
/// `Style` — la couleur est résolue à chaque frame par
/// `ui::push_rendered_lines`, donc un `/theme` en session re-colore les blocs
/// déjà poussés.
#[derive(Debug, Clone, PartialEq)]
pub struct RoledSpan {
    pub text: String,
    pub role: SpanRole,
}

/// Une ligne pré-rendue : ses spans, dans l'ordre.
pub type RoledLine = Vec<RoledSpan>;

/// Un constructeur par rôle, nommé comme la fonction de style que
/// `theme::style` lui associe : les producteurs de blocs écrivent le même
/// vocabulaire qu'avec `Span::styled(.., theme::xxx())`, sans résoudre de
/// couleur.
impl RoledSpan {
    pub fn new(text: impl Into<String>, role: SpanRole) -> Self {
        Self {
            text: text.into(),
            role,
        }
    }

    pub fn plain(text: impl Into<String>) -> Self {
        Self::new(text, SpanRole::Plain)
    }

    pub fn text(text: impl Into<String>) -> Self {
        Self::new(text, SpanRole::Text)
    }

    pub fn dim(text: impl Into<String>) -> Self {
        Self::new(text, SpanRole::Dim)
    }

    pub fn system(text: impl Into<String>) -> Self {
        Self::new(text, SpanRole::System)
    }

    pub fn accent(text: impl Into<String>) -> Self {
        Self::new(text, SpanRole::Accent)
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self::new(text, SpanRole::Error)
    }

    pub fn title(text: impl Into<String>) -> Self {
        Self::new(text, SpanRole::Title)
    }

    pub fn table_header(text: impl Into<String>) -> Self {
        Self::new(text, SpanRole::TableHeader)
    }

    pub fn border_inactive(text: impl Into<String>) -> Self {
        Self::new(text, SpanRole::BorderInactive)
    }

    pub fn gold(text: impl Into<String>) -> Self {
        Self::new(text, SpanRole::Gold)
    }
}

#[derive(Debug, Clone)]
pub struct ChatLine {
    pub sender: Sender,
    pub text: String,
    pub tool: Option<ToolLineState>,
    /// System lines only: pre-rendered blocks (aligned tables) whose spans
    /// carry a semantic role instead of the plain dim-italic style — used for
    /// on-demand report blocks (`/cost`, `/docker`). `text` still carries the
    /// flattened plain-text equivalent for scroll-height accounting.
    pub rendered: Option<Vec<RoledLine>>,
}

#[derive(Debug, Clone)]
pub struct ToolApprovalRequest {
    pub id: String,
    pub tool_name: String,
    /// The call's own arguments — what the `s`/`a` answers derive their grant
    /// from, and what the Tab detail panel renders.
    pub arguments: JsonObject,
    pub prompt: Option<String>,
}

impl ToolApprovalRequest {
    /// What `s`/`a` would actually persist, in the same serialized form the
    /// permission lists store — derived by the core, never re-derived here.
    pub fn grant_label(&self) -> String {
        match derive_grant_spec(&self.tool_name, Some(&self.arguments)) {
            Some(spec) => spec.to_string(),
            None => format!("tout l'outil {}", self.tool_name),
        }
    }

    /// The Tab panel's body: the full command for a shell call, a line diff for
    /// an edit or an overwrite, the raw arguments for anything else.
    /// `working_dir` is the session's, the same base the tool itself writes
    /// against — see [`App::set_working_dir`].
    pub fn detail_text(&self, working_dir: &std::path::Path) -> String {
        let base = self.tool_base_name();
        let text = if is_shell_tool(&self.tool_name) {
            self.string_arg("command").map(str::to_string)
        } else if base == "edit" {
            self.string_arg("before")
                .zip(self.string_arg("after"))
                .map(|(before, after)| diff::line_diff(before, after).join("\n"))
        } else if base == "write" {
            self.string_arg("path")
                .zip(self.string_arg("content"))
                .map(|(path, content)| write_preview(path, content, working_dir))
        } else {
            None
        };
        cap_detail(text.unwrap_or_else(|| self.pretty_arguments()))
    }

    /// The developer extension is registered unprefixed in-process and prefixed
    /// over MCP — both spellings name the same tool.
    fn tool_base_name(&self) -> &str {
        self.tool_name
            .strip_prefix("developer__")
            .unwrap_or(&self.tool_name)
    }

    fn string_arg(&self, key: &str) -> Option<&str> {
        self.arguments.get(key)?.as_str()
    }

    fn pretty_arguments(&self) -> String {
        serde_json::to_string_pretty(&self.arguments).unwrap_or_default()
    }
}

fn write_preview(path: &str, content: &str, working_dir: &std::path::Path) -> String {
    match read_bounded(&crate::tui::mentions::resolve(path, working_dir)) {
        Some(existing) => diff::line_diff(&existing, content).join("\n"),
        None => format!("nouveau fichier {path}\n{content}"),
    }
}

/// `None` for anything that isn't a readable file — a `write` to a new path,
/// which the caller renders as a creation rather than a diff.
fn read_bounded(path: &std::path::Path) -> Option<String> {
    let mut buffer = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(DETAIL_READ_LIMIT)
        .read_to_end(&mut buffer)
        .ok()?;
    Some(String::from_utf8_lossy(&buffer).into_owned())
}

fn cap_detail(text: String) -> String {
    let total = text.chars().count();
    if total <= DETAIL_MAX_CHARS {
        return text;
    }
    let head: String = text.chars().take(DETAIL_MAX_CHARS).collect();
    format!("{head}… (+{} car.)", total - DETAIL_MAX_CHARS)
}

/// `Chat` is deliberately outside the cycle: Shift+Tab ramps how much the agent
/// may do on its own, and a mode that refuses every tool call would read as a
/// freeze. Landing on it from elsewhere still steps back to `Approve`.
fn next_kaji_mode(mode: KajiMode) -> KajiMode {
    match mode {
        KajiMode::Approve => KajiMode::SmartApprove,
        KajiMode::SmartApprove => KajiMode::Auto,
        KajiMode::Auto | KajiMode::Chat => KajiMode::Approve,
    }
}

pub fn kaji_mode_badge(mode: KajiMode) -> &'static str {
    match mode {
        KajiMode::Auto => "auto",
        KajiMode::Approve => "approve",
        KajiMode::SmartApprove => "smart",
        KajiMode::Chat => "chat",
    }
}

/// Ce que le mode autorise, en une ligne — poussée au démarrage et à chaque
/// Shift+Tab, seule source du libellé pour les deux.
pub fn mode_line(mode: KajiMode) -> String {
    let promise = match mode {
        KajiMode::Approve => "kaji demande avant chaque outil",
        KajiMode::SmartApprove => "kaji demande pour les outils risqués",
        KajiMode::Auto => "kaji agit sans demander",
        KajiMode::Chat => "aucun outil",
    };
    format!(
        "mode : {} {} — {promise}",
        kaji_mode_badge(mode),
        kaji_mode_seal(mode)
    )
}

/// The kanji the status bar's seal carries, and the colour the bar gives it
/// (`statusbar::mode_color`), are what say which mode is running.
pub fn kaji_mode_seal(mode: KajiMode) -> &'static str {
    match mode {
        KajiMode::Auto => "自",
        KajiMode::Approve => "承",
        KajiMode::SmartApprove => "智",
        KajiMode::Chat => "話",
    }
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
    /// The four approval answers, each carrying the permission the event loop
    /// hands to `Agent::handle_confirmation` verbatim.
    ToolAnswer(Permission),
    /// Shift+Tab already moved `App::kaji_mode` — the event loop applies the
    /// carried mode to the agent and persists it.
    Mode(KajiMode),
    Help,
    /// `/cost` seul ou `/cost <vue>` — la vue est déjà résolue, l'event loop
    /// n'a plus qu'à interroger le ledger.
    Cost(report::CostView),
    Context,
    Docker,
    Checkpoints,
    /// Parsed `/restore <id>` — carries the checkpoint id, not yet
    /// confirmed. `event_loop` opens the y/n modal on receipt; the modal
    /// itself resolves to `RestoreConfirm`/`RestoreCancel`.
    Restore(String),
    RestoreConfirm,
    RestoreCancel,
    /// `y` sur la question d'annulation d'une lame — l'id attend dans
    /// `pending_forge_cancel`, seul l'event loop peut atteindre le summon.
    ForgeCancel,
    /// `Ctrl+S` — flush queued steer messages into the running turn.
    SteerNow,
    /// Parsed `/goal <condition>` — the event loop asks [`App::goal_set`] for
    /// the first work prompt and starts the turn with it.
    GoalSet(String),
    GoalStatus,
    /// `/goal clear` — the goal is already stopped by [`App::goal_clear`];
    /// the event loop only still has to cancel the turn it was driving.
    GoalClear,
    /// `e` on the viewer/explorer or `/edit <chemin>` — carries the absolute
    /// path to open and, when the caller knows it, the line to land on. Only
    /// the event loop can honor it: suspending the TUI for the editor needs
    /// the terminal and the input thread, neither of which `App` owns.
    EditFile {
        path: std::path::PathBuf,
        line: Option<usize>,
    },
    /// Éditeur déjà choisi pour la session par [`App::run_editor_command`] —
    /// la boucle d'événements n'a plus qu'à l'écrire dans `KAJI_EDITOR`.
    Editor(String),
    /// `/editor reset` : `KAJI_EDITOR` sort de la config, l'environnement et la
    /// détection reprennent la main.
    EditorReset,
    /// `/editor mode <valeur>` — `App` a déjà appliqué le mode à la session,
    /// l'event loop n'a plus qu'à le persister dans `KAJI_EDIT_MODE`.
    EditMode(String),
    /// Palette already switched by [`App::run_theme_command`] — carries the
    /// applied name so the event loop can persist it to `KAJI_THEME`. The
    /// switch itself must not wait on the config write: the redraw right
    /// after this event is what the user is looking at.
    Theme(String),
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
        name: "/goal",
        desc: "goal session — /goal <condition> lance la boucle évaluée, /goal seul le statut, /goal clear arrête",
        run: |_| Action::GoalStatus,
    },
    Command {
        name: "/files",
        desc: "(ou Ctrl+P) recherche floue de fichiers — ⏎ ouvre le lecteur, Tab attache @",
        run: |app| {
            app.open_finder();
            Action::None
        },
    },
    Command {
        name: "/explorer",
        desc: "(ou Ctrl+E) explorateur de fichiers — j/k naviguer, ⏎ ouvrir, a attacher @",
        run: |app| {
            app.toggle_explorer();
            Action::None
        },
    },
    Command {
        name: "/forge",
        desc: "(ou Ctrl+F) volet forge — ↑/↓ choisir, ⏎ la fiche, x annuler, f (ou /forge full) plein écran",
        run: |app| {
            app.toggle_forge();
            Action::None
        },
    },
    Command {
        name: "/edit",
        desc: "éditer un fichier — /edit <chemin>[:ligne], ou e depuis le lecteur/explorateur",
        run: |app| app.run_edit_command(""),
    },
    Command {
        name: "/editor",
        desc: "choisir l'éditeur (détectés) — /editor <cmd> · reset · mode <auto|suspend|remote|pane|gui>",
        run: |app| app.run_editor_command(""),
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
        desc: "usage tokens/coût — `/cost [modèles|jour|semaine|mois|cache|projection]`, budgets via KAJI_BUDGET_5H / KAJI_BUDGET_7J / KAJI_BUDGET_MONTHLY_USD",
        run: |_| Action::Cost(report::CostView::Windows),
    },
    Command {
        name: "/context",
        desc: "répartition du contexte par catégorie",
        run: |_| Action::Context,
    },
    Command {
        name: "/docker",
        desc: "liste les conteneurs en cours",
        run: |_| Action::Docker,
    },
    Command {
        name: "/checkpoints",
        desc: "liste les snapshots pris avant chaque tour",
        run: |_| Action::Checkpoints,
    },
    Command {
        name: "/theme",
        desc: "choisir un thème (aperçu en direct) — /theme <nom> · next",
        run: |app| app.run_theme_command(""),
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

/// Parses a trimmed input line as an invocation of the slash command
/// `command`, returning its (possibly empty) argument — `None` when the
/// input names another command.
///
/// `strip_prefix("/restore")` alone would also match a hypothetical future
/// `/restorepoint` as `"point"` — checking the remainder starts with a space
/// (or is empty) keeps the command a whole word.
fn slash_command_arg<'a>(text: &'a str, command: &str) -> Option<&'a str> {
    let rest = text.strip_prefix(command)?;
    if rest.is_empty() || rest.starts_with(' ') {
        Some(rest.trim())
    } else {
        None
    }
}

/// Deliberately NOT in `COMMANDS`: that table only holds arg-less commands
/// the palette can autocomplete and run standalone (`Command::run` takes no
/// argument), while `/restore` always needs an id.
fn restore_command_arg(text: &str) -> Option<&str> {
    slash_command_arg(text, "/restore")
}

/// `/theme` alone IS in `COMMANDS` (arg-less = sélecteur), so it also reaches
/// [`App::run_theme_command`] through the palette.
fn theme_command_arg(text: &str) -> Option<&str> {
    slash_command_arg(text, "/theme")
}

/// `/goal` alone IS in `COMMANDS` (arg-less = status). The agent core has its
/// own `/goal` self-nudge command, so this match is also what keeps a goal
/// session from being forwarded to it as a plain message.
fn goal_command_arg(text: &str) -> Option<&str> {
    slash_command_arg(text, "/goal")
}

/// `/cost` seul EST dans `COMMANDS` (sans arg = les fenêtres session/5 h/7 j),
/// donc la palette l'autocomplète ; avec une vue il atterrit ici.
fn cost_command_arg(text: &str) -> Option<&str> {
    slash_command_arg(text, "/cost")
}

/// `/edit` alone IS in `COMMANDS` (arg-less = son usage), so the palette can
/// autocomplete it; with a path it lands here.
fn edit_command_arg(text: &str) -> Option<&str> {
    slash_command_arg(text, "/edit")
}

/// `/editor` alone IS in `COMMANDS` (arg-less = sélecteur). Le mot entier
/// exigé par [`slash_command_arg`] est ce qui l'empêche d'être lu comme un
/// `/edit` dont le chemin serait `or`.
fn editor_command_arg(text: &str) -> Option<&str> {
    slash_command_arg(text, "/editor")
}

/// `/forge` seul EST dans `COMMANDS` (sans argument = le volet), donc la
/// palette le complète ; `/forge full` atterrit ici.
fn forge_command_arg(text: &str) -> Option<&str> {
    slash_command_arg(text, "/forge")
}

/// `/editor mode` est un sous-mot de `/editor <cmd>`, pas une commande à
/// part : même garde de mot entier que [`slash_command_arg`], pour qu'un
/// binaire réellement nommé `modeline` ne soit pas lu comme `mode line`.
/// Insensible à la casse comme `list`/`reset` (`/editor Mode pane` marche).
fn editor_mode_arg(arg: &str) -> Option<&str> {
    let (head, rest) = arg.split_once(' ').unwrap_or((arg, ""));
    head.eq_ignore_ascii_case("mode").then(|| rest.trim())
}

/// A pasted block lands on one line: the composer and the finder query are
/// both single-line, and dropping the newlines outright would glue the last
/// word of one line onto the first of the next.
fn flatten_newlines(text: &str) -> String {
    text.replace("\r\n", " ").replace(['\r', '\n'], " ")
}

/// Byte offset of the `index`-th char — `String::insert_str` and
/// `String::remove` take byte offsets and panic off a char boundary.
fn char_boundary(text: &str, index: usize) -> usize {
    text.char_indices()
        .nth(index)
        .map_or(text.len(), |(at, _)| at)
}

/// Stable vocabulary for the `goal_end` event payload — the labels the UI
/// shows are French and free to change, an event log's is not.
fn outcome_token(outcome: GoalOutcome) -> &'static str {
    match outcome {
        GoalOutcome::Met => "met",
        GoalOutcome::Unreachable => "unreachable",
        GoalOutcome::Cleared => "cleared",
        GoalOutcome::Interrupted => "interrupted",
        GoalOutcome::IterationCap => "iteration_cap",
    }
}

pub struct App {
    pub header: String,
    /// Model name shown by the status bar's telemetry — set once at startup by
    /// the event loop, empty everywhere else (tests, `App::new`).
    pub model: String,
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
    /// Last snapshot delivered by the background read (task 15) — `None` until
    /// the first one lands, and again whenever `working_dir` is not a
    /// repository. Never computed here: `App` must not shell out to git on the
    /// draw path.
    pub git_status: Option<GitStatus>,
    /// A read is in flight: no second one is requested until it answers, so a
    /// slow repository can't pile up blocking tasks behind the 5 s tick.
    git_refresh_inflight: bool,
    /// Picked up by `event_loop` (`take_git_refresh_request`), same hand-off as
    /// `pending_index_request`.
    pending_git_request: bool,
    pub spec: Option<SpecDoc>,
    pub pass: SddPass,
    pub gate_open: bool,
    pub tool_approval: Option<ToolApprovalRequest>,
    /// `Some` while Tab has the detail panel open on the current approval —
    /// built once per toggle rather than per frame, since a `write` preview
    /// reads the file it would replace. Cleared with the approval itself.
    pub approval_detail: Option<String>,
    /// Mirrors the agent's mode for the header badge, and is what Shift+Tab
    /// cycles: the badge must update on the redraw that follows the keypress,
    /// not after the agent round-trip the event loop then performs.
    pub kaji_mode: KajiMode,
    /// Jusqu'à quand le sceau de la barre d'état déplie le mot du mode —
    /// `None` au repos. Posé par [`App::unfold_seal`] au démarrage et à chaque
    /// changement de mode.
    seal_unfolded_until: Option<Instant>,
    /// `KAJI_ICONS` — env > config, défaut `Nerd`. Posé par `event_loop` au
    /// démarrage : `App::new` ne lit pas la config.
    pub icons: IconSet,
    /// Checkpoint id awaiting y/n confirmation from `/restore <id>` — mirrors
    /// `tool_approval`'s shape (an `Option` carrying the payload the modal
    /// needs, taken by `event_loop` rather than cleared inline in
    /// `on_event`) rather than `gate_open`'s bare bool, since a restore needs
    /// to remember *which* checkpoint it's confirming.
    pub pending_restore: Option<String>,
    /// `true` when the checkpoint awaiting confirmation is a pre-restore
    /// safety net (captured "pre_restore") — such a restore is files-only by
    /// construction, which the confirm modal and the success message must
    /// say honestly.
    pub pending_restore_files_only: bool,
    /// Lame dont l'annulation attend son `y` — même forme que
    /// [`Self::pending_restore`] : l'id est porté jusqu'à l'event loop, seul
    /// endroit d'où le summon s'atteint.
    pub pending_forge_cancel: Option<String>,
    /// Id de la lame dont le lecteur montre la fiche. C'est l'id qui fait
    /// autorité, jamais le titre : deux délégations d'un même fan-out portent
    /// la même description, et le premier snapshot renomme celles qu'une
    /// notification d'outil avait nommées par leur id.
    forge_sheet_open: Option<String>,
    pub driver: PassDriver,
    /// Goal session (item 5 ante) — kept after it ends so `/goal` can still
    /// report the last outcome. [`App::goal_driver`] is what says whether it
    /// is still driving turns.
    pub goal: Option<GoalState>,
    /// The evaluator turn's assistant text, as `validate_buffer` is for the
    /// SDD judge — the verdict is read from it at turn end.
    goal_buffer: String,
    /// `(kind, payload_json)` pairs awaiting `SessionManager::append_event`,
    /// drained by the event loop: `App` has no session handle and must never
    /// block a keystroke on a write.
    goal_events: Vec<(&'static str, String)>,
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
    /// Next-prompt suggestion generated off the critical path after a turn
    /// ends (item 7 — ante "dim ghost text, acceptable with Tab"). `Some`
    /// only when a suggestion is ready to show; rendered as dim ghost text in
    /// the empty input line, Tab accepts it into `input`. Cleared by any edit
    /// to `input` (`exit_history_navigation` path) and when a new turn starts.
    pub suggestion: Option<String>,
    /// True while the background generation task is in flight — renders a
    /// "…" hint so the ghost doesn't pop in mid-draw. Never blocks the event
    /// loop (best-effort, off critical path); a generation failure just
    /// clears both.
    pub suggestion_loading: bool,
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
    /// Messages typed while a turn is running (item 2 ante "steering"):
    /// Enter queues them instead of dropping them, `Ctrl+S` flushes them into
    /// the running turn as live guidance, and they auto-submit when the turn
    /// ends. Kept in submit order, drained FIFO.
    pub steer_queue: Vec<String>,
    /// The session's own root (`Agent::reply` → `session.working_dir`), set by
    /// `event_loop` — every relative path the TUI resolves goes through it: the
    /// @-mention index and its `./`, `../` fragments, pasted paths, and the
    /// approval modal's `write` preview. Defaults to the process cwd, which a
    /// resumed session may have left behind.
    working_dir: std::path::PathBuf,
    /// Last index snapshot delivered by the background build. `None` until
    /// the first build lands — completion serves nothing rather than
    /// blocking the event loop on a full walk.
    mention_index: Option<crate::tui::mentions::MentionIndex>,
    mention_index_built_at: Option<Instant>,
    /// A build is in flight: no second one is requested, and the dropdown
    /// shows `indexation…` while there is nothing to complete with.
    mention_indexing: bool,
    /// Picked up by `event_loop` (`take_mention_index_request`) which owns
    /// the blocking task — `App` itself never walks the filesystem.
    pending_index_request: bool,
    /// Current dropdown contents for the active `@` fragment, recomputed on
    /// every input mutation so `ui` reads them with a shared `&App`.
    pub mention_matches: Vec<String>,
    /// Selected row in the mention dropdown — cyclic ↑/↓, reset on edit.
    pub mention_selected: usize,
    /// Set by Esc on the dropdown: keeps it closed for the current fragment
    /// without deleting it. Cleared by any input edit.
    mention_suppressed: bool,
    /// Fuzzy file finder overlay (task 8) — while `Some` it owns the
    /// keyboard: nothing typed into it reaches the composer.
    pub finder: Option<FinderState>,
    /// Sélecteur de thème (`/theme`) — même contrat d'overlay que le finder :
    /// tant qu'il est `Some`, il capture le clavier.
    pub theme_picker: Option<ThemePicker>,
    /// Sélecteur d'éditeur (`/editor`) — même contrat d'overlay.
    pub editor_picker: Option<EditorPicker>,
    /// Éditeurs détectés, environnement et choix courant, posés par
    /// `event_loop` (`tui::startup_editors`). Vide par défaut : `App::new` ne
    /// lit ni le `PATH` ni la config.
    pub editors: EditorState,
    /// `KAJI_EDIT_MODE` — env > config, défaut `auto`. Posé par `event_loop`
    /// au démarrage et par `/editor mode`. `request_edit` s'en sert pour la
    /// même décision que l'exécution prendra (`editors::plan`) : un tour en
    /// cours ne bloque que si le lancement resterait un `Suspend`.
    pub edit_mode: EditMode,
    /// Ce que kaji voit de son terminal hôte — nvim, Zellij, tmux — posé une
    /// fois par `event_loop` (`tui::launch_context`) à partir de
    /// l'environnement et de `working_dir`. Ne change pas en cours de
    /// session.
    pub launch_ctx: LaunchContext,
    /// Right-column file viewer. Takes the SPEC panel's slot for as long as
    /// it is open; closing it hands the slot back with no state to restore
    /// (`ui::draw` reads `spec_panel_visible` again).
    pub viewer: Option<crate::tui::viewer::Viewer>,
    /// Left-column file tree (task 9). Open and focused are two different
    /// things: `Ctrl+E` opens it focused, focuses it back, then closes it.
    pub explorer: Option<crate::tui::explorer::ExplorerState>,
    pub focus: Focus,
    /// Viewer geometry measured by `ui::draw_viewer` — same render-time
    /// measurement as `chat_overflow`: the wheel hit-test and the half-page
    /// jump need a height only the renderer knows.
    pub viewer_area: Cell<Rect>,
    /// A provider failure reached the transcript as an `Error` block during
    /// this turn. The stream still ends normally afterwards, so this is what
    /// tells `turn_end` the turn produced nothing to judge — see
    /// [`App::abort_drivers_on_provider_error`]. Re-armed per turn by
    /// `reset_turn_visibility`.
    turn_had_error: bool,
    /// Le volet 炉 forge : ce que les lames déléguées font, alimenté par le
    /// tick 1 s d'`event_loop` et par les notifications MCP des subagents.
    pub forge: forge::ForgeState,
    /// La vue plein écran du même 炉 : stages en colonnes, cartes agent,
    /// timeline. Ouverte depuis le volet (`f`) ou par `/forge full`.
    pub mission: missioncontrol::MissionState,
}

impl App {
    pub fn new(spec: Option<SpecDoc>) -> Self {
        Self {
            header: String::new(),
            model: String::new(),
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
            git_refresh_inflight: false,
            pending_git_request: false,
            spec,
            pass: SddPass::new(),
            gate_open: false,
            tool_approval: None,
            approval_detail: None,
            kaji_mode: KajiMode::default(),
            seal_unfolded_until: None,
            icons: IconSet::Nerd,
            pending_restore: None,
            pending_restore_files_only: false,
            pending_forge_cancel: None,
            forge_sheet_open: None,
            driver: PassDriver::Idle,
            goal: None,
            goal_buffer: String::new(),
            goal_events: Vec::new(),
            scroll_offset: 0,
            chat_overflow: Cell::new(0),
            user_turn_rows: RefCell::new(Vec::new()),
            prompt_history: Vec::new(),
            history_index: None,
            palette_selected: 0,
            suggestion: None,
            suggestion_loading: false,
            mouse_enabled: false,
            validate_buffer: String::new(),
            last_agent_msg_id: None,
            last_thinking_msg_id: None,
            agent_stream_idx: None,
            pending_tools: HashMap::new(),
            spec_panel_forced: None,
            steer_queue: Vec::new(),
            working_dir: std::env::current_dir().unwrap_or_default(),
            mention_index: None,
            mention_index_built_at: None,
            mention_indexing: false,
            pending_index_request: false,
            mention_matches: Vec::new(),
            mention_selected: 0,
            mention_suppressed: false,
            finder: None,
            theme_picker: None,
            editor_picker: None,
            editors: EditorState::default(),
            edit_mode: EditMode::default(),
            launch_ctx: LaunchContext::default(),
            viewer: None,
            explorer: None,
            focus: Focus::default(),
            viewer_area: Cell::new(Rect::default()),
            turn_had_error: false,
            forge: forge::ForgeState::default(),
            mission: missioncontrol::MissionState::default(),
        }
    }

    /// Called once by `event_loop` with the session's `working_dir`. Kept out
    /// of `new` so the ~60 existing call sites (mostly tests) keep the process
    /// cwd, as `mouse_enabled` does.
    pub fn set_working_dir(&mut self, dir: std::path::PathBuf) {
        self.working_dir = dir;
    }

    pub fn working_dir(&self) -> &std::path::Path {
        &self.working_dir
    }

    /// Arms a git read for the status bar. Idempotent while one is in flight —
    /// the 5 s tick calls this on every beat and only the first one after an
    /// answer arms anything.
    pub fn request_git_refresh(&mut self) {
        if self.git_refresh_inflight {
            return;
        }
        self.git_refresh_inflight = true;
        self.pending_git_request = true;
    }

    /// `event_loop` polls this and runs [`crate::tui::gitstatus::read`] on a
    /// blocking task, delivering the result to [`App::on_git_status`].
    pub fn take_git_refresh_request(&mut self) -> Option<std::path::PathBuf> {
        if !self.pending_git_request {
            return None;
        }
        self.pending_git_request = false;
        Some(self.working_dir.clone())
    }

    pub fn on_git_status(&mut self, status: Option<GitStatus>) {
        self.git_status = status;
        self.git_refresh_inflight = false;
    }

    pub fn start_pass(&mut self) {
        if self.goal_driver() != GoalDriver::Idle {
            self.push_system("but en cours — /goal clear avant de lancer une passe SDD");
            return;
        }
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

    pub fn goal_driver(&self) -> GoalDriver {
        match self.goal.as_ref().filter(|goal| goal.is_active()) {
            None => GoalDriver::Idle,
            Some(goal) => match goal.phase {
                GoalPhase::Working => GoalDriver::Working,
                GoalPhase::Evaluating => GoalDriver::Evaluating,
            },
        }
    }

    /// Sets (or replaces) the goal and returns the first work prompt — `None`
    /// when an SDD pass owns the turn chain, since both drivers relaunch turns
    /// from the same `turn_end`.
    pub fn goal_set(&mut self, condition: &str, max_iterations: usize) -> Option<String> {
        if self.driver != PassDriver::Idle {
            self.push_system("passe SDD en cours — termine-la avant de fixer un but");
            return None;
        }
        if self.goal_driver() != GoalDriver::Idle {
            self.end_goal(GoalOutcome::Cleared, "目標 but remplacé");
        }
        self.goal = Some(GoalState::new(condition.to_string(), max_iterations));
        self.goal_buffer.clear();
        self.push_goal_event(
            "goal_start",
            serde_json::json!({ "condition": condition, "max_iterations": max_iterations }),
        );
        self.push_system(&format!(
            "目標 but fixé : {} — évaluateur après chaque tour, cap {max_iterations} itérations",
            sanitize_for_display(condition)
        ));
        Some(goal::work_prompt(condition))
    }

    /// `true` when a live goal was actually stopped — the caller only cancels
    /// the running turn in that case, so a stray `/goal clear` doesn't kill an
    /// unrelated turn.
    pub fn goal_clear(&mut self) -> bool {
        if self.goal_driver() == GoalDriver::Idle {
            self.push_system("aucun but en cours");
            return false;
        }
        self.end_goal(GoalOutcome::Cleared, "目標 but effacé");
        true
    }

    /// Same four sites as [`App::pass_abort`]: Esc, steering, a setup failure
    /// and a stream error all leave the goal loop stopped rather than silently
    /// waiting for a turn that will never end.
    pub fn goal_abort(&mut self, reason: &str) {
        if self.goal_driver() == GoalDriver::Idle {
            return;
        }
        self.end_goal(GoalOutcome::Interrupted, reason);
    }

    pub fn push_goal_status(&mut self) {
        let Some(goal) = self.goal.as_ref() else {
            self.push_system("aucun but — /goal <condition> pour en fixer un");
            return;
        };
        let condition = sanitize_for_display(&goal.condition);
        let line = match goal.outcome {
            None => format!(
                "目標 {} · it {}/{} · {}",
                condition,
                goal.iteration,
                goal.max_iterations,
                goal.phase.label()
            ),
            Some(outcome) => format!(
                "目標 {} · terminé : {} (it {}/{})",
                condition,
                outcome.label(),
                goal.iteration,
                goal.max_iterations
            ),
        };
        self.push_system(&line);
    }

    /// Drained by the event loop, which owns the session handle the
    /// `append_event` writes need.
    pub fn take_goal_events(&mut self) -> Vec<(&'static str, String)> {
        std::mem::take(&mut self.goal_events)
    }

    fn push_goal_event(&mut self, kind: &'static str, payload: serde_json::Value) {
        self.goal_events.push((kind, payload.to_string()));
    }

    fn push_goal_end(&mut self, outcome: GoalOutcome, iteration: usize) {
        self.push_goal_event(
            "goal_end",
            serde_json::json!({ "outcome": outcome_token(outcome), "iteration": iteration }),
        );
    }

    /// Stops the goal from outside the evaluator loop (cleared, replaced,
    /// interrupted) — `message` is the caller's wording, as `pass_abort` takes
    /// its reason.
    fn end_goal(&mut self, outcome: GoalOutcome, message: &str) {
        let Some(goal) = self.goal.as_mut() else {
            return;
        };
        goal.finish(outcome);
        let iteration = goal.iteration;
        self.goal_buffer.clear();
        self.push_goal_end(outcome, iteration);
        self.push_system(message);
    }

    /// The goal half of [`App::turn_end`]: a finished work turn hands over to
    /// the evaluator, a finished evaluator turn is judged.
    fn goal_turn_end(&mut self) -> Option<String> {
        match self.goal_driver() {
            GoalDriver::Idle => None,
            GoalDriver::Working => {
                let goal = self.goal.as_mut()?;
                goal.begin_evaluation();
                let condition = goal.condition.clone();
                let iteration = goal.iteration;
                let max_iterations = goal.max_iterations;
                self.goal_buffer.clear();
                self.push_system(&format!(
                    "目標 évaluation — itération {iteration}/{max_iterations}"
                ));
                Some(goal::evaluator_prompt(&condition))
            }
            GoalDriver::Evaluating => self.judge_goal(),
        }
    }

    /// Fail-closed like the SDD judge: an evaluator that never wrote a verdict
    /// line continues the loop, it never ends the goal.
    fn judge_goal(&mut self) -> Option<String> {
        let evaluation = std::mem::take(&mut self.goal_buffer);
        let verdict = match goal::parse_verdict(&evaluation) {
            Some(verdict) => verdict,
            None => {
                self.push_system("⚠ 目標 verdict absent — on continue par prudence");
                Verdict::Continue(goal::bound_feedback(evaluation.trim()))
            }
        };
        let (token, feedback) = match &verdict {
            Verdict::Met => ("met", String::new()),
            Verdict::Continue(feedback) => ("continue", feedback.clone()),
            Verdict::Unreachable(reason) => ("unreachable", reason.clone()),
        };
        let goal = self.goal.as_mut()?;
        let condition = goal.condition.clone();
        let iteration = goal.iteration;
        let max_iterations = goal.max_iterations;
        let step = goal.apply_verdict(verdict);
        self.push_goal_event(
            "goal_iteration",
            serde_json::json!({ "iteration": iteration, "verdict": token, "feedback": feedback }),
        );
        match step {
            GoalStep::Continue(feedback) => {
                let next = iteration + 1;
                self.push_system(&format!(
                    "目標 but non atteint — itération {next}/{max_iterations}"
                ));
                Some(goal::continuation_prompt(&condition, &feedback))
            }
            GoalStep::Finished(outcome) => {
                let message = match outcome {
                    GoalOutcome::Met => format!(
                        "✓ 目標 but atteint en {iteration} itération(s) : {}",
                        sanitize_for_display(&condition)
                    ),
                    GoalOutcome::Unreachable => format!(
                        "⚠ 目標 but jugé inatteignable : {}",
                        sanitize_for_display(&feedback)
                    ),
                    GoalOutcome::IterationCap => format!(
                        "⚠ 目標 cap de {max_iterations} itérations atteint — but non atteint : {}",
                        sanitize_for_display(&condition)
                    ),
                    GoalOutcome::Cleared => "目標 but effacé".to_string(),
                    GoalOutcome::Interrupted => "目標 but interrompu".to_string(),
                };
                self.push_goal_end(outcome, iteration);
                self.push_system(&message);
                None
            }
        }
    }

    /// A provider failure arrives as an `Error` block on a stream that then
    /// ends normally, so neither driver's `Err` path fires: the work turn
    /// produced nothing, and the evaluator buffer stays empty — which
    /// `judge_goal` would read as "verdict absent" and answer by hammering the
    /// provider that just failed, up to the iteration cap.
    fn abort_drivers_on_provider_error(&mut self) {
        if self.driver != PassDriver::Idle {
            self.pass_abort("erreur provider — passe interrompue");
        }
        self.goal_abort("目標 erreur provider — but interrompu");
    }

    pub fn turn_end(&mut self) -> Option<String> {
        self.turn_active = false;
        if self.turn_had_error {
            self.abort_drivers_on_provider_error();
            return None;
        }
        match self.driver {
            PassDriver::Executing => {
                let body = self.spec.as_ref()?.body.clone();
                self.pass.advance();
                self.driver = PassDriver::Validating;
                self.validate_buffer.clear();
                Some(format!(
                    "Tu es un juge de conformité SDD, pas un assistant complaisant : ton biais par défaut doit être DRIFT, pas VALIDE. Liste chaque exigence de la SPEC ci-dessous une par une et vérifie-la individuellement contre la réponse précédente — cherche activement les écarts, ne les suppose pas absents. Ne conclus VALIDE que si CHAQUE exigence est vérifiablement satisfaite ; au moindre doute ou à la moindre exigence non démontrée, le verdict est DRIFT. Justifie ton verdict exigence par exigence (l'écart constaté, ou l'absence d'écart) avant la ligne finale. Dernière ligne, exactement : `VERDICT: VALIDE` ou `VERDICT: DRIFT`.\n\n{body}"
                ))
            }
            PassDriver::Validating => {
                self.pass.advance();
                let last_line = goal::last_verdict_line(&self.validate_buffer)
                    .map(|(_, line)| line)
                    .unwrap_or_default();
                if last_line.contains("VERDICT: VALIDE") {
                    self.pass.advance();
                    self.push_system("✓ passe SDD complète — spec verrouillée");
                } else {
                    self.pass.fail_current();
                    if last_line.contains("VERDICT: DRIFT") {
                        self.push_system("⚠ drift détecté — spec non verrouillée");
                    } else {
                        self.push_system("⚠ verdict absent ou imparsable — DRIFT par prudence");
                    }
                }
                self.driver = PassDriver::Idle;
                None
            }
            // Mutually exclusive by construction (`start_pass`/`goal_set` each
            // refuse while the other drives), so the goal loop only ever gets
            // the turn an idle SDD pass leaves it.
            _ => self.goal_turn_end(),
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
    /// The turn is the one moment the working tree is guaranteed to have
    /// changed under the user's feet, so the status bar asks for a fresh read
    /// here rather than waiting for the next tick.
    pub fn finish_turn(&mut self) {
        self.turn_active = false;
        self.turn_started = None;
        self.request_git_refresh();
    }

    /// Queues a steering message typed while a turn is running (item 2 ante).
    /// Returns the queue depth after pushing — the caller surfaces it so the
    /// user sees how many messages are waiting.
    pub fn queue_steer(&mut self, text: &str) -> usize {
        self.steer_queue.push(text.to_string());
        self.steer_queue.len()
    }

    pub fn steer_len(&self) -> usize {
        self.steer_queue.len()
    }

    /// Takes the next queued steering message, FIFO. Returns `None` when the
    /// queue is empty.
    pub fn next_steer(&mut self) -> Option<String> {
        if self.steer_queue.is_empty() {
            None
        } else {
            Some(self.steer_queue.remove(0))
        }
    }

    /// Recomputes the mention dropdown contents from the live `@` fragment.
    /// Called on every input mutation; Esc suppression holds until the next
    /// edit — which is exactly when this runs.
    ///
    /// Never walks the project: `~/`, `/`, `./`, `../` fragments cost one
    /// bounded `read_dir`, and everything else is served from whatever index
    /// snapshot is on hand — a stale one keeps answering while the rebuild
    /// runs on its own task.
    fn update_mention_matches(&mut self) {
        self.reset_mention_state();
        let Some(fragment) = crate::tui::mentions::active_fragment(&self.input).map(str::to_string)
        else {
            return;
        };
        if let Some(listing) =
            crate::tui::mentions::complete_via_listing(&fragment, &self.working_dir)
        {
            self.mention_matches = listing;
            return;
        }
        if self.mention_index_stale() {
            self.request_mention_index();
        }
        if let Some(index) = &self.mention_index {
            self.mention_matches = index.complete(&fragment);
        }
    }

    /// Clears everything the dropdown derives from the live fragment. Single
    /// reset point so a new dismissal path can't leave `mention_selected` or
    /// the Esc suppression pointing at a fragment that no longer exists.
    fn reset_mention_state(&mut self) {
        self.mention_matches.clear();
        self.mention_selected = 0;
        self.mention_suppressed = false;
    }

    fn mention_index_stale(&self) -> bool {
        self.mention_index_built_at
            .map(|t| t.elapsed() > crate::tui::mentions::INDEX_TTL)
            .unwrap_or(true)
    }

    fn request_mention_index(&mut self) {
        if self.mention_indexing {
            return;
        }
        self.mention_indexing = true;
        self.pending_index_request = true;
    }

    /// `event_loop` polls this after every event and runs the walk on a
    /// blocking task, delivering the result to `on_mention_index_ready`.
    pub fn take_mention_index_request(&mut self) -> Option<std::path::PathBuf> {
        if !self.pending_index_request {
            return None;
        }
        self.pending_index_request = false;
        Some(self.working_dir.clone())
    }

    pub fn on_mention_index_ready(&mut self, index: crate::tui::mentions::MentionIndex) {
        self.mention_index = Some(index);
        self.mention_index_built_at = Some(Instant::now());
        self.mention_indexing = false;
        // An index landing mid-fragment must not re-open a dropdown Esc just
        // dismissed — only an input edit re-arms completion.
        let suppressed = self.mention_suppressed;
        self.update_mention_matches();
        self.mention_suppressed = suppressed;
        self.update_finder_results();
    }

    pub fn mention_index_truncated(&self) -> bool {
        self.mention_index
            .as_ref()
            .is_some_and(crate::tui::mentions::MentionIndex::truncated)
    }

    pub fn mention_dropdown_visible(&self) -> bool {
        !self.modal_active() && !self.mention_suppressed && !self.mention_matches.is_empty()
    }

    /// The `indexation…` placeholder stands in for the dropdown while the
    /// first build is in flight. Deliberately not part of
    /// `mention_dropdown_visible`: an empty dropdown must not capture
    /// Tab/Enter/Esc.
    pub fn mention_indexing_visible(&self) -> bool {
        self.mention_indexing
            && self.mention_matches.is_empty()
            && !self.mention_suppressed
            && !self.modal_active()
            && crate::tui::mentions::active_fragment(&self.input).is_some()
    }

    /// Tab/Enter on the dropdown: replaces the active `@` fragment with the
    /// selected completion and refreshes — completing a directory exposes its
    /// children for the next keystroke.
    fn apply_mention_completion(&mut self) {
        let Some(completion) = self
            .mention_matches
            .get(
                self.mention_selected
                    .min(self.mention_matches.len().saturating_sub(1)),
            )
            .cloned()
        else {
            return;
        };
        let fragment_len = crate::tui::mentions::active_fragment(&self.input)
            .map(str::len)
            .unwrap_or(0);
        let keep = self.input.len() - fragment_len;
        self.input.truncate(keep);
        self.input.push_str(&completion);
        self.update_mention_matches();
    }

    /// Opens the fuzzy file finder (`Ctrl+P`, `/files`). It reads the very
    /// index the @-mention dropdown does, with the same contract: a stale
    /// snapshot keeps answering while the rebuild runs on its own task, and a
    /// missing one only emits the request `event_loop` picks up.
    pub fn open_finder(&mut self) {
        if self.modal_active() {
            return;
        }
        if self.mention_index_stale() {
            self.request_mention_index();
        }
        self.finder = Some(FinderState::default());
        self.update_finder_results();
    }

    pub fn close_finder(&mut self) {
        self.finder = None;
    }

    /// The finder has nothing to list because the first walk is still
    /// running — distinct from "no match", which a served (even stale) index
    /// produces.
    pub fn finder_indexing(&self) -> bool {
        self.finder.is_some() && self.mention_index.is_none()
    }

    fn update_finder_results(&mut self) {
        let Some(query) = self.finder.as_ref().map(|finder| finder.query.clone()) else {
            return;
        };
        let results = self
            .mention_index
            .as_ref()
            .map(|index| index.search(&query, FINDER_MAX_RESULTS))
            .unwrap_or_default();
        if let Some(finder) = self.finder.as_mut() {
            finder.results = results;
            finder.selected = 0;
        }
    }

    fn finder_insert(&mut self, text: &str) {
        let Some(finder) = self.finder.as_mut() else {
            return;
        };
        let at = char_boundary(&finder.query, finder.cursor);
        finder.query.insert_str(at, text);
        finder.cursor += text.chars().count();
        self.update_finder_results();
    }

    fn finder_backspace(&mut self) {
        let Some(finder) = self.finder.as_mut() else {
            return;
        };
        let Some(previous) = finder.cursor.checked_sub(1) else {
            return;
        };
        let at = char_boundary(&finder.query, previous);
        finder.query.remove(at);
        finder.cursor = previous;
        self.update_finder_results();
    }

    fn finder_move(&mut self, delta: isize) {
        let Some(finder) = self.finder.as_mut() else {
            return;
        };
        let count = finder.results.len();
        if count == 0 {
            return;
        }
        let step = delta.rem_euclid(count as isize) as usize;
        finder.selected = (finder.selected + step) % count;
    }

    fn finder_cursor_left(&mut self) {
        if let Some(finder) = self.finder.as_mut() {
            finder.cursor = finder.cursor.saturating_sub(1);
        }
    }

    fn finder_cursor_right(&mut self) {
        if let Some(finder) = self.finder.as_mut() {
            finder.cursor = (finder.cursor + 1).min(finder.query.chars().count());
        }
    }

    fn finder_selection(&self) -> Option<String> {
        let finder = self.finder.as_ref()?;
        finder.results.get(finder.selected).cloned()
    }

    /// Every key while the finder is open — nothing it swallows reaches the
    /// composer, which is the whole point of an overlay you type into.
    fn finder_key(&mut self, key: &KeyEvent) -> Action {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('p') if ctrl => self.close_finder(),
            // Telescope's chords, alongside the arrows.
            KeyCode::Char('n' | 'j') if ctrl => self.finder_move(1),
            KeyCode::Char('k') if ctrl => self.finder_move(-1),
            KeyCode::Down => self.finder_move(1),
            KeyCode::Up => self.finder_move(-1),
            KeyCode::Left => self.finder_cursor_left(),
            KeyCode::Right => self.finder_cursor_right(),
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.finder_insert(c.encode_utf8(&mut [0u8; 4]))
            }
            KeyCode::Backspace => self.finder_backspace(),
            KeyCode::Enter => {
                if let Some(path) = self.finder_selection() {
                    self.close_finder();
                    self.open_viewer(&path);
                }
            }
            KeyCode::Tab => {
                if let Some(path) = self.finder_selection() {
                    self.close_finder();
                    self.attach_mention(&path);
                }
            }
            KeyCode::Esc => self.close_finder(),
            _ => {}
        }
        Action::None
    }

    /// Ouvre le sélecteur de thème (`/theme`, `/theme list`) sur la palette
    /// active — même garde que le finder : une modale y/n garde le clavier.
    pub fn open_theme_picker(&mut self) {
        if self.modal_active() {
            return;
        }
        let active = theme::active_index();
        self.theme_picker = Some(ThemePicker {
            selected: active,
            initial: active,
        });
    }

    fn theme_picker_select(&mut self, index: usize) {
        let Some(picker) = self.theme_picker.as_mut() else {
            return;
        };
        picker.selected = index;
        theme::set_active_index(index);
    }

    fn theme_picker_move(&mut self, delta: isize) {
        let Some(picker) = self.theme_picker.as_ref() else {
            return;
        };
        let count = theme::THEMES.len();
        let step = delta.rem_euclid(count as isize) as usize;
        self.theme_picker_select((picker.selected + step) % count);
    }

    /// Every key while the theme picker is open. Moving the selection applies
    /// the palette for real — the whole screen is the preview — so Enter has
    /// nothing left to apply and Esc has one thing to undo.
    fn theme_picker_key(&mut self, key: &KeyEvent) -> Action {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('n' | 'j') if ctrl => self.theme_picker_move(1),
            KeyCode::Char('k') if ctrl => self.theme_picker_move(-1),
            KeyCode::Down | KeyCode::Char('j') if !ctrl => self.theme_picker_move(1),
            KeyCode::Up | KeyCode::Char('k') if !ctrl => self.theme_picker_move(-1),
            KeyCode::Home => self.theme_picker_select(0),
            KeyCode::End => self.theme_picker_select(theme::THEMES.len() - 1),
            KeyCode::Enter => {
                self.theme_picker = None;
                let applied = theme::active().name;
                self.push_system(&format!("thème : {applied}"));
                return Action::Theme(applied.to_string());
            }
            KeyCode::Esc | KeyCode::Char('q') if !ctrl => {
                if let Some(picker) = self.theme_picker.take() {
                    theme::set_active_index(picker.initial);
                }
            }
            _ => {}
        }
        Action::None
    }

    /// Ouvre le sélecteur d'éditeur (`/editor`, `/editor list`) : les éditeurs
    /// détectés, plus la ligne `$VISUAL`/`$EDITOR` quand elle est définie.
    /// Sans une seule ligne à montrer, dire quoi taper vaut mieux qu'une boîte
    /// vide.
    pub fn open_editor_picker(&mut self) {
        if self.modal_active() {
            return;
        }
        let mut rows: Vec<EditorRow> = self
            .editors
            .detected
            .iter()
            .map(|spec| EditorRow::Detected(spec))
            .collect();
        if let Some(label) = self.editors.env_label() {
            rows.push(EditorRow::Env(label));
        }
        if rows.is_empty() {
            self.push_system("aucun éditeur détecté — /editor <commande>");
            return;
        }
        let current = self.current_editor_row(&rows);
        self.editor_picker = Some(EditorPicker {
            selected: current.unwrap_or(0),
            rows,
            current,
        });
    }

    fn current_editor_row(&self, rows: &[EditorRow]) -> Option<usize> {
        if self.editors.selected.is_none() && self.editors.env_label().is_some() {
            return rows.iter().position(|row| matches!(row, EditorRow::Env(_)));
        }
        let resolved = self.editors.resolve().ok()?;
        rows.iter().position(|row| match row {
            EditorRow::Detected(spec) => spec.program == resolved.program_name(),
            EditorRow::Env(_) => false,
        })
    }

    fn editor_picker_select(&mut self, index: usize) {
        if let Some(picker) = self.editor_picker.as_mut() {
            picker.selected = index.min(picker.rows.len() - 1);
        }
    }

    fn editor_picker_move(&mut self, delta: isize) {
        let Some(picker) = self.editor_picker.as_mut() else {
            return;
        };
        let count = picker.rows.len();
        let step = delta.rem_euclid(count as isize) as usize;
        picker.selected = (picker.selected + step) % count;
    }

    fn editor_picker_key(&mut self, key: &KeyEvent) -> Action {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('n' | 'j') if ctrl => self.editor_picker_move(1),
            KeyCode::Char('k') if ctrl => self.editor_picker_move(-1),
            KeyCode::Down | KeyCode::Char('j') if !ctrl => self.editor_picker_move(1),
            KeyCode::Up | KeyCode::Char('k') if !ctrl => self.editor_picker_move(-1),
            KeyCode::Home => self.editor_picker_select(0),
            KeyCode::End => self.editor_picker_select(usize::MAX),
            KeyCode::Enter => {
                let Some(picker) = self.editor_picker.take() else {
                    return Action::None;
                };
                return match &picker.rows[picker.selected] {
                    EditorRow::Detected(spec) => self.select_editor(&spec.command(), spec.id),
                    EditorRow::Env(_) => self.reset_editor(),
                };
            }
            KeyCode::Esc | KeyCode::Char('q') if !ctrl => self.editor_picker = None,
            _ => {}
        }
        Action::None
    }

    fn select_editor(&mut self, command: &str, label: &str) -> Action {
        self.editors.selected = Some(command.to_string());
        self.push_system(&format!("éditeur : {label}"));
        Action::Editor(command.to_string())
    }

    fn reset_editor(&mut self) -> Action {
        self.editors.selected = None;
        let source = match self.editors.resolve() {
            Ok(_) => self
                .editors
                .env_label()
                .unwrap_or_else(|| "détection du PATH".to_string()),
            Err(note) => note,
        };
        self.push_system(&format!("éditeur : {source}"));
        Action::EditorReset
    }

    /// `/editor` : vide ou `list` = sélecteur, `reset` rend la main à
    /// l'environnement, `mode` bascule/affiche `KAJI_EDIT_MODE`, sinon la
    /// commande donnée devient l'éditeur de la session — libre, un binaire
    /// hors catalogue compris.
    fn run_editor_command(&mut self, arg: &str) -> Action {
        if let Some(mode_arg) = editor_mode_arg(arg) {
            return self.run_editor_mode_command(mode_arg);
        }
        if arg.is_empty() || arg.eq_ignore_ascii_case("list") {
            self.open_editor_picker();
            return Action::None;
        }
        if arg.eq_ignore_ascii_case("reset") {
            return self.reset_editor();
        }
        self.select_editor(arg, arg)
    }

    /// `/editor mode` seul affiche le mode configuré et le mode effectif pour
    /// l'éditeur courant ; `/editor mode <valeur>` bascule le mode et le
    /// fait persister par l'event loop.
    fn run_editor_mode_command(&mut self, arg: &str) -> Action {
        if arg.is_empty() {
            self.push_system(&format!(
                "éditeur : {} (effectif : {})",
                self.edit_mode.as_str(),
                self.effective_edit_mode_label()
            ));
            return Action::None;
        }
        let Some(mode) = EditMode::parse(arg) else {
            self.push_system(&format!(
                "mode inconnu : {arg} — auto | suspend | remote | pane | gui"
            ));
            return Action::None;
        };
        self.edit_mode = mode;
        self.push_system(&format!("éditeur : {}", mode.as_str()));
        Action::EditMode(mode.as_str().to_string())
    }

    /// Ce que `editors::plan` choisirait maintenant pour l'éditeur courant —
    /// le fichier cible n'entre dans aucune branche de `plan` (seulement dans
    /// l'argv qu'elle construit), donc un chemin d'espace réservé suffit ici.
    fn effective_edit_mode_label(&self) -> String {
        match self.editors.resolve() {
            Ok(editor) => {
                let launch = editors::plan(
                    self.edit_mode,
                    &self.launch_ctx,
                    &editor,
                    std::path::Path::new(""),
                    None,
                );
                editors::launch_label(&launch, &editor)
            }
            Err(_) => "aucun éditeur".to_string(),
        }
    }

    /// Loads a file into the right-column viewer and gives it the focus. A
    /// directory, a missing file or an unreadable one is reported as a system
    /// line instead of opening an empty pane.
    pub fn open_viewer(&mut self, path: &str) {
        let resolved = crate::tui::mentions::resolve(path, &self.working_dir);
        match crate::tui::viewer::load(path, &resolved) {
            Ok(viewer) => {
                self.viewer = Some(viewer);
                self.forge_sheet_open = None;
                self.focus = Focus::Viewer;
            }
            Err(e) => {
                self.focus = Focus::Composer;
                self.push_system(&format!("lecture impossible : {e}"));
            }
        }
    }

    pub fn close_viewer(&mut self) {
        self.viewer = None;
        self.forge_sheet_open = None;
        self.focus = Focus::Composer;
    }

    /// Every path that ends up in `$EDITOR` goes through here: `~` expanded,
    /// relative resolved against the session's root, `@path` accepted as the
    /// same thing the composer would attach. A missing file is deliberately
    /// let through — the editor creates it.
    ///
    /// Un lancement qui suspendrait le terminal (`Launch::Suspend`) est
    /// refusé pendant un tour : il prendrait l'écran pendant que l'agent
    /// écrit dans le même arbre, et deux écrivains sur un fichier est un
    /// conflit qu'aucun des deux ne voit. Tout le reste — IDE graphique,
    /// nvim hôte, pane Zellij/tmux — ne prend rien : ça passe, avec
    /// l'avertissement qui va avec.
    fn request_edit(&mut self, path: &str, line: Option<usize>) -> Action {
        let raw = path.trim_start_matches('@');
        let resolved = crate::tui::mentions::resolve(raw, &self.working_dir);
        if resolved.is_dir() {
            self.push_system(&format!("{raw} : dossier, pas un fichier"));
            return Action::None;
        }
        let busy = self.turn_active || self.turn_pending;
        let non_blocking = self.editors.resolve().is_ok_and(|editor| {
            !matches!(
                editors::plan(self.edit_mode, &self.launch_ctx, &editor, &resolved, line),
                Launch::Suspend(_)
            )
        });
        if busy && !non_blocking {
            self.push_system("un tour est en cours — attends la fin");
            return Action::None;
        }
        if busy {
            self.push_system_lines(vec![vec![RoledSpan::dim(
                "le tour continue — éditions concurrentes à ta charge",
            )]]);
        }
        Action::EditFile {
            path: resolved,
            line,
        }
    }

    fn run_edit_command(&mut self, arg: &str) -> Action {
        if arg.is_empty() {
            self.push_system("usage : /edit <chemin>[:ligne]");
            return Action::None;
        }
        let (path, line) = self.split_line_suffix(arg);
        self.request_edit(&path, line)
    }

    /// `src/app.rs:42` — le suffixe n'est une ligne que s'il est numérique et
    /// que le chemin littéral n'existe pas : un fichier réellement nommé
    /// `notes:42` s'ouvre lui-même plutôt qu'une ligne de `notes`.
    fn split_line_suffix(&self, arg: &str) -> (String, Option<usize>) {
        let Some((path, line)) = arg.rsplit_once(':') else {
            return (arg.to_string(), None);
        };
        let Ok(line) = line.parse::<usize>() else {
            return (arg.to_string(), None);
        };
        if crate::tui::mentions::resolve(arg.trim_start_matches('@'), &self.working_dir).exists() {
            return (arg.to_string(), None);
        }
        (path.to_string(), Some(line))
    }

    /// Le mécanisme partagé par `on_file_edited` et `reload_viewer` : relit
    /// `path` sous le nom d'affichage `display`, clampe le scroll dans le
    /// nouveau contenu. Seul ce que chaque appelant en dit au chat diffère.
    fn reload_viewer_content(
        &mut self,
        display: &str,
        path: &std::path::Path,
        scroll: usize,
    ) -> std::result::Result<(), String> {
        let viewport = self.viewer_viewport();
        let mut reloaded = crate::tui::viewer::load(display, path).map_err(|e| e.to_string())?;
        reloaded.scroll = scroll.min(reloaded.max_scroll(viewport));
        self.viewer = Some(reloaded);
        Ok(())
    }

    /// The editor had the terminal and the file may have changed under the
    /// panes: the viewer holds a snapshot taken at open time, the explorer a
    /// listing that a brand new file is missing from.
    pub fn on_file_edited(&mut self, path: &std::path::Path) {
        let stale = self.viewer.as_ref().filter(|viewer| {
            crate::tui::mentions::resolve(&viewer.path, &self.working_dir) == path
        });
        if let Some((display, scroll)) = stale.map(|v| (v.path.clone(), v.scroll)) {
            let _ = self.reload_viewer_content(&display, path, scroll);
        }
        if let Some(explorer) = self.explorer.as_mut() {
            explorer.refresh();
        }
        self.request_git_refresh();
    }

    /// `r` : recharge le fichier affiché depuis le disque. Une édition non
    /// bloquante (task 19b — nvim hôte, pane Zellij/tmux) ne prévient jamais
    /// kaji quand elle termine, contrairement à `edit_file` qui rend la main
    /// à la sortie de l'éditeur ; le lecteur doit donc pouvoir le redemander.
    /// Même rafraîchissement que `on_file_edited` (explorateur, statut git) —
    /// un lancement non bloquant a pu toucher plus que le seul fichier
    /// affiché.
    fn reload_viewer(&mut self) {
        let Some((display, scroll)) = self
            .viewer
            .as_ref()
            .map(|viewer| (viewer.path.clone(), viewer.scroll))
        else {
            return;
        };
        let resolved = crate::tui::mentions::resolve(&display, &self.working_dir);
        match self.reload_viewer_content(&display, &resolved, scroll) {
            Ok(()) => self.push_system(&format!("{} {display} rechargé", theme::VIEWER_GLYPH)),
            Err(e) => self.push_system(&format!("lecture impossible : {e}")),
        }
        if let Some(explorer) = self.explorer.as_mut() {
            explorer.refresh();
        }
        self.request_git_refresh();
    }

    /// The reader while it holds the focus, which is when it takes the chat's
    /// column too (task 21): reading a file is a full-width activity, and the
    /// chat comes back as soon as the focus does.
    pub fn zoomed_viewer(&self) -> Option<&crate::tui::viewer::Viewer> {
        self.viewer.as_ref().filter(|_| self.focus == Focus::Viewer)
    }

    /// Rows of file content the pane paints, from the last measured geometry
    /// minus its two borders (the header rides the top border as a title) and
    /// les deux lignes de marge intérieure de `draw_viewer`. Floors at one so a
    /// viewer that has never been drawn still scrolls.
    fn viewer_viewport(&self) -> usize {
        usize::from(self.viewer_area.get().height.saturating_sub(4)).max(1)
    }

    fn point_in_viewer(&self, column: u16, row: u16) -> bool {
        let area = self.viewer_area.get();
        self.viewer.is_some()
            && column >= area.x
            && column < area.right()
            && row >= area.y
            && row < area.bottom()
    }

    /// Every key while the viewer holds the focus: it scrolls, attaches or
    /// closes, and ignores the rest — a stray `j` must never end up in the
    /// composer behind it.
    fn viewer_key(&mut self, key: &KeyEvent) -> Action {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            // The one key that isn't ignored: the finder opens from anywhere
            // outside a modal, viewer included.
            KeyCode::Char('p') if ctrl => {
                self.open_finder();
                return Action::None;
            }
            KeyCode::Char('q') | KeyCode::Esc if !ctrl => {
                self.close_viewer();
                return Action::None;
            }
            // Les trois touches de fichier ne valent que pour un fichier : sur
            // une fiche de forge, `viewer.path` est un titre (`遣 …`), pas un
            // chemin — `e` ouvrirait l'éditeur sur un fantôme et le créerait.
            KeyCode::Char('e') | KeyCode::Char('a') | KeyCode::Char('r')
                if !ctrl && self.forge_sheet_open.is_some() =>
            {
                return Action::None;
            }
            // The pane is read-only by design (task 8) — `e` is how a file
            // read here reaches an editor, without kaji growing one. L'éditeur
            // s'ouvre là où on lisait : la première ligne visible.
            KeyCode::Char('e') if !ctrl => {
                let Some((path, scroll)) = self
                    .viewer
                    .as_ref()
                    .map(|viewer| (viewer.path.clone(), viewer.scroll))
                else {
                    return Action::None;
                };
                return self.request_edit(&path, Some(scroll + 1));
            }
            KeyCode::Char('a') if !ctrl => {
                if let Some(path) = self.viewer.as_ref().map(|viewer| viewer.path.clone()) {
                    self.attach_mention(&path);
                }
                // The pane stays open — attaching is a step in writing a
                // message, not a reason to lose what you were reading.
                self.focus = Focus::Composer;
                return Action::None;
            }
            // Non-blocking editing (task 19b) never says when it's done — no
            // suspended kaji to hand the terminal back on exit — so the
            // viewer has to be told to look again.
            KeyCode::Char('r') if !ctrl => {
                self.reload_viewer();
                return Action::None;
            }
            _ => {}
        }
        let viewport = self.viewer_viewport();
        let half = (viewport / 2).max(1);
        let Some(viewer) = self.viewer.as_mut() else {
            return Action::None;
        };
        match key.code {
            KeyCode::Char('j') | KeyCode::Down if !ctrl => viewer.scroll_down(1, viewport),
            KeyCode::Char('k') | KeyCode::Up if !ctrl => viewer.scroll_up(1),
            KeyCode::Char('d') if ctrl => viewer.scroll_down(half, viewport),
            KeyCode::Char('u') if ctrl => viewer.scroll_up(half),
            KeyCode::PageDown => viewer.scroll_down(half, viewport),
            KeyCode::PageUp => viewer.scroll_up(half),
            KeyCode::Char('g') if !ctrl => viewer.scroll_to_start(),
            KeyCode::Char('G') => viewer.scroll_to_end(viewport),
            _ => {}
        }
        Action::None
    }

    /// `Ctrl+E` / `/explorer`, three-state: closed it opens focused, open but
    /// focused elsewhere it takes the keyboard back, focused it closes.
    pub fn toggle_explorer(&mut self) {
        if self.modal_active() {
            return;
        }
        match (self.explorer.is_some(), self.focus) {
            (true, Focus::Explorer) => self.close_explorer(),
            (true, _) => self.focus = Focus::Explorer,
            (false, _) => {
                self.explorer = Some(crate::tui::explorer::ExplorerState::new(
                    self.working_dir.clone(),
                ));
                self.focus = Focus::Explorer;
            }
        }
    }

    pub fn close_explorer(&mut self) {
        self.explorer = None;
        if self.focus == Focus::Explorer {
            self.focus = Focus::Composer;
        }
    }

    /// `Ctrl+F` / `/forge`. Le volet suit la main, sauf sur une forge vide :
    /// sans extension summon aucune lame n'a jamais tourné, et ouvrir donnerait
    /// un cadre muet pour toute réponse.
    pub fn toggle_forge(&mut self) {
        if self.forge.tasks.is_empty() && !self.forge.visible() {
            self.push_system("forge : aucune tâche");
            return;
        }
        self.forge.toggle();
        if self.forge.visible() {
            self.focus = Focus::Forge;
        } else if self.focus == Focus::Forge {
            self.focus = Focus::Composer;
        }
    }

    fn close_forge(&mut self) {
        self.forge.view = forge::ForgeView::ForcedClosed;
        if self.focus == Focus::Forge {
            self.focus = Focus::Composer;
        }
    }

    /// `/forge` sans argument bascule le volet ; `/forge full` ouvre la vue
    /// plein écran.
    fn run_forge_command(&mut self, arg: &str) -> Action {
        match arg.trim() {
            "" => self.toggle_forge(),
            "full" => self.open_mission_control(),
            other => self.push_system(&format!("usage : /forge [full] (reçu « {other} »)")),
        }
        Action::None
    }

    /// `f` depuis le volet forge, ou `/forge full` : la même forge, en plein
    /// écran. La vue s'ouvre même vide — `/forge full` ne doit jamais être un
    /// non-événement silencieux, et le plateau sait dire qu'il n'a rien.
    pub fn open_mission_control(&mut self) {
        self.mission.open = true;
        self.clamp_mission_selection();
    }

    pub fn close_mission_control(&mut self) {
        self.mission.open = false;
    }

    /// Le snapshot du workflow que la session pilote — `None` quand elle n'en
    /// pilote aucun, ce qui rend la vue à sa colonne « libre ».
    pub fn apply_workflow_snapshot(&mut self, state: Option<kaji::workflow::WorkflowState>) {
        self.mission.workflow = state;
        self.clamp_mission_selection();
    }

    /// L'usage ledger, par identifiant de session d'agent.
    pub fn apply_agent_usage(
        &mut self,
        usage: std::collections::HashMap<String, missioncontrol::AgentUsage>,
    ) {
        self.mission.usage = usage;
    }

    /// Toutes les touches du mission-control. T6 ne livre que la navigation :
    /// `x`, `p`, `s`, `g` et `Enter` appartiennent à T7 et sont ignorées ici
    /// plutôt que d'atterrir dans le composer derrière.
    fn mission_key(&mut self, key: &KeyEvent) -> Action {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return Action::None;
        }
        match key.code {
            KeyCode::Char('h') | KeyCode::Left => self.mission_move_stage(-1),
            KeyCode::Char('l') | KeyCode::Right => self.mission_move_stage(1),
            KeyCode::Char('j') | KeyCode::Down => self.mission_move_card(1),
            KeyCode::Char('k') | KeyCode::Up => self.mission_move_card(-1),
            KeyCode::Char('q') | KeyCode::Esc => self.close_mission_control(),
            _ => {}
        }
        Action::None
    }

    /// Changer de stage remet l'œil en tête de colonne : la carte 3 d'un stage
    /// n'a rien à voir avec la carte 3 du suivant.
    fn mission_move_stage(&mut self, delta: isize) {
        let last = self.mission_columns().saturating_sub(1) as isize;
        let target = self.mission.stage as isize + delta;
        let stage = target.clamp(0, last.max(0)) as usize;
        if stage != self.mission.stage {
            self.mission.card = 0;
        }
        self.mission.stage = stage;
    }

    fn mission_move_card(&mut self, delta: isize) {
        let last = self.mission_cards(self.mission.stage).saturating_sub(1) as isize;
        let target = self.mission.card as isize + delta;
        self.mission.card = target.clamp(0, last.max(0)) as usize;
    }

    fn mission_columns(&self) -> usize {
        match self.mission.workflow.as_ref() {
            Some(workflow) => workflow.stages.len(),
            None => 1,
        }
    }

    fn mission_cards(&self, stage: usize) -> usize {
        match self.mission.workflow.as_ref() {
            Some(workflow) => workflow
                .stages
                .get(stage)
                .map(|stage| stage.agents.len())
                .unwrap_or(0),
            None => self.forge.tasks.len(),
        }
    }

    /// Un workflow qui perd un stage, une vague de lames qui se range : la
    /// sélection suit le plateau plutôt que de désigner une carte disparue.
    fn clamp_mission_selection(&mut self) {
        self.mission.stage = self
            .mission
            .stage
            .min(self.mission_columns().saturating_sub(1));
        self.mission.card = self
            .mission
            .card
            .min(self.mission_cards(self.mission.stage).saturating_sub(1));
    }

    /// `Ctrl+O`: composer → explorer → viewer → forge → composer, skipping
    /// whatever is closed. Attaching from a pane hands the keyboard back to the
    /// composer, and without this chord nothing ever gave it back.
    pub fn cycle_focus(&mut self) {
        const ORDER: [Focus; 4] = [
            Focus::Composer,
            Focus::Explorer,
            Focus::Viewer,
            Focus::Forge,
        ];
        let start = ORDER.iter().position(|f| *f == self.focus).unwrap_or(0);
        for step in 1..=ORDER.len() {
            let next = ORDER[(start + step) % ORDER.len()];
            let open = match next {
                Focus::Composer => true,
                Focus::Explorer => self.explorer.is_some(),
                Focus::Viewer => self.viewer.is_some(),
                Focus::Forge => self.forge.visible(),
            };
            if open {
                self.focus = next;
                return;
            }
        }
    }

    /// Toutes les touches pendant que la forge a le focus. Comme l'explorateur
    /// c'est un volet, pas une surimpression : il répond ou il ignore, et un `j`
    /// égaré n'atterrit jamais dans le composer derrière.
    fn forge_key(&mut self, key: &KeyEvent) -> Action {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return Action::None;
        }
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.forge_move(1),
            KeyCode::Char('k') | KeyCode::Up => self.forge_move(-1),
            KeyCode::Enter => self.open_forge_sheet(),
            KeyCode::Char('x') => self.open_forge_cancel_confirm(),
            KeyCode::Char('f') => self.open_mission_control(),
            KeyCode::Esc => self.close_forge(),
            _ => {}
        }
        Action::None
    }

    /// La ligne 0 est la première lame : l'agent principal ouvre la liste sans
    /// être sélectionnable, il n'y a rien à lui demander.
    fn forge_move(&mut self, delta: isize) {
        let last = self.forge.tasks.len().saturating_sub(1) as isize;
        let target = self.forge.selected as isize + delta;
        self.forge.selected = target.clamp(0, last) as usize;
    }

    /// La fiche de la lame désignée, dans le lecteur : ce que la colonne du
    /// volet n'a pas la place de dire.
    fn open_forge_sheet(&mut self) {
        let Some((id, sheet)) = self
            .forge
            .selected_task()
            .map(|task| (task.id.clone(), forge_sheet(task, 0)))
        else {
            return;
        };
        self.viewer = Some(sheet);
        self.forge_sheet_open = Some(id);
        self.focus = Focus::Viewer;
    }

    /// La fiche est une vue, pas une copie : tant que le lecteur montre celle
    /// d'une lame encore listée, chaque tick la réécrit — titre compris, donc un
    /// renommage suit au lieu de geler. La position de lecture reste où l'œil
    /// l'a laissée.
    pub fn refresh_forge_sheet(&mut self) {
        let Some(sheet) = self.forge_sheet_open.as_ref().and_then(|id| {
            let task = self.forge.tasks.get(id)?;
            Some(forge_sheet(task, self.viewer.as_ref()?.scroll))
        }) else {
            return;
        };
        self.viewer = Some(sheet);
    }

    /// `x` : une lame se coupe, elle ne s'interrompt pas par accident — même
    /// patron que `/restore`, la question est posée avant que quoi que ce soit
    /// n'atteigne le summon.
    fn open_forge_cancel_confirm(&mut self) {
        let Some(task) = self.forge.selected_task() else {
            return;
        };
        if task.status != forge::ForgeStatus::Running {
            self.push_system("forge : tâche déjà terminée");
            return;
        }
        let id = task.id.clone();
        let question = format!(
            "annuler {} {} ? y/n",
            theme::SUBAGENT_GLYPH,
            task.description
        );
        self.push_system(&question);
        self.scroll_offset = 0;
        self.pending_forge_cancel = Some(id);
    }

    pub fn take_pending_forge_cancel(&mut self) -> Option<String> {
        self.pending_forge_cancel.take()
    }

    /// Every key while the explorer holds the focus. Like the viewer it is a
    /// pane rather than an overlay: it answers or ignores, and a stray `j`
    /// never lands in the composer behind it.
    fn explorer_key(&mut self, key: &KeyEvent) -> Action {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            if key.code == KeyCode::Char('p') {
                self.open_finder();
            }
            return Action::None;
        }
        if self.explorer.as_ref().is_some_and(|e| e.filtering) {
            self.explorer_filter_key(key);
            return Action::None;
        }
        match key.code {
            KeyCode::Char('l') | KeyCode::Right | KeyCode::Enter => {
                self.explorer_activate();
                return Action::None;
            }
            KeyCode::Char('e') => {
                let Some(path) = self.explorer_file_path() else {
                    return Action::None;
                };
                return self.request_edit(&path, None);
            }
            KeyCode::Char('a') => {
                if let Some(path) = self.explorer_mention_path() {
                    self.attach_mention(&path);
                    // Same bargain as the viewer's `a`: the pane stays open,
                    // the keyboard goes where the message is being written.
                    self.focus = Focus::Composer;
                }
                return Action::None;
            }
            KeyCode::Char('q') => {
                self.close_explorer();
                return Action::None;
            }
            KeyCode::Esc => {
                if self.explorer.as_ref().is_some_and(|e| !e.filter.is_empty()) {
                    if let Some(explorer) = self.explorer.as_mut() {
                        explorer.clear_filter();
                    }
                } else {
                    self.close_explorer();
                }
                return Action::None;
            }
            _ => {}
        }
        let Some(explorer) = self.explorer.as_mut() else {
            return Action::None;
        };
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => explorer.move_cursor(1),
            KeyCode::Char('k') | KeyCode::Up => explorer.move_cursor(-1),
            KeyCode::Char('h') | KeyCode::Left => explorer.collapse_or_parent(),
            KeyCode::Char('g') => explorer.cursor_to_start(),
            KeyCode::Char('G') => explorer.cursor_to_end(),
            KeyCode::Char('.') => explorer.toggle_hidden(),
            KeyCode::Char('R') => explorer.refresh(),
            KeyCode::Char('/') => explorer.start_filter(),
            _ => {}
        }
        Action::None
    }

    fn explorer_filter_key(&mut self, key: &KeyEvent) {
        let Some(explorer) = self.explorer.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Char(c) => explorer.push_filter(c),
            KeyCode::Backspace => explorer.pop_filter(),
            KeyCode::Down => explorer.move_cursor(1),
            KeyCode::Up => explorer.move_cursor(-1),
            KeyCode::Enter => explorer.end_filter(),
            KeyCode::Esc => explorer.clear_filter(),
            _ => {}
        }
    }

    /// `l`/`→`/`Enter`: a directory folds or unfolds in place, a file opens in
    /// the viewer and takes the focus with it.
    fn explorer_activate(&mut self) {
        let Some((is_dir, path)) = self.explorer.as_ref().and_then(|explorer| {
            explorer
                .selected()
                .filter(|node| node.overflow.is_none())
                .map(|node| (node.is_dir, node.path.clone()))
        }) else {
            return;
        };
        if is_dir {
            if let Some(explorer) = self.explorer.as_mut() {
                explorer.toggle_selected();
            }
        } else {
            self.open_viewer(&path);
        }
    }

    /// The selected row when it is a file — a directory has nothing to open
    /// in an editor, and the overflow row isn't a path at all.
    fn explorer_file_path(&self) -> Option<String> {
        self.explorer
            .as_ref()?
            .selected()
            .filter(|node| node.overflow.is_none() && !node.is_dir)
            .map(|node| node.path.clone())
    }

    fn explorer_mention_path(&self) -> Option<String> {
        self.explorer
            .as_ref()?
            .selected()
            .filter(|node| node.overflow.is_none())
            .map(crate::tui::explorer::Node::mention_path)
    }

    /// `Tab` on the finder and `a` on the viewer both land here: the path
    /// joins the draft as a real `@` mention — separated from what is already
    /// typed, since `foo@bar` mid-word is not a mention, and followed by a
    /// space so the next word doesn't glue onto it.
    pub fn attach_mention(&mut self, path: &str) {
        self.exit_history_navigation();
        self.reset_palette_selection();
        if !self.input.is_empty() && !self.input.ends_with(char::is_whitespace) {
            self.input.push(' ');
        }
        self.input.push('@');
        self.input.push_str(path);
        self.input.push(' ');
        self.reset_mention_state();
    }

    /// Bracketed paste (item 4 ante): a pasted path is auto-prefixed with `@`
    /// so it expands like a typed mention; anything else lands verbatim. The
    /// composer is single-line, so newlines collapse into spaces.
    fn insert_pasted(&mut self, text: &str) {
        let flattened = flatten_newlines(text);
        if flattened.is_empty() {
            return;
        }
        self.exit_history_navigation();
        self.reset_palette_selection();
        let trimmed = flattened.trim();
        if self.pasted_path_exists(trimmed) {
            self.input.push('@');
            self.input.push_str(trimmed);
        } else {
            self.input.push_str(&flattened);
        }
        // A pasted path is already complete — opening the dropdown on it
        // would only put a redundant match over the composer.
        self.reset_mention_state();
    }

    fn pasted_path_exists(&self, text: &str) -> bool {
        if text.is_empty() || text.starts_with('@') || text.chars().any(char::is_whitespace) {
            return false;
        }
        crate::tui::mentions::resolve(text, &self.working_dir).exists()
    }

    /// Called from every turn-begin path (`begin_setup`, covering Submit,
    /// GateApprove, and the chained exec→validate turn) — arms the loader
    /// zen and the thinking-merge chain for the new turn.
    pub fn reset_turn_visibility(&mut self) {
        self.turn_has_visible_output = false;
        self.turn_thinking_shown = false;
        self.turn_had_error = false;
        self.last_thinking_msg_id = None;
        self.agent_stream_idx = None;
        self.clear_suggestion();
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
        self.clear_suggestion();
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
        self.approval_detail = None;
        self.tool_approval.take()
    }

    /// Tab on the approval modal. Building the text here (rather than in the
    /// renderer) keeps the `write` file read off the per-frame draw path.
    pub fn toggle_approval_detail(&mut self) {
        self.approval_detail = match self.approval_detail {
            Some(_) => None,
            None => self
                .tool_approval
                .as_ref()
                .map(|req| req.detail_text(&self.working_dir)),
        };
    }

    /// Shift+Tab. Applies the new mode to `self` immediately and returns it —
    /// the caller owns the agent update and the config write.
    pub fn cycle_kaji_mode(&mut self) -> KajiMode {
        self.kaji_mode = next_kaji_mode(self.kaji_mode);
        self.push_system(&mode_line(self.kaji_mode));
        self.unfold_seal();
        self.kaji_mode
    }

    /// Déplie le mot du mode à côté du sceau : le kanji seul ne se traduit pas
    /// tout seul la première fois qu'on le voit.
    pub fn unfold_seal(&mut self) {
        self.seal_unfolded_until = Some(Instant::now() + SEAL_UNFOLD);
    }

    pub fn seal_unfolded(&self) -> bool {
        self.seal_unfolded_until
            .is_some_and(|until| until > Instant::now())
    }

    /// Ce que le feu de la barre d'état brûle : le dernier outil demandé qui
    /// attend encore sa réponse.
    pub fn current_tool(&self) -> Option<&str> {
        let idx = *self.pending_tools.values().max()?;
        Some(self.chat.get(idx)?.tool.as_ref()?.name.as_str())
    }

    /// Arms the `/restore <id>` y/n modal — mirrors `start_pass` pushing its
    /// "Intent : …" line before setting `gate_open`. Destructive by design
    /// (spec §3: "jamais automatique" — always confirmed): this only ever
    /// records intent, never touches the checkpoint store itself.
    pub fn open_restore_confirm(&mut self, id: String, files_only: bool) {
        if files_only {
            self.push_system(&format!(
                "restaurer le filet {id} ? l'arbre de travail sera rembobiné (fichiers seuls — la conversation ne sera pas touchée) — y/n"
            ));
        } else {
            self.push_system(&format!(
                "restaurer le checkpoint {id} ? l'arbre de travail et la conversation seront ramenés à cet état — y/n"
            ));
        }
        // The prompt was just pushed — make sure the user sees it even if
        // they had scrolled up.
        self.scroll_offset = 0;
        self.pending_restore = Some(id);
        self.pending_restore_files_only = files_only;
    }

    pub fn take_pending_restore(&mut self) -> Option<String> {
        self.pending_restore_files_only = false;
        self.pending_restore.take()
    }

    /// Accepts the current next-prompt ghost: moves it into `input` (ready to
    /// edit) and clears the ghost. No-op when there is nothing to accept.
    pub fn accept_suggestion(&mut self) {
        if let Some(text) = self.suggestion.take() {
            self.input = text;
            self.history_index = None;
            self.reset_palette_selection();
            self.update_mention_matches();
        }
        self.suggestion_loading = false;
    }

    /// Clears a stale ghost — called on every input edit and at the start
    /// of a new turn so a suggestion that outlived its turn can never be
    /// accepted against unrelated text.
    pub fn clear_suggestion(&mut self) {
        self.suggestion = None;
        self.suggestion_loading = false;
    }

    /// Replaces the chat view with `conversation`'s current messages. A
    /// restore just changed the session's real conversation (coupled:
    /// truncated at the boundary; net: untouched) — stale lines would make
    /// the screen claim a conversation the session no longer holds. The
    /// y/n prompt and any pre-restore chatter are dropped too; the honest
    /// success message is pushed by the caller *after* this call.
    pub fn reseed_chat(&mut self, conversation: &Conversation) {
        self.chat.clear();
        self.reset_agent_merge_ids();
        for message in conversation.messages() {
            self.apply_agent_event(&AgentEvent::Message(message.clone()));
        }
        self.close_orphaned_tool_requests();
    }

    /// A y/n modal (tool approval, gate, restore confirmation or forge
    /// cancellation) is on screen and owns the keyboard — the palette must
    /// not open underneath it, and the plain arrows must fall back to chat
    /// scroll instead of history recall (see `on_event`'s `modal_active`
    /// local).
    pub fn modal_active(&self) -> bool {
        self.tool_approval.is_some()
            || self.gate_open
            || self.pending_restore.is_some()
            || self.pending_forge_cancel.is_some()
    }

    /// Commands whose name starts with the current input, trimmed — empty
    /// whenever the trimmed input doesn't start with `/` (including the
    /// empty input), which is also what makes `palette_visible` false with
    /// nothing typed. Trimming here must match the trim the Enter dispatch
    /// applies to the legacy submit path, or the palette can disappear
    /// (surrounding whitespace breaking the prefix match) while Enter still
    /// executes the command the user can no longer see selected — the UI
    /// would visually promise a plain message send and then run a command.
    pub fn palette_matches(&self) -> Vec<&'static Command> {
        let trimmed = self.input.trim();
        if !trimmed.starts_with('/') {
            return Vec::new();
        }
        COMMANDS
            .iter()
            .filter(|c| c.name.starts_with(trimmed))
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
            // The finder and the pickers are overlays: the wheel under them
            // would scroll a chat the overlay is covering.
            if self.finder.is_some() || self.theme_picker.is_some() || self.editor_picker.is_some()
            {
                return Action::None;
            }
            // The wheel follows the pointer: over the file viewer it scrolls
            // the file, anywhere else the chat.
            let on_viewer = self.point_in_viewer(mouse.column, mouse.row);
            let viewport = self.viewer_viewport();
            // A folded chat is not on screen to be scrolled: off the reader,
            // the wheel does nothing rather than move a pane nobody can see.
            let zoomed = self.zoomed_viewer().is_some();
            match mouse.kind {
                MouseEventKind::ScrollUp if on_viewer => {
                    if let Some(viewer) = self.viewer.as_mut() {
                        viewer.scroll_up(SCROLL_WHEEL as usize);
                    }
                }
                MouseEventKind::ScrollDown if on_viewer => {
                    if let Some(viewer) = self.viewer.as_mut() {
                        viewer.scroll_down(SCROLL_WHEEL as usize, viewport);
                    }
                }
                MouseEventKind::ScrollUp if !zoomed => self.scroll_wheel_up(),
                MouseEventKind::ScrollDown if !zoomed => self.scroll_wheel_down(),
                _ => {}
            }
            return Action::None;
        }
        // A y/n modal owns the keyboard: a paste landing behind it would
        // mutate the composer the user can't see. So would one landing behind
        // the finder, the theme picker or the viewer — the finder takes it as
        // query text, the other two drop it.
        if let Event::Paste(text) = ev {
            if !self.modal_active() {
                if self.finder.is_some() {
                    let pasted: String = flatten_newlines(text)
                        .chars()
                        .filter(|c| !c.is_control())
                        .collect();
                    self.finder_insert(&pasted);
                } else if self.theme_picker.is_none()
                    && self.editor_picker.is_none()
                    && self.focus == Focus::Composer
                {
                    self.insert_pasted(text);
                }
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
        // File navigation (task 8) sits between Ctrl+C and everything else: an
        // open finder or a focused viewer consumes the key before the global
        // scroll bindings and the composer see it, but never before a y/n
        // modal — an approval on screen still answers y/n.
        if !self.modal_active() {
            if self.finder.is_some() {
                return self.finder_key(key);
            }
            // Same contract for the theme picker; the two can't be open at
            // once since whichever is open swallows the key that would open
            // the other.
            if self.theme_picker.is_some() {
                return self.theme_picker_key(key);
            }
            if self.editor_picker.is_some() {
                return self.editor_picker_key(key);
            }
            // Le mission-control est une vue plein écran, pas un volet : tant
            // qu'il est ouvert il prend la touche avant les accords de volets
            // — `Ctrl+F` replierait une forge qu'on ne verrait plus. Les
            // modales restent au-dessus, elles : une approbation à l'écran
            // répond toujours y/n.
            if self.mission.open {
                return self.mission_key(key);
            }
            // The pane chords sit above the panes themselves: `Ctrl+E` has to
            // reach the explorer from inside the viewer, and `Ctrl+O` has to
            // rotate out of whichever pane currently owns the keyboard.
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                match key.code {
                    KeyCode::Char('e') => {
                        self.toggle_explorer();
                        return Action::None;
                    }
                    KeyCode::Char('f') => {
                        self.toggle_forge();
                        return Action::None;
                    }
                    KeyCode::Char('o') => {
                        self.cycle_focus();
                        return Action::None;
                    }
                    _ => {}
                }
            }
            if self.focus == Focus::Viewer {
                return self.viewer_key(key);
            }
            if self.focus == Focus::Explorer {
                return self.explorer_key(key);
            }
            if self.focus == Focus::Forge {
                // Le repli automatique ne demande la permission à personne : le
                // volet peut disparaître sous le focus, et la touche qui le
                // découvre appartient au composer.
                if self.forge.visible() {
                    return self.forge_key(key);
                }
                self.focus = Focus::Composer;
            }
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
                if self.mention_dropdown_visible() {
                    let n = self.mention_matches.len();
                    self.mention_selected = (self.mention_selected + n - 1) % n;
                } else if self.palette_visible() {
                    let n = self.palette_matches().len();
                    self.palette_selected = (self.palette_selected + n - 1) % n;
                } else if !modal_active && self.mouse_enabled {
                    self.history_prev();
                    self.update_mention_matches();
                } else {
                    self.scroll_line_up();
                }
                return Action::None;
            }
            KeyCode::Down => {
                if self.mention_dropdown_visible() {
                    let n = self.mention_matches.len();
                    self.mention_selected = (self.mention_selected + 1) % n;
                } else if self.palette_visible() {
                    let n = self.palette_matches().len();
                    self.palette_selected = (self.palette_selected + 1) % n;
                } else if !modal_active && self.mouse_enabled {
                    self.history_next();
                    self.update_mention_matches();
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
            // Chorded letters belong to the bindings below (Ctrl+S flushes the
            // steer queue, Ctrl+W deletes a word): reading them as answers
            // would let an unrelated reflex grant a session-wide permission.
            if key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
            {
                return Action::None;
            }
            return match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    Action::ToolAnswer(Permission::AllowOnce)
                }
                KeyCode::Char('s') | KeyCode::Char('S') => {
                    Action::ToolAnswer(Permission::AllowSession)
                }
                KeyCode::Char('a') | KeyCode::Char('A') => {
                    Action::ToolAnswer(Permission::AlwaysAllow)
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    Action::ToolAnswer(Permission::DenyOnce)
                }
                KeyCode::Tab => {
                    self.toggle_approval_detail();
                    Action::None
                }
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
        if self.pending_restore.is_some() {
            return match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => Action::RestoreConfirm,
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => Action::RestoreCancel,
                _ => Action::None,
            };
        }
        // Une lame vit sa vie : la question n'attend pas, et tout ce qui n'est
        // pas un `y` franc la laisse tourner.
        if self.pending_forge_cancel.is_some() {
            if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                return Action::ForgeCancel;
            }
            self.pending_forge_cancel = None;
            return Action::None;
        }
        match key.code {
            KeyCode::Backspace
                if key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.delete_last_word();
                self.update_mention_matches();
                Action::None
            }
            KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.delete_last_word();
                self.update_mention_matches();
                Action::None
            }
            // Steering flush (item 2 ante): `Ctrl+S` while a turn is running
            // (or with messages queued) — `event_loop` cancels the running
            // turn and submits the queued message as live guidance.
            KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if self.steer_queue.is_empty() {
                    self.push_system(
                        "rien en file — tape un message pendant un tour pour le steer",
                    );
                    Action::None
                } else {
                    Action::SteerNow
                }
            }
            // Fuzzy file finder (task 8) — before the plain `Char(c)` arm, or
            // the chord would type a `p` into the composer.
            KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.open_finder();
                Action::None
            }
            // Mode ramp (item 3 ante): the modal gates above already returned,
            // and both open lists own Shift+Tab's sibling Tab — leave the chord
            // alone there rather than switching modes under a list the user is
            // navigating.
            KeyCode::BackTab if !self.palette_visible() && !self.mention_dropdown_visible() => {
                Action::Mode(self.cycle_kaji_mode())
            }
            KeyCode::Char(c) => {
                self.exit_history_navigation();
                self.reset_palette_selection();
                self.input.push(c);
                self.update_mention_matches();
                Action::None
            }
            KeyCode::Backspace => {
                self.exit_history_navigation();
                self.reset_palette_selection();
                self.input.pop();
                self.update_mention_matches();
                Action::None
            }
            // @-mention dropdown (item 4 ante): Tab completes the selected
            // path into the input, replacing the live fragment. Before the
            // palette arm — a `/`-prefixed palette and a `@` fragment can't
            // be live at once, but the ordering keeps the intent explicit.
            KeyCode::Tab if self.mention_dropdown_visible() => {
                self.apply_mention_completion();
                Action::None
            }
            KeyCode::Tab if self.palette_visible() => {
                let matches = self.palette_matches();
                let name = matches[self.palette_selected.min(matches.len() - 1)].name;
                self.input = name.to_string();
                self.exit_history_navigation();
                self.reset_palette_selection();
                self.update_mention_matches();
                Action::None
            }
            // Next-prompt ghost (item 7): Tab accepts the suggestion into the
            // input when the palette is closed, the input is empty and a
            // suggestion is ready — it reads as "Tab fills the blank".
            KeyCode::Tab if self.suggestion.is_some() && self.input.is_empty() => {
                self.accept_suggestion();
                Action::None
            }
            KeyCode::Esc if self.palette_visible() => {
                self.input.clear();
                self.reset_palette_selection();
                self.update_mention_matches();
                Action::None
            }
            // Esc on the mention dropdown dismisses it without touching the
            // input — the fragment stays, any edit re-arms completion.
            KeyCode::Esc if self.mention_dropdown_visible() => {
                self.reset_mention_state();
                self.mention_suppressed = true;
                Action::None
            }
            KeyCode::Esc if self.turn_active || self.turn_pending => Action::CancelTurn,
            // Enter on the mention dropdown confirms the selected path into
            // the input (it does NOT submit) — ante: "Tab to autocomplete,
            // then Enter to confirm". Before the mid-turn steer guard so a
            // completion mid-turn doesn't queue a half-typed fragment.
            KeyCode::Enter if self.mention_dropdown_visible() => {
                self.apply_mention_completion();
                Action::None
            }
            // Steering (item 2 ante): while a turn is running, Enter queues
            // the draft instead of dropping it — `Ctrl+S` flushes it into the
            // running turn, and anything still queued auto-submits at turn
            // end. `/restore` carves itself out of the blanket queue below:
            // it needs to reach the general Enter arm even mid-turn so it can
            // push its OWN barrier message (premortem PM6) instead of the
            // queue path. `/goal` does the same, for `/goal clear`: stopping
            // a goal loop mid-turn must not wait for the turn it is stopping.
            // `/edit` too: queued, it would reach the model as a message
            // instead of being refused the way `e` is. Every other
            // command/message still gets the queue.
            KeyCode::Enter
                if (self.turn_active || self.turn_pending)
                    && restore_command_arg(self.input.trim()).is_none()
                    && goal_command_arg(self.input.trim()).is_none()
                    && edit_command_arg(self.input.trim()).is_none() =>
            {
                let text = self.input.trim().to_string();
                self.input.clear();
                self.reset_mention_state();
                if text.is_empty() {
                    Action::None
                } else {
                    let depth = self.queue_steer(&text);
                    // A driver relaunches a turn from `turn_end`, which is
                    // exactly where the auto-flush would have run — under a
                    // goal or a pass the queue waits for the whole loop.
                    let when = if self.driver == PassDriver::Idle
                        && self.goal_driver() == GoalDriver::Idle
                    {
                        "envoi auto en fin de tour"
                    } else {
                        "envoi à la fin de la boucle (but/passe en cours)"
                    };
                    self.push_system(&format!(
                        "{} mis en file ({depth}) — Ctrl+S pour steer, {when}",
                        theme::STEER_GLYPH
                    ));
                    Action::None
                }
            }
            KeyCode::Enter => {
                if self.palette_visible() {
                    let matches = self.palette_matches();
                    let cmd = matches[self.palette_selected.min(matches.len() - 1)];
                    self.input.clear();
                    self.reset_palette_selection();
                    self.reset_mention_state();
                    self.push_history(cmd.name);
                    return cmd.run(self);
                }
                let text = std::mem::take(&mut self.input);
                self.reset_mention_state();
                let text = text.trim().to_string();
                if text.is_empty() {
                    Action::None
                } else {
                    self.push_history(&text);
                    if let Some(arg) = restore_command_arg(&text) {
                        if arg.is_empty() {
                            self.push_system("usage : /restore <id>");
                            return Action::None;
                        }
                        // premortem PM6: the store's bare-repo index is not
                        // safe under concurrent git ops — refuse rather than
                        // let a restore race a snapshot still in flight.
                        if self.turn_active || self.turn_pending {
                            self.push_system("termine ou annule le tour avant de restaurer");
                            return Action::None;
                        }
                        return Action::Restore(arg.to_string());
                    }
                    if let Some(arg) = goal_command_arg(&text) {
                        let arg = arg.to_string();
                        return self.run_goal_command(&arg);
                    }
                    if let Some(arg) = cost_command_arg(&text) {
                        return match report::CostView::parse(arg) {
                            Some(view) => Action::Cost(view),
                            None => {
                                self.push_system(report::CostView::usage());
                                Action::None
                            }
                        };
                    }
                    if let Some(arg) = theme_command_arg(&text) {
                        return self.run_theme_command(arg);
                    }
                    if let Some(arg) = editor_command_arg(&text) {
                        return self.run_editor_command(arg);
                    }
                    if let Some(arg) = edit_command_arg(&text) {
                        return self.run_edit_command(arg);
                    }
                    if let Some(arg) = forge_command_arg(&text) {
                        return self.run_forge_command(arg);
                    }
                    // Unreachable today: any trimmed input that names a command
                    // keeps the palette visible (see `palette_matches`), so this
                    // branch never runs for typed input — kept as a safety net
                    // against a future `input` mutation path that bypasses the
                    // palette state (e.g. a programmatic/paste submit).
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

    /// `/goal` : argument vide = statut, `clear` = arrêt, sinon la condition
    /// à poursuivre. Fixer un but demande un tour libre (le premier prompt de
    /// travail en ouvre un) ; `clear` est justement ce qui libère le tour.
    fn run_goal_command(&mut self, arg: &str) -> Action {
        if arg.is_empty() {
            return Action::GoalStatus;
        }
        if arg.eq_ignore_ascii_case("clear") {
            return if self.goal_clear() {
                Action::GoalClear
            } else {
                Action::None
            };
        }
        if self.turn_active || self.turn_pending {
            self.push_system("un tour est en cours — attends la fin, ou /goal clear");
            return Action::None;
        }
        Action::GoalSet(arg.to_string())
    }

    /// `/theme` : argument vide ou `list` = sélecteur avec aperçu en direct,
    /// `next` = palette suivante du cycle, sinon le nom donné. Un nom inconnu
    /// laisse la palette intacte et renvoie la liste des noms disponibles.
    fn run_theme_command(&mut self, arg: &str) -> Action {
        if arg.is_empty() || arg.eq_ignore_ascii_case("list") {
            self.open_theme_picker();
            return Action::None;
        }
        let requested = if arg.eq_ignore_ascii_case("next") {
            theme::next_name(theme::active().name)
        } else {
            arg
        };
        match theme::set_active(requested) {
            Ok(()) => {
                let applied = theme::active().name;
                self.push_system(&format!("thème : {applied}"));
                Action::Theme(applied.to_string())
            }
            Err(err) => {
                self.push_system(&err.to_string());
                Action::None
            }
        }
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

    /// Same as [`Self::push_system`] but with pre-rendered blocks (aligned
    /// tables) whose spans carry a [`SpanRole`] — used for on-demand report
    /// blocks (`/cost`, `/docker`) that need theme styling the plain system
    /// register can't express.
    pub fn push_system_lines(&mut self, lines: Vec<RoledLine>) {
        let text = lines
            .iter()
            .map(|spans| {
                spans
                    .iter()
                    .map(|span| span.text.as_str())
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

    /// Provider/LLM failures: same placement as a system notice, in the
    /// theme's alert colour so a failed turn doesn't read as chatter. The
    /// message already opens with the taxonomy label (see
    /// `Message::from_provider_error`).
    pub fn push_error(&mut self, message: &str) {
        let lines = message
            .lines()
            .enumerate()
            .map(|(index, line)| {
                let text = if index == 0 {
                    format!("✗ {line}")
                } else {
                    line.to_string()
                };
                vec![RoledSpan::error(text)]
            })
            .collect();
        self.push_system_lines(lines);
    }

    pub fn apply_agent_event(&mut self, ev: &AgentEvent) {
        match ev {
            AgentEvent::Message(message) => self.apply_message(message),
            AgentEvent::Usage(usage) => self.apply_usage(usage),
            // MessageUsage carries the same round-trip totals already seen via
            // Usage (both derive from the same provider response) — accumulating
            // both would double the token tally shown in the header.
            AgentEvent::MessageUsage { .. } => {}
            AgentEvent::McpNotification((_, notification)) => {
                self.apply_mcp_notification(notification)
            }
            AgentEvent::HistoryReplaced(_) => self.push_system("— historique compacté —"),
        }
    }

    /// La seule notification MCP que la TUI lit : `subagent_tool_request`, qui
    /// dit quel outil la lame déléguée vient de demander. Le payload vient d'un
    /// serveur, jamais de nous — tout champ manquant le fait passer sans bruit.
    #[expect(deprecated)]
    fn apply_mcp_notification(&mut self, notification: &ServerNotification) {
        let ServerNotification::LoggingMessageNotification(log) = notification else {
            return;
        };
        let data = &log.params.data;
        if data.get("type").and_then(|value| value.as_str()) != Some(SUBAGENT_TOOL_REQUEST_TYPE) {
            return;
        }
        let Some(subagent_id) = data.get("subagent_id").and_then(|value| value.as_str()) else {
            return;
        };
        let Some(tool_name) = data
            .get("tool_call")
            .and_then(|call| call.get("name"))
            .and_then(|name| name.as_str())
        else {
            return;
        };
        self.forge.apply_tool_notification(subagent_id, tool_name);
        // La notification arrive entre deux snapshots : sans ça, la fiche
        // ouverte annoncerait l'outil du tick précédent jusqu'au suivant.
        self.refresh_forge_sheet();
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
                    if self.goal_driver() == GoalDriver::Evaluating {
                        self.goal_buffer.push_str(&text.text);
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
                        text: format!("{} {name}", theme::TOOL_GLYPH),
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
                        arguments,
                        prompt,
                    } = &action.data
                    {
                        self.approval_detail = None;
                        self.tool_approval = Some(ToolApprovalRequest {
                            id: id.clone(),
                            tool_name: tool_name.clone(),
                            arguments: arguments.clone(),
                            prompt: prompt.clone(),
                        });
                    }
                }
                MessageContentBlock::Error(error) => {
                    self.turn_had_error = true;
                    self.push_error(&error.message);
                }
                // Thinking/progress notifications drive the loader, not the
                // transcript — only the two user-facing registers are shown.
                MessageContentBlock::SystemNotification(notification) => {
                    if matches!(
                        notification.notification_type,
                        SystemNotificationType::InlineMessage
                            | SystemNotificationType::CreditsExhausted
                    ) {
                        self.push_system(&notification.msg);
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
        if let Some(req) = self.take_tool_approval() {
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

/// Largeur de repli de la prose de la fiche. Le lecteur peut être plus large,
/// mais une ligne qui traverse un écran entier ne se lit plus.
const FORGE_SHEET_WRAP: usize = 76;

/// Largeur du champ de gauche : tous les intitulés tiennent dedans, donc les
/// valeurs commencent à la même colonne.
const FORGE_SHEET_LABEL: usize = 9;

/// Ce qu'un titre de fiche garde d'une description, en cellules — au-delà il
/// déborderait de l'en-tête du lecteur.
const FORGE_SHEET_TITLE: usize = 60;

/// L'en-tête de la fiche dans le lecteur. C'est un titre, pas un chemin, et
/// pas non plus une clé : [`App::refresh_forge_sheet`] suit l'id de la tâche,
/// donc un renommage se contente de réécrire cette ligne.
fn forge_sheet_title(description: &str) -> String {
    let head = gitstatus::truncate_cells(
        &sanitize_for_display(&description.replace('\n', "␊")),
        FORGE_SHEET_TITLE,
    );
    format!("{} {head}", theme::SUBAGENT_GLYPH)
}

fn forge_sheet(task: &forge::ForgeTask, scroll: usize) -> crate::tui::viewer::Viewer {
    let mut lines = forge_sheet_field("tâche", &task.description);
    lines.extend(forge_sheet_field(
        "statut",
        &format!(
            "{} · {}",
            forge_status_label(task.status),
            crate::tui::ui::forge_duration(task.elapsed_secs)
        ),
    ));
    lines.extend(forge_sheet_field("tours", &task.turns.to_string()));
    if let Some(tool) = task.current_tool.as_deref() {
        lines.extend(forge_sheet_field("outil", tool));
    }
    if let Some((label, body)) = forge_sheet_verdict(task) {
        lines.push(String::new());
        lines.push(format!("{label:<FORGE_SHEET_LABEL$}:"));
        // Rien ne promet qu'un agent replie ce qu'il rend : une erreur d'une
        // seule ligne de mille caractères, le lecteur la rogne en silence.
        for line in body.lines() {
            lines.extend(wrap_words(line, FORGE_SHEET_WRAP));
        }
    }
    crate::tui::viewer::Viewer {
        path: forge_sheet_title(&task.description),
        lines: lines
            .iter()
            .map(|line| sanitize_for_display(line))
            .collect(),
        scroll,
        truncated: false,
        binary: false,
    }
}

/// Ce qu'une lame a laissé derrière elle — l'erreur d'abord : un échec dont on
/// lirait le résultat partiel avant la cause se raconterait à l'envers.
fn forge_sheet_verdict(task: &forge::ForgeTask) -> Option<(&'static str, &str)> {
    match (task.error.as_deref(), task.result.as_deref()) {
        (Some(error), _) => Some(("erreur", error)),
        (None, Some(result)) => Some(("résultat", result)),
        (None, None) => None,
    }
}

fn forge_status_label(status: forge::ForgeStatus) -> &'static str {
    match status {
        forge::ForgeStatus::Running => "en cours",
        forge::ForgeStatus::Done => "terminé",
        forge::ForgeStatus::Failed => "échec",
        forge::ForgeStatus::Cancelled => "annulé",
    }
}

fn forge_sheet_field(label: &str, value: &str) -> Vec<String> {
    let indent = " ".repeat(FORGE_SHEET_LABEL + 2);
    wrap_words(value, FORGE_SHEET_WRAP)
        .into_iter()
        .enumerate()
        .map(|(rank, chunk)| {
            if rank == 0 {
                format!("{label:<FORGE_SHEET_LABEL$}: {chunk}")
            } else {
                format!("{indent}{chunk}")
            }
        })
        .collect()
}

/// Repli sur les espaces, à la cellule : une prose japonaise repliée sur des
/// `chars()` rend des lignes deux fois trop larges, que le lecteur rogne
/// ensuite en silence. Un mot plus long que la largeur part seul sur sa ligne :
/// le lecteur rogne, il ne coupe pas un chemin en deux.
fn wrap_words(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;
    for word in text.split_whitespace() {
        let word_width = gitstatus::display_width(word);
        if current.is_empty() {
            current = word.to_string();
            current_width = word_width;
        } else if current_width + 1 + word_width <= width {
            current.push(' ');
            current.push_str(word);
            current_width += 1 + word_width;
        } else {
            lines.push(std::mem::take(&mut current));
            current = word.to_string();
            current_width = word_width;
        }
    }
    lines.push(current);
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaji::agents::{SubagentTaskSnapshot, SubagentTaskStatus};
    use kaji::conversation::message::{Message, MessageErrorKind};
    use kaji::providers::base::Usage;
    use ratatui::crossterm::event::{
        Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
    };
    use rmcp::model::{CallToolRequestParams, CallToolResult};
    use rmcp::object;
    use test_case::test_case;

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
    fn slash_checkpoints_returns_the_list_action() {
        let mut app = App::new(None);
        for c in "/checkpoints".chars() {
            app.on_event(&key(KeyCode::Char(c)));
        }
        assert_eq!(app.on_event(&key(KeyCode::Enter)), Action::Checkpoints);
    }

    #[test]
    fn slash_restore_with_id_parses_the_argument() {
        let mut app = App::new(None);
        for c in "/restore a1b2c3".chars() {
            app.on_event(&key(KeyCode::Char(c)));
        }
        assert_eq!(
            app.on_event(&key(KeyCode::Enter)),
            Action::Restore("a1b2c3".to_string())
        );
    }

    fn submit(app: &mut App, text: &str) -> Action {
        for c in text.chars() {
            app.on_event(&key(KeyCode::Char(c)));
        }
        app.on_event(&key(KeyCode::Enter))
    }

    #[test]
    fn slash_theme_next_cycles_to_the_next_palette() {
        let _theme = theme::test_guard();
        let mut app = App::new(None);

        let action = submit(&mut app, "/theme next");

        assert_eq!(action, Action::Theme("light".to_string()));
        assert_eq!(theme::active().name, "light");
        assert!(app.chat.iter().any(|l| l.text.contains("thème : light")));
    }

    #[test]
    fn slash_theme_opens_the_picker_on_the_active_palette() {
        let _theme = theme::test_guard();
        theme::set_active("nord").expect("nord is a built-in theme");
        let mut app = App::new(None);

        let action = submit(&mut app, "/theme");

        assert_eq!(action, Action::None);
        let picker = app.theme_picker.as_ref().expect("le sélecteur est ouvert");
        assert_eq!(theme::THEMES[picker.selected].name, "nord");
        assert_eq!(picker.initial, picker.selected);
        assert_eq!(theme::active().name, "nord", "ouvrir ne change rien");
        assert!(app.chat.is_empty(), "ouvrir n'écrit aucune ligne système");
    }

    #[test]
    fn theme_picker_navigation_previews_the_palette_without_committing() {
        let _theme = theme::test_guard();
        theme::set_active("zen").expect("zen is a built-in theme");
        let mut app = App::new(None);
        submit(&mut app, "/theme");

        let action = app.on_event(&key(KeyCode::Down));

        assert_eq!(action, Action::None, "l'aperçu ne persiste rien");
        assert_eq!(theme::active().name, "light", "aperçu appliqué en direct");
        assert!(app.theme_picker.is_some(), "le sélecteur reste ouvert");
        assert!(app.chat.is_empty(), "aucune ligne système pendant l'aperçu");
    }

    #[test]
    fn theme_picker_enter_keeps_the_previewed_palette() {
        let _theme = theme::test_guard();
        theme::set_active("zen").expect("zen is a built-in theme");
        let mut app = App::new(None);
        submit(&mut app, "/theme");
        app.on_event(&key(KeyCode::Down));

        let action = app.on_event(&key(KeyCode::Enter));

        assert_eq!(action, Action::Theme("light".to_string()));
        assert_eq!(theme::active().name, "light");
        assert!(app.theme_picker.is_none(), "le sélecteur se ferme");
        assert!(app.chat.iter().any(|l| l.text.contains("thème : light")));
    }

    #[test]
    fn theme_picker_enter_without_moving_validates_the_active_palette() {
        let _theme = theme::test_guard();
        theme::set_active("nord").expect("nord is a built-in theme");
        let mut app = App::new(None);
        submit(&mut app, "/theme");

        let action = app.on_event(&key(KeyCode::Enter));

        assert_eq!(action, Action::Theme("nord".to_string()));
        assert!(app.chat.iter().any(|l| l.text.contains("thème : nord")));
    }

    #[test]
    fn theme_picker_esc_restores_the_palette_it_opened_on() {
        let _theme = theme::test_guard();
        theme::set_active("zen").expect("zen is a built-in theme");
        let mut app = App::new(None);
        submit(&mut app, "/theme");
        app.on_event(&key(KeyCode::Down));
        app.on_event(&key(KeyCode::Down));
        assert_eq!(theme::active().name, "nord", "l'aperçu a bien bougé");

        let action = app.on_event(&key(KeyCode::Esc));

        assert_eq!(action, Action::None);
        assert_eq!(theme::active().name, "zen", "Esc rend la palette d'origine");
        assert!(app.theme_picker.is_none(), "le sélecteur se ferme");
        assert!(app.chat.is_empty(), "annuler n'écrit aucune ligne système");
    }

    #[test]
    fn theme_picker_navigation_wraps_at_both_ends() {
        let _theme = theme::test_guard();
        theme::set_active("zen").expect("zen is a built-in theme");
        let mut app = App::new(None);
        submit(&mut app, "/theme");

        app.on_event(&key(KeyCode::Up));
        assert_eq!(theme::active().name, "mono", "↑ depuis le premier boucle");

        app.on_event(&key(KeyCode::Down));
        assert_eq!(theme::active().name, "zen", "↓ depuis le dernier boucle");

        app.on_event(&key(KeyCode::End));
        assert_eq!(theme::active().name, "mono");

        app.on_event(&key(KeyCode::Home));
        assert_eq!(theme::active().name, "zen");
    }

    #[test]
    fn the_theme_picker_swallows_every_key_it_does_not_use() {
        let _theme = theme::test_guard();
        let mut app = App::new(None);
        submit(&mut app, "/theme");

        app.on_event(&key(KeyCode::Char('x')));
        app.on_event(&ctrl_key(KeyCode::Char('p')));
        app.on_event(&ctrl_key(KeyCode::Char('e')));

        assert!(app.input.is_empty(), "rien ne fuit dans le composer");
        assert!(app.finder.is_none(), "Ctrl+P est inerte sous le sélecteur");
        assert!(
            app.explorer.is_none(),
            "Ctrl+E est inerte sous le sélecteur"
        );
        assert!(app.theme_picker.is_some());
    }

    #[test]
    fn slash_theme_list_opens_the_picker() {
        let _theme = theme::test_guard();
        let mut app = App::new(None);

        let action = submit(&mut app, "/theme list");

        assert_eq!(action, Action::None);
        assert!(app.theme_picker.is_some(), "list ouvre le même sélecteur");
    }

    #[test]
    fn slash_theme_with_a_name_applies_that_palette() {
        let _theme = theme::test_guard();
        let mut app = App::new(None);

        let action = submit(&mut app, "/theme nord");

        assert_eq!(action, Action::Theme("nord".to_string()));
        assert_eq!(theme::active().name, "nord");
        assert!(app.chat.iter().any(|l| l.text.contains("thème : nord")));
    }

    #[test]
    fn slash_theme_with_an_unknown_name_lists_the_available_themes() {
        let _theme = theme::test_guard();
        let mut app = App::new(None);

        let action = submit(&mut app, "/theme xyz");

        assert_eq!(action, Action::None);
        assert_eq!(theme::active().name, "zen", "the palette must not change");
        let error = app
            .chat
            .iter()
            .find(|l| l.text.contains("xyz"))
            .expect("an error line naming the rejected theme");
        for palette in &theme::THEMES {
            assert!(error.text.contains(palette.name), "{}", error.text);
        }
    }

    #[test]
    fn theme_command_arg_keeps_theme_a_whole_word() {
        assert_eq!(theme_command_arg("/theme"), Some(""));
        assert_eq!(theme_command_arg("/theme nord"), Some("nord"));
        assert_eq!(theme_command_arg("/themex"), None);
        assert_eq!(theme_command_arg("/restore a1"), None);
    }

    /// ⛔ BARRIÈRE premortem PM6 — /restore refusé pendant un tour actif
    /// (l'index du store et l'arbre ne doivent pas bouger sous un tour).
    #[test]
    fn restore_is_refused_while_a_turn_is_active() {
        let mut app = App::new(None);
        app.turn_active = true;
        for c in "/restore a1b2c3".chars() {
            app.on_event(&key(KeyCode::Char(c)));
        }
        let action = app.on_event(&key(KeyCode::Enter));
        assert_eq!(action, Action::None);
        assert!(app
            .chat
            .iter()
            .any(|l| l.text.contains("termine ou annule le tour")));
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
    fn enter_during_active_turn_queues_steer() {
        let mut app = App::new(None);
        app.turn_active = true;
        app.on_event(&key(KeyCode::Char('h')));
        app.on_event(&key(KeyCode::Char('i')));
        let action = app.on_event(&key(KeyCode::Enter));
        assert_eq!(action, Action::None);
        assert_eq!(app.input, "");
        assert_eq!(app.steer_len(), 1);
        assert_eq!(app.next_steer().as_deref(), Some("hi"));
        assert!(app.chat.iter().any(|l| l.text.contains("mis en file")
            && l.text.contains("Ctrl+S")
            && l.text.contains("fin de tour")));
    }

    /// Sous un but (ou une passe), le tour qui se termine en relance un autre :
    /// la file n'est drainée qu'à la fin de la boucle, pas à la fin du tour.
    #[test]
    fn a_steer_queued_under_a_goal_announces_the_end_of_the_loop() {
        let mut app = App::new(None);
        app.goal_set("les tests passent", 10);
        app.turn_active = true;
        app.on_event(&key(KeyCode::Char('x')));

        assert_eq!(app.on_event(&key(KeyCode::Enter)), Action::None);

        let line = &app.chat.last().expect("ligne de mise en file").text;
        assert!(line.contains("fin de la boucle"), "{line}");
    }

    #[test]
    fn esc_during_pending_setup_returns_cancel_turn() {
        let mut app = App::new(None);
        app.turn_pending = true;
        assert_eq!(app.on_event(&key(KeyCode::Esc)), Action::CancelTurn);
    }

    #[test]
    fn enter_during_pending_setup_queues_steer() {
        let mut app = App::new(None);
        app.turn_pending = true;
        app.on_event(&key(KeyCode::Char('h')));
        app.on_event(&key(KeyCode::Char('i')));
        let action = app.on_event(&key(KeyCode::Enter));
        assert_eq!(action, Action::None);
        assert_eq!(app.input, "");
        assert_eq!(app.steer_len(), 1);
    }

    #[test]
    fn ctrl_s_with_queue_returns_steer_now() {
        let mut app = App::new(None);
        app.turn_active = true;
        app.queue_steer("use the helper");
        assert_eq!(
            app.on_event(&ctrl_key(KeyCode::Char('s'))),
            Action::SteerNow
        );
    }

    #[test]
    fn ctrl_s_without_queue_is_noop_with_hint() {
        let mut app = App::new(None);
        app.turn_active = true;
        assert_eq!(app.on_event(&ctrl_key(KeyCode::Char('s'))), Action::None);
        assert_eq!(app.steer_len(), 0);
        assert!(app.chat.iter().any(|l| l.text.contains("rien en file")));
    }

    #[test]
    fn ctrl_s_when_idle_returns_steer_now_if_queue_has_items() {
        let mut app = App::new(None);
        app.queue_steer("resume");
        assert_eq!(
            app.on_event(&ctrl_key(KeyCode::Char('s'))),
            Action::SteerNow
        );
    }

    #[test]
    fn steer_queue_is_fifo() {
        let mut app = App::new(None);
        app.queue_steer("first");
        app.queue_steer("second");
        assert_eq!(app.steer_len(), 2);
        assert_eq!(app.next_steer().as_deref(), Some("first"));
        assert_eq!(app.next_steer().as_deref(), Some("second"));
        assert_eq!(app.next_steer(), None);
    }

    /// Fixture: an App rooted on a tempdir instead of the process cwd, so
    /// completion is hermetic. No index yet — the walk is `event_loop`'s job.
    fn app_awaiting_mention_index() -> (App, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/tui")).unwrap();
        std::fs::write(dir.path().join("src/tui/app.rs"), "x").unwrap();
        std::fs::write(dir.path().join("README.md"), "x").unwrap();
        let mut app = App::new(None);
        app.set_working_dir(dir.path().to_path_buf());
        (app, dir)
    }

    /// Same fixture with the index already delivered — what the TUI looks
    /// like once the background build has landed.
    fn app_with_mention_fixture() -> (App, tempfile::TempDir) {
        let (mut app, dir) = app_awaiting_mention_index();
        app.on_mention_index_ready(crate::tui::mentions::MentionIndex::build(
            dir.path().to_path_buf(),
        ));
        (app, dir)
    }

    #[test]
    fn typing_at_fragment_opens_mention_dropdown() {
        let (mut app, _dir) = app_with_mention_fixture();
        for c in "@rea".chars() {
            app.on_event(&key(KeyCode::Char(c)));
        }
        assert!(app.mention_dropdown_visible());
        assert!(app.mention_matches.iter().any(|m| m == "README.md"));
    }

    #[test]
    fn tab_completes_selected_mention() {
        let (mut app, _dir) = app_with_mention_fixture();
        for c in "@rea".chars() {
            app.on_event(&key(KeyCode::Char(c)));
        }
        assert_eq!(app.on_event(&key(KeyCode::Tab)), Action::None);
        assert_eq!(app.input, "@README.md");
    }

    #[test]
    fn enter_confirms_mention_without_submitting() {
        let (mut app, _dir) = app_with_mention_fixture();
        for c in "@rea".chars() {
            app.on_event(&key(KeyCode::Char(c)));
        }
        let action = app.on_event(&key(KeyCode::Enter));
        assert_eq!(action, Action::None, "Enter confirms the path, no submit");
        assert_eq!(app.input, "@README.md");
    }

    #[test]
    fn esc_dismisses_dropdown_until_next_edit() {
        let (mut app, _dir) = app_with_mention_fixture();
        for c in "@rea".chars() {
            app.on_event(&key(KeyCode::Char(c)));
        }
        assert_eq!(app.on_event(&key(KeyCode::Esc)), Action::None);
        assert!(!app.mention_dropdown_visible());
        // Editing re-arms completion.
        app.on_event(&key(KeyCode::Char('d')));
        assert!(app.mention_dropdown_visible());
    }

    #[test]
    fn completing_a_directory_lists_its_children() {
        let (mut app, _dir) = app_with_mention_fixture();
        for c in "@sr".chars() {
            app.on_event(&key(KeyCode::Char(c)));
        }
        app.on_event(&key(KeyCode::Tab));
        assert_eq!(app.input, "@src/");
        assert!(
            app.mention_dropdown_visible(),
            "completing into a directory exposes its children"
        );
        assert!(app.mention_matches.iter().any(|m| m == "src/tui/"));
    }

    #[test]
    fn arrows_cycle_mention_selection() {
        let (mut app, _dir) = app_with_mention_fixture();
        for c in "@s".chars() {
            app.on_event(&key(KeyCode::Char(c)));
        }
        assert!(app.mention_matches.len() >= 2, "{:?}", app.mention_matches);
        app.on_event(&key(KeyCode::Down));
        assert_eq!(app.mention_selected, 1);
        app.on_event(&key(KeyCode::Up));
        assert_eq!(app.mention_selected, 0, "cyclique en haut");
    }

    #[test]
    fn fragment_without_index_asks_for_a_build_instead_of_walking() {
        let (mut app, dir) = app_awaiting_mention_index();
        for c in "@src".chars() {
            app.on_event(&key(KeyCode::Char(c)));
        }
        assert!(
            app.mention_matches.is_empty(),
            "rien à compléter sans index"
        );
        assert!(app.mention_indexing);
        assert!(app.mention_indexing_visible());
        assert_eq!(
            app.take_mention_index_request().as_deref(),
            Some(dir.path()),
            "event_loop doit recevoir la demande de build"
        );
        assert!(
            app.take_mention_index_request().is_none(),
            "une seule demande par build"
        );
    }

    #[test]
    fn index_delivery_fills_the_dropdown() {
        let (mut app, dir) = app_awaiting_mention_index();
        for c in "@rea".chars() {
            app.on_event(&key(KeyCode::Char(c)));
        }
        app.on_mention_index_ready(crate::tui::mentions::MentionIndex::build(
            dir.path().to_path_buf(),
        ));
        assert!(!app.mention_indexing);
        assert!(!app.mention_indexing_visible());
        assert!(app.mention_matches.iter().any(|m| m == "README.md"));
    }

    #[test]
    fn stale_index_still_answers_while_a_rebuild_is_requested() {
        let (mut app, _dir) = app_with_mention_fixture();
        app.mention_index_built_at = Instant::now().checked_sub(Duration::from_secs(600));
        for c in "@rea".chars() {
            app.on_event(&key(KeyCode::Char(c)));
        }
        assert!(
            app.mention_matches.iter().any(|m| m == "README.md"),
            "le périmé continue de servir"
        );
        assert!(app.mention_indexing);
        assert!(app.take_mention_index_request().is_some());
    }

    #[test]
    fn a_git_refresh_request_carries_the_session_working_dir() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new(None);
        app.set_working_dir(dir.path().to_path_buf());

        app.request_git_refresh();

        assert_eq!(
            app.take_git_refresh_request().as_deref(),
            Some(dir.path()),
            "event_loop doit recevoir le dossier de la session"
        );
        assert!(
            app.take_git_refresh_request().is_none(),
            "une seule demande par lecture"
        );
    }

    #[test]
    fn a_second_git_refresh_waits_for_the_first_to_answer() {
        let mut app = App::new(None);
        app.request_git_refresh();
        app.take_git_refresh_request().expect("première demande");

        app.request_git_refresh();
        assert!(
            app.take_git_refresh_request().is_none(),
            "une seule lecture en vol"
        );

        app.on_git_status(Some(GitStatus {
            branch: "main".to_string(),
            ..GitStatus::default()
        }));
        assert_eq!(
            app.git_status.as_ref().map(|g| g.branch.as_str()),
            Some("main")
        );

        app.request_git_refresh();
        assert!(
            app.take_git_refresh_request().is_some(),
            "la réponse réarme la lecture suivante"
        );
    }

    #[test]
    fn finishing_a_turn_asks_for_a_fresh_git_status() {
        let mut app = App::new(None);
        app.begin_turn();
        app.finish_turn();

        assert!(app.take_git_refresh_request().is_some());
    }

    #[test]
    fn pasting_an_existing_path_prefixes_it_as_a_mention() {
        let (mut app, dir) = app_with_mention_fixture();
        app.on_event(&Event::Paste(
            dir.path().join("README.md").to_string_lossy().into_owned(),
        ));
        assert_eq!(
            app.input,
            format!("@{}", dir.path().join("README.md").display())
        );
        assert!(!app.mention_dropdown_visible(), "chemin déjà complet");
    }

    #[test]
    fn pasting_free_text_stays_verbatim() {
        let (mut app, _dir) = app_with_mention_fixture();
        app.on_event(&Event::Paste("corrige le parseur stp".to_string()));
        assert_eq!(app.input, "corrige le parseur stp");
    }

    #[test]
    fn ctrl_p_opens_the_finder_on_the_whole_index() {
        let (mut app, _dir) = app_with_mention_fixture();
        assert_eq!(app.on_event(&ctrl_key(KeyCode::Char('p'))), Action::None);
        let finder = app.finder.as_ref().expect("finder ouvert");
        assert!(finder.query.is_empty());
        assert!(
            finder.results.iter().any(|p| p == "README.md"),
            "{:?}",
            finder.results
        );
        assert!(app.input.is_empty(), "Ctrl+P ne tape rien dans le composer");
    }

    #[test]
    fn ctrl_p_is_ignored_under_a_modal() {
        let (mut app, _dir) = app_with_mention_fixture();
        app.gate_open = true;
        assert_eq!(app.on_event(&ctrl_key(KeyCode::Char('p'))), Action::None);
        assert!(app.finder.is_none());
    }

    #[test]
    fn slash_files_opens_the_finder() {
        let (mut app, _dir) = app_with_mention_fixture();
        for c in "/files".chars() {
            app.on_event(&key(KeyCode::Char(c)));
        }
        assert_eq!(app.on_event(&key(KeyCode::Enter)), Action::None);
        assert!(app.finder.is_some());
        assert!(app.input.is_empty());
    }

    #[test]
    fn typing_in_the_finder_filters_without_touching_the_composer() {
        let (mut app, _dir) = app_with_mention_fixture();
        app.open_finder();
        for c in "app".chars() {
            app.on_event(&key(KeyCode::Char(c)));
        }
        let finder = app.finder.as_ref().expect("finder ouvert");
        assert_eq!(finder.query, "app");
        assert_eq!(
            finder.results.first().map(String::as_str),
            Some("src/tui/app.rs")
        );
        assert!(app.input.is_empty());
    }

    #[test]
    fn finder_tab_attaches_the_selected_path_and_closes() {
        let (mut app, _dir) = app_with_mention_fixture();
        app.open_finder();
        for c in "readme".chars() {
            app.on_event(&key(KeyCode::Char(c)));
        }
        assert_eq!(app.on_event(&key(KeyCode::Tab)), Action::None);
        assert!(app.finder.is_none());
        assert_eq!(app.input, "@README.md ");
    }

    #[test]
    fn finder_tab_separates_the_mention_from_what_is_already_typed() {
        let (mut app, _dir) = app_with_mention_fixture();
        for c in "relis".chars() {
            app.on_event(&key(KeyCode::Char(c)));
        }
        app.open_finder();
        for c in "readme".chars() {
            app.on_event(&key(KeyCode::Char(c)));
        }
        app.on_event(&key(KeyCode::Tab));
        assert_eq!(
            app.input, "relis @README.md ",
            "foo@bar n'est pas une mention"
        );
    }

    #[test]
    fn finder_enter_opens_the_viewer_and_hands_it_the_focus() {
        let (mut app, _dir) = app_with_mention_fixture();
        app.open_finder();
        for c in "readme".chars() {
            app.on_event(&key(KeyCode::Char(c)));
        }
        assert_eq!(app.on_event(&key(KeyCode::Enter)), Action::None);
        assert!(app.finder.is_none());
        assert_eq!(app.focus, Focus::Viewer);
        assert_eq!(
            app.viewer.as_ref().expect("lecteur ouvert").path,
            "README.md"
        );
        assert!(app.input.is_empty());
    }

    #[test]
    fn finder_esc_closes_without_touching_the_composer() {
        let (mut app, _dir) = app_with_mention_fixture();
        for c in "salut".chars() {
            app.on_event(&key(KeyCode::Char(c)));
        }
        app.open_finder();
        for c in "read".chars() {
            app.on_event(&key(KeyCode::Char(c)));
        }
        assert_eq!(app.on_event(&key(KeyCode::Esc)), Action::None);
        assert!(app.finder.is_none());
        assert_eq!(app.input, "salut");
    }

    #[test]
    fn finder_navigation_walks_the_result_list() {
        let (mut app, _dir) = app_with_mention_fixture();
        app.open_finder();
        let count = app.finder.as_ref().expect("finder ouvert").results.len();
        assert!(count >= 3, "{count} résultats");

        app.on_event(&key(KeyCode::Down));
        assert_eq!(app.finder.as_ref().unwrap().selected, 1);
        app.on_event(&ctrl_key(KeyCode::Char('n')));
        assert_eq!(app.finder.as_ref().unwrap().selected, 2);
        app.on_event(&ctrl_key(KeyCode::Char('k')));
        assert_eq!(app.finder.as_ref().unwrap().selected, 1);
        app.on_event(&key(KeyCode::Up));
        app.on_event(&key(KeyCode::Up));
        assert_eq!(
            app.finder.as_ref().unwrap().selected,
            count - 1,
            "cyclique en haut"
        );
    }

    #[test]
    fn opening_the_finder_without_an_index_asks_for_a_build() {
        let (mut app, dir) = app_awaiting_mention_index();
        app.open_finder();
        assert!(app.finder_indexing());
        assert_eq!(
            app.take_mention_index_request().as_deref(),
            Some(dir.path()),
            "event_loop doit recevoir la demande de build"
        );
    }

    #[test]
    fn a_delivered_index_fills_an_open_finder() {
        let (mut app, dir) = app_awaiting_mention_index();
        app.open_finder();
        app.on_mention_index_ready(crate::tui::mentions::MentionIndex::build(
            dir.path().to_path_buf(),
        ));
        assert!(!app.finder_indexing());
        assert!(app
            .finder
            .as_ref()
            .unwrap()
            .results
            .iter()
            .any(|p| p == "README.md"));
    }

    /// Viewer open on a 200-line file, with the geometry `draw_viewer` would
    /// have measured: 24 lignes moins deux bordures et deux marges, soit un
    /// viewport de 20 lignes, so the half-page jumps are testable.
    fn app_with_open_viewer() -> (App, tempfile::TempDir) {
        let (mut app, dir) = app_with_mention_fixture();
        std::fs::write(dir.path().join("long.txt"), "x\n".repeat(200)).unwrap();
        app.open_viewer("long.txt");
        app.viewer_area.set(Rect {
            x: 60,
            y: 1,
            width: 40,
            height: 24,
        });
        (app, dir)
    }

    #[test]
    fn viewer_keys_scroll_the_file_and_never_reach_the_composer() {
        let (mut app, _dir) = app_with_open_viewer();
        assert_eq!(app.focus, Focus::Viewer);

        for _ in 0..3 {
            app.on_event(&key(KeyCode::Char('j')));
        }
        assert_eq!(app.viewer.as_ref().unwrap().scroll, 3);
        app.on_event(&key(KeyCode::Char('k')));
        assert_eq!(app.viewer.as_ref().unwrap().scroll, 2);
        app.on_event(&ctrl_key(KeyCode::Char('d')));
        assert_eq!(
            app.viewer.as_ref().unwrap().scroll,
            12,
            "demi-page d'un viewport de 20 lignes"
        );
        app.on_event(&key(KeyCode::PageUp));
        assert_eq!(app.viewer.as_ref().unwrap().scroll, 2);
        app.on_event(&key(KeyCode::Char('G')));
        assert_eq!(app.viewer.as_ref().unwrap().scroll, 180);
        app.on_event(&key(KeyCode::Char('g')));
        assert_eq!(app.viewer.as_ref().unwrap().scroll, 0);
        assert!(app.input.is_empty(), "rien ne fuit dans le composer");
    }

    #[test]
    fn q_closes_the_viewer_and_gives_the_composer_its_keys_back() {
        let (mut app, _dir) = app_with_open_viewer();
        assert_eq!(app.on_event(&key(KeyCode::Char('q'))), Action::None);
        assert!(app.viewer.is_none());
        assert_eq!(app.focus, Focus::Composer);
        app.on_event(&key(KeyCode::Char('q')));
        assert_eq!(app.input, "q");
    }

    #[test]
    fn a_attaches_the_open_file_and_keeps_the_pane() {
        let (mut app, _dir) = app_with_open_viewer();
        app.on_event(&key(KeyCode::Char('a')));
        assert_eq!(app.input, "@long.txt ");
        assert!(app.viewer.is_some(), "le lecteur reste ouvert");
        assert_eq!(app.focus, Focus::Composer);
    }

    #[test]
    fn e_asks_to_edit_the_file_the_viewer_shows_at_the_line_it_shows() {
        let (mut app, dir) = app_with_open_viewer();
        assert_eq!(
            app.on_event(&key(KeyCode::Char('e'))),
            Action::EditFile {
                path: dir.path().join("long.txt"),
                line: Some(1),
            }
        );
        assert!(app.viewer.is_some(), "le lecteur reste ouvert");

        app.on_event(&key(KeyCode::PageDown));
        let scroll = app.viewer.as_ref().unwrap().scroll;
        assert_eq!(
            app.on_event(&key(KeyCode::Char('e'))),
            Action::EditFile {
                path: dir.path().join("long.txt"),
                line: Some(scroll + 1),
            },
            "l'éditeur ouvre là où on lisait"
        );
    }

    #[test]
    fn a_file_edited_outside_is_reloaded_with_its_scroll_clamped() {
        let (mut app, dir) = app_with_open_viewer();
        app.on_event(&key(KeyCode::Char('G')));
        assert_eq!(app.viewer.as_ref().unwrap().scroll, 180);

        std::fs::write(dir.path().join("long.txt"), "court\n").unwrap();
        app.on_file_edited(&dir.path().join("long.txt"));

        let viewer = app.viewer.as_ref().expect("le lecteur reste ouvert");
        assert_eq!(viewer.lines, vec!["court"]);
        assert_eq!(viewer.scroll, 0, "scroll ramené dans le nouveau fichier");
    }

    #[test]
    fn editing_another_file_leaves_the_open_one_alone() {
        let (mut app, dir) = app_with_open_viewer();
        std::fs::write(dir.path().join("long.txt"), "court\n").unwrap();

        app.on_file_edited(&dir.path().join("README.md"));

        assert_eq!(app.viewer.as_ref().unwrap().lines.len(), 200);
    }

    #[test]
    fn a_modal_still_owns_the_keyboard_over_the_viewer() {
        let (mut app, _dir) = app_with_open_viewer();
        app.gate_open = true;
        assert_eq!(app.on_event(&key(KeyCode::Char('y'))), Action::GateApprove);
    }

    #[test]
    fn the_wheel_over_the_viewer_scrolls_the_file_not_the_chat() {
        let (mut app, _dir) = app_with_open_viewer();
        app.chat_overflow.set(50);

        app.on_event(&Event::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 70,
            row: 5,
            modifiers: KeyModifiers::NONE,
        }));
        assert_eq!(app.viewer.as_ref().unwrap().scroll, SCROLL_WHEEL as usize);
        assert_eq!(app.scroll_offset, 0, "le chat n'a pas bougé");

        let off_the_pane = Event::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 5,
            row: 5,
            modifiers: KeyModifiers::NONE,
        });
        app.on_event(&off_the_pane);
        assert_eq!(
            app.scroll_offset, 0,
            "le chat replié derrière le lecteur n'est pas là pour défiler"
        );

        app.focus = Focus::Composer;
        app.on_event(&off_the_pane);
        assert_eq!(
            app.scroll_offset, SCROLL_WHEEL,
            "chat déplié : hors du lecteur la molette le rend"
        );
    }

    #[test]
    fn opening_a_directory_reports_it_instead_of_opening_a_pane() {
        let (mut app, _dir) = app_with_mention_fixture();
        app.open_viewer("src/tui/");
        assert!(app.viewer.is_none());
        assert_eq!(app.focus, Focus::Composer);
        assert!(app
            .chat
            .last()
            .expect("ligne système")
            .text
            .contains("lecture impossible"));
    }

    #[test]
    fn the_wheel_is_swallowed_while_the_finder_is_open() {
        let (mut app, _dir) = app_with_mention_fixture();
        app.chat_overflow.set(50);
        app.open_finder();
        app.on_event(&mouse_event(MouseEventKind::ScrollDown));
        assert_eq!(
            app.scroll_offset, 0,
            "la molette ne défile pas le chat caché sous l'overlay"
        );
    }

    #[test]
    fn pasting_several_lines_into_the_finder_keeps_them_apart() {
        let (mut app, _dir) = app_with_mention_fixture();
        app.open_finder();
        app.on_event(&Event::Paste("src\r\ntui\napp".to_string()));
        assert_eq!(
            app.finder.as_ref().expect("finder ouvert").query,
            "src tui app"
        );
    }

    /// Explorer open on the mention fixture: `src/` first (directories lead),
    /// then `README.md`.
    fn app_with_open_explorer() -> (App, tempfile::TempDir) {
        let (mut app, dir) = app_with_mention_fixture();
        app.toggle_explorer();
        (app, dir)
    }

    #[test]
    fn e_edits_the_selected_file_and_ignores_a_directory() {
        let (mut app, dir) = app_with_open_explorer();
        assert_eq!(
            app.on_event(&key(KeyCode::Char('e'))),
            Action::None,
            "src/ est un dossier"
        );
        assert!(app.chat.is_empty(), "un dossier ne dit rien");

        app.on_event(&key(KeyCode::Char('j')));
        assert_eq!(
            app.on_event(&key(KeyCode::Char('e'))),
            Action::EditFile {
                path: dir.path().join("README.md"),
                line: None,
            }
        );
    }

    #[test]
    fn slash_edit_resolves_its_path_against_the_working_dir() {
        let (mut app, dir) = app_with_mention_fixture();
        assert_eq!(
            submit(&mut app, "/edit src/tui/app.rs"),
            Action::EditFile {
                path: dir.path().join("src/tui/app.rs"),
                line: None,
            }
        );
    }

    #[test]
    fn slash_edit_takes_a_mention_form_and_a_file_that_does_not_exist_yet() {
        let (mut app, dir) = app_with_mention_fixture();
        assert_eq!(
            submit(&mut app, "/edit @nouveau.rs"),
            Action::EditFile {
                path: dir.path().join("nouveau.rs"),
                line: None,
            },
            "l'éditeur crée le fichier"
        );
    }

    #[test]
    fn slash_edit_reads_a_line_suffix_unless_the_file_really_is_named_that_way() {
        let (mut app, dir) = app_with_mention_fixture();
        assert_eq!(
            submit(&mut app, "/edit src/tui/app.rs:42"),
            Action::EditFile {
                path: dir.path().join("src/tui/app.rs"),
                line: Some(42),
            }
        );
        assert_eq!(
            submit(&mut app, "/edit src/tui/app.rs:tail"),
            Action::EditFile {
                path: dir.path().join("src/tui/app.rs:tail"),
                line: None,
            },
            "un suffixe non numérique fait partie du chemin"
        );

        std::fs::write(dir.path().join("notes:42"), "x\n").unwrap();
        assert_eq!(
            submit(&mut app, "/edit notes:42"),
            Action::EditFile {
                path: dir.path().join("notes:42"),
                line: None,
            },
            "un fichier qui porte ce nom gagne sur la ligne"
        );
    }

    #[test]
    fn slash_edit_on_a_directory_refuses_instead_of_launching_an_editor() {
        let (mut app, _dir) = app_with_mention_fixture();
        assert_eq!(submit(&mut app, "/edit src"), Action::None);
        assert!(app
            .chat
            .last()
            .expect("ligne système")
            .text
            .contains("dossier, pas un fichier"));
    }

    #[test]
    fn slash_edit_alone_shows_its_usage() {
        let mut app = App::new(None);
        assert_eq!(submit(&mut app, "/edit"), Action::None);
        assert!(app
            .chat
            .last()
            .expect("ligne système")
            .text
            .contains("usage : /edit"));
    }

    #[test]
    fn e_is_refused_while_a_turn_is_running() {
        let (mut app, _dir) = app_with_open_viewer();
        app.turn_active = true;

        assert_eq!(app.on_event(&key(KeyCode::Char('e'))), Action::None);
        assert!(app
            .chat
            .last()
            .expect("ligne système")
            .text
            .contains("un tour est en cours"));
    }

    /// Mid-turn, the blanket Enter guard queues everything as steering — a
    /// `/edit` swallowed that way would be sent to the model as a message
    /// instead of being refused.
    #[test]
    fn slash_edit_mid_turn_is_refused_rather_than_queued_as_steering() {
        let (mut app, _dir) = app_with_mention_fixture();
        app.turn_active = true;

        assert_eq!(submit(&mut app, "/edit README.md"), Action::None);
        assert_eq!(app.steer_len(), 0);
        assert!(app
            .chat
            .last()
            .expect("ligne système")
            .text
            .contains("un tour est en cours"));
    }

    fn editor_state(ids: &[&str]) -> EditorState {
        EditorState {
            detected: ids
                .iter()
                .map(|id| {
                    crate::tui::editors::EDITORS
                        .iter()
                        .find(|spec| spec.id == *id)
                        .expect("éditeur au catalogue")
                })
                .collect(),
            ..EditorState::default()
        }
    }

    fn last_line(app: &App) -> &str {
        &app.chat.last().expect("ligne système").text
    }

    #[test]
    fn slash_editor_opens_the_picker_on_the_current_resolution() {
        let mut app = App::new(None);
        app.editors = editor_state(&["nvim", "code"]);

        assert_eq!(submit(&mut app, "/editor"), Action::None);
        let picker = app.editor_picker.as_ref().expect("sélecteur ouvert");
        assert_eq!(picker.rows.len(), 2, "aucune ligne (env) sans $EDITOR");
        assert_eq!(
            picker.current,
            Some(0),
            "le repli est le premier éditeur terminal détecté"
        );
        assert_eq!(picker.selected, 0);
    }

    #[test]
    fn the_editor_picker_enter_chooses_the_command_and_says_which() {
        let mut app = App::new(None);
        app.editors = editor_state(&["nvim", "code"]);
        app.open_editor_picker();

        app.on_event(&key(KeyCode::Char('j')));
        assert_eq!(
            app.on_event(&key(KeyCode::Enter)),
            Action::Editor("code".to_string())
        );
        assert!(app.editor_picker.is_none(), "le sélecteur se ferme");
        assert_eq!(app.editors.selected.as_deref(), Some("code"));
        assert_eq!(last_line(&app), "éditeur : code");
    }

    #[test]
    fn the_editor_picker_offers_the_environment_and_choosing_it_resets() {
        let mut app = App::new(None);
        app.editors = EditorState {
            selected: Some("code".to_string()),
            visual: Some("nvim".to_string()),
            ..editor_state(&["nvim"])
        };
        app.open_editor_picker();

        let picker = app.editor_picker.as_ref().expect("sélecteur ouvert");
        assert_eq!(picker.rows.len(), 2);
        assert_eq!(picker.rows[1].detail().as_deref(), Some("$VISUAL = nvim"));

        app.on_event(&key(KeyCode::End));
        assert_eq!(app.on_event(&key(KeyCode::Enter)), Action::EditorReset);
        assert_eq!(app.editors.selected, None);
        assert_eq!(last_line(&app), "éditeur : $VISUAL = nvim");
    }

    #[test]
    fn the_editor_picker_swallows_every_key_it_does_not_use() {
        let mut app = App::new(None);
        app.editors = editor_state(&["nvim", "vim"]);
        app.open_editor_picker();

        for c in "abc".chars() {
            assert_eq!(app.on_event(&key(KeyCode::Char(c))), Action::None);
        }
        assert!(app.input.is_empty(), "rien ne fuit dans le composer");
        assert!(app.editor_picker.is_some());

        app.on_event(&key(KeyCode::Esc));
        assert!(app.editor_picker.is_none());
        assert_eq!(app.editors.selected, None, "Esc ne choisit rien");
    }

    #[test]
    fn slash_editor_takes_a_free_command_as_it_is() {
        let mut app = App::new(None);
        assert_eq!(
            submit(&mut app, "/editor kak -e"),
            Action::Editor("kak -e".to_string())
        );
        assert_eq!(app.editors.selected.as_deref(), Some("kak -e"));
        assert_eq!(last_line(&app), "éditeur : kak -e");
    }

    #[test]
    fn slash_editor_reset_hands_the_choice_back_to_the_detection() {
        let mut app = App::new(None);
        app.editors = EditorState {
            selected: Some("kak".to_string()),
            ..editor_state(&["nvim"])
        };

        assert_eq!(submit(&mut app, "/editor reset"), Action::EditorReset);
        assert_eq!(app.editors.selected, None);
        assert_eq!(last_line(&app), "éditeur : détection du PATH");
    }

    /// Le cul-de-sac que la tâche 19a existe pour supprimer : sans rien de
    /// détecté ni d'exporté, `/editor` dit quoi taper.
    #[test]
    fn slash_editor_without_a_single_editor_says_what_to_type() {
        let mut app = App::new(None);
        assert_eq!(submit(&mut app, "/editor"), Action::None);
        assert!(app.editor_picker.is_none());
        assert!(
            last_line(&app).contains("/editor <commande>"),
            "{}",
            last_line(&app)
        );
    }

    #[test]
    fn a_graphical_editor_opens_during_a_turn_with_a_warning() {
        let (mut app, dir) = app_with_open_viewer();
        app.editors = EditorState {
            selected: Some("code".to_string()),
            ..EditorState::default()
        };
        app.turn_active = true;

        assert_eq!(
            app.on_event(&key(KeyCode::Char('e'))),
            Action::EditFile {
                path: dir.path().join("long.txt"),
                line: Some(1),
            }
        );
        assert!(
            last_line(&app).contains("le tour continue"),
            "{}",
            last_line(&app)
        );
    }

    #[test]
    fn a_terminal_editor_still_waits_for_the_turn_to_end() {
        let (mut app, _dir) = app_with_open_viewer();
        app.editors = EditorState {
            selected: Some("nvim".to_string()),
            ..EditorState::default()
        };
        app.turn_active = true;

        assert_eq!(app.on_event(&key(KeyCode::Char('e'))), Action::None);
        assert!(last_line(&app).contains("un tour est en cours"));
    }

    /// Task 19b : un lancement non bloquant (nvim hôte, pane Zellij/tmux) ne
    /// prend rien au tour en cours, exactement comme le graphique de 19a.
    #[test]
    fn a_zellij_pane_edit_is_allowed_during_a_turn_with_a_warning() {
        let (mut app, dir) = app_with_open_viewer();
        app.editors = EditorState {
            selected: Some("vim".to_string()),
            ..EditorState::default()
        };
        app.launch_ctx = LaunchContext {
            zellij: true,
            cwd: dir.path().to_path_buf(),
            ..LaunchContext::default()
        };
        app.turn_active = true;

        assert_eq!(
            app.on_event(&key(KeyCode::Char('e'))),
            Action::EditFile {
                path: dir.path().join("long.txt"),
                line: Some(1),
            }
        );
        assert!(
            last_line(&app).contains("le tour continue"),
            "{}",
            last_line(&app)
        );
    }

    #[test]
    fn slash_editor_mode_switches_and_persists() {
        let mut app = App::new(None);

        assert_eq!(
            submit(&mut app, "/editor mode pane"),
            Action::EditMode("pane".to_string())
        );
        assert_eq!(app.edit_mode, EditMode::Pane);
        assert!(last_line(&app).contains("éditeur : pane"));
    }

    /// Comme `list`/`reset`, `mode` est insensible à la casse.
    #[test]
    fn slash_editor_mode_is_case_insensitive() {
        let mut app = App::new(None);

        assert_eq!(
            submit(&mut app, "/editor Mode PANE"),
            Action::EditMode("pane".to_string())
        );
        assert_eq!(app.edit_mode, EditMode::Pane);
    }

    #[test]
    fn slash_editor_mode_alone_shows_the_configured_and_effective_mode() {
        let mut app = App::new(None);
        app.editors = editor_state(&["vim"]);

        assert_eq!(submit(&mut app, "/editor mode"), Action::None);
        assert!(
            last_line(&app).contains("éditeur : auto"),
            "{}",
            last_line(&app)
        );
        assert!(
            last_line(&app).contains("effectif : suspend"),
            "{}",
            last_line(&app)
        );
    }

    #[test]
    fn slash_editor_mode_rejects_an_unknown_value() {
        let mut app = App::new(None);

        assert_eq!(submit(&mut app, "/editor mode bogus"), Action::None);
        assert!(
            last_line(&app).contains("mode inconnu"),
            "{}",
            last_line(&app)
        );
        assert_eq!(app.edit_mode, EditMode::Auto);
    }

    #[test]
    fn r_reloads_the_viewer_from_disk_with_its_scroll_clamped() {
        let (mut app, dir) = app_with_open_viewer();
        app.on_event(&key(KeyCode::Char('G')));
        assert_eq!(app.viewer.as_ref().unwrap().scroll, 180);

        std::fs::write(dir.path().join("long.txt"), "court\n").unwrap();
        assert_eq!(app.on_event(&key(KeyCode::Char('r'))), Action::None);

        let viewer = app.viewer.as_ref().expect("le lecteur reste ouvert");
        assert_eq!(viewer.lines, vec!["court"]);
        assert_eq!(viewer.scroll, 0, "scroll ramené dans le nouveau fichier");
        assert!(last_line(&app).contains("rechargé"), "{}", last_line(&app));
    }

    /// Un lancement non bloquant a pu toucher plus que le seul fichier
    /// affiché : `r` rafraîchit l'explorateur ouvert et arme un
    /// rafraîchissement du statut git, comme `on_file_edited`.
    #[test]
    fn r_also_refreshes_the_open_explorer_and_arms_a_git_refresh() {
        let (mut app, dir) = app_with_open_viewer();
        app.explorer = Some(crate::tui::explorer::ExplorerState::new(
            dir.path().to_path_buf(),
        ));
        assert!(!app
            .explorer
            .as_ref()
            .unwrap()
            .nodes
            .iter()
            .any(|n| n.name == "new.txt"));

        std::fs::write(dir.path().join("new.txt"), "x").unwrap();
        app.on_event(&key(KeyCode::Char('r')));

        assert!(
            app.explorer
                .as_ref()
                .unwrap()
                .nodes
                .iter()
                .any(|n| n.name == "new.txt"),
            "l'explorateur doit refléter le nouveau fichier après r"
        );
        assert!(
            app.take_git_refresh_request().is_some(),
            "r doit aussi armer un rafraîchissement du statut git"
        );
    }

    #[test]
    fn ctrl_e_opens_the_explorer_and_hands_it_the_focus() {
        let (mut app, _dir) = app_with_mention_fixture();
        assert_eq!(app.on_event(&ctrl_key(KeyCode::Char('e'))), Action::None);
        let explorer = app.explorer.as_ref().expect("explorateur ouvert");
        assert_eq!(explorer.selected().expect("racine listée").name, "src");
        assert_eq!(app.focus, Focus::Explorer);
        assert!(app.input.is_empty(), "Ctrl+E ne tape rien dans le composer");
    }

    #[test]
    fn ctrl_e_takes_the_focus_back_before_it_closes() {
        let (mut app, _dir) = app_with_open_explorer();
        app.focus = Focus::Composer;

        app.on_event(&ctrl_key(KeyCode::Char('e')));
        assert_eq!(
            app.focus,
            Focus::Explorer,
            "ouvert ailleurs : reprend le focus"
        );
        assert!(app.explorer.is_some());

        app.on_event(&ctrl_key(KeyCode::Char('e')));
        assert!(app.explorer.is_none(), "ouvert et focus dessus : ferme");
        assert_eq!(app.focus, Focus::Composer);
    }

    #[test]
    fn ctrl_e_is_ignored_under_a_modal() {
        let (mut app, _dir) = app_with_mention_fixture();
        app.gate_open = true;
        assert_eq!(app.on_event(&ctrl_key(KeyCode::Char('e'))), Action::None);
        assert!(app.explorer.is_none());
    }

    #[test]
    fn slash_explorer_opens_the_explorer() {
        let (mut app, _dir) = app_with_mention_fixture();
        for c in "/explorer".chars() {
            app.on_event(&key(KeyCode::Char(c)));
        }
        assert_eq!(app.on_event(&key(KeyCode::Enter)), Action::None);
        assert!(app.explorer.is_some());
        assert_eq!(app.focus, Focus::Explorer);
        assert!(app.input.is_empty());
    }

    #[test]
    fn explorer_keys_walk_the_tree_and_never_reach_the_composer() {
        let (mut app, _dir) = app_with_open_explorer();
        app.on_event(&key(KeyCode::Char('l')));
        assert_eq!(
            app.explorer
                .as_ref()
                .unwrap()
                .nodes
                .iter()
                .map(|node| node.path.as_str())
                .collect::<Vec<_>>(),
            vec!["src", "src/tui", "README.md"],
            "l déplie src/"
        );

        app.on_event(&key(KeyCode::Char('j')));
        assert_eq!(
            app.explorer.as_ref().unwrap().selected().unwrap().path,
            "src/tui"
        );
        app.on_event(&key(KeyCode::Char('h')));
        assert_eq!(
            app.explorer.as_ref().unwrap().selected().unwrap().path,
            "src",
            "h remonte au parent"
        );
        app.on_event(&key(KeyCode::Char('h')));
        assert!(
            !app.explorer.as_ref().unwrap().nodes[0].expanded,
            "h replie"
        );

        app.on_event(&key(KeyCode::Char('G')));
        assert_eq!(
            app.explorer.as_ref().unwrap().selected().unwrap().path,
            "README.md"
        );
        app.on_event(&key(KeyCode::Char('g')));
        assert_eq!(
            app.explorer.as_ref().unwrap().selected().unwrap().path,
            "src"
        );
        app.on_event(&key(KeyCode::Char('R')));
        app.on_event(&key(KeyCode::Char('.')));
        assert!(app.explorer.as_ref().unwrap().show_hidden);

        assert!(app.input.is_empty(), "rien ne fuit dans le composer");
    }

    #[test]
    fn explorer_enter_on_a_file_opens_the_viewer() {
        let (mut app, _dir) = app_with_open_explorer();
        app.on_event(&key(KeyCode::Char('j')));
        assert_eq!(app.on_event(&key(KeyCode::Enter)), Action::None);
        assert_eq!(
            app.viewer.as_ref().expect("lecteur ouvert").path,
            "README.md"
        );
        assert_eq!(app.focus, Focus::Viewer);
        assert!(app.explorer.is_some(), "l'arbre reste ouvert à gauche");
        assert!(app.input.is_empty());
    }

    #[test]
    fn explorer_a_attaches_the_selected_path_to_the_composer() {
        let (mut app, _dir) = app_with_open_explorer();
        app.on_event(&key(KeyCode::Char('a')));
        assert_eq!(app.input, "@src/ ", "un dossier garde son slash");
        assert!(app.explorer.is_some(), "le volet reste ouvert");
        assert_eq!(app.focus, Focus::Composer);

        app.focus = Focus::Explorer;
        app.on_event(&key(KeyCode::Char('j')));
        app.on_event(&key(KeyCode::Char('a')));
        assert_eq!(app.input, "@src/ @README.md ");
    }

    #[test]
    fn explorer_esc_closes_and_gives_the_composer_its_keys_back() {
        let (mut app, _dir) = app_with_open_explorer();
        assert_eq!(app.on_event(&key(KeyCode::Esc)), Action::None);
        assert!(app.explorer.is_none());
        assert_eq!(app.focus, Focus::Composer);
        app.on_event(&key(KeyCode::Char('j')));
        assert_eq!(app.input, "j");
    }

    #[test]
    fn explorer_slash_filters_the_visible_names() {
        let (mut app, _dir) = app_with_open_explorer();
        app.on_event(&key(KeyCode::Char('/')));
        for c in "read".chars() {
            app.on_event(&key(KeyCode::Char(c)));
        }
        let explorer = app.explorer.as_ref().unwrap();
        assert_eq!(explorer.filter, "read");
        assert_eq!(explorer.visible().len(), 1);
        assert_eq!(explorer.selected().unwrap().path, "README.md");
        assert!(app.input.is_empty());

        app.on_event(&key(KeyCode::Esc));
        assert!(app.explorer.as_ref().unwrap().filter.is_empty());
        assert!(app.explorer.is_some(), "Esc vide le filtre avant de fermer");
    }

    #[test]
    fn a_modal_still_owns_the_keyboard_over_the_explorer() {
        let (mut app, _dir) = app_with_open_explorer();
        app.gate_open = true;
        assert_eq!(app.on_event(&key(KeyCode::Char('y'))), Action::GateApprove);
    }

    #[test]
    fn ctrl_o_cycles_the_focus_over_the_open_panes_only() {
        let (mut app, _dir) = app_with_open_explorer();
        app.focus = Focus::Composer;

        app.on_event(&ctrl_key(KeyCode::Char('o')));
        assert_eq!(app.focus, Focus::Explorer);
        app.on_event(&ctrl_key(KeyCode::Char('o')));
        assert_eq!(
            app.focus,
            Focus::Composer,
            "le lecteur est fermé, on l'enjambe"
        );

        app.open_viewer("README.md");
        assert_eq!(app.focus, Focus::Viewer);
        app.on_event(&ctrl_key(KeyCode::Char('o')));
        assert_eq!(app.focus, Focus::Composer);
        app.on_event(&ctrl_key(KeyCode::Char('o')));
        assert_eq!(app.focus, Focus::Explorer);
        app.on_event(&ctrl_key(KeyCode::Char('o')));
        assert_eq!(app.focus, Focus::Viewer);
    }

    #[test]
    fn ctrl_e_reaches_the_explorer_from_inside_the_viewer() {
        let (mut app, _dir) = app_with_open_viewer();
        assert_eq!(app.focus, Focus::Viewer);
        app.on_event(&ctrl_key(KeyCode::Char('e')));
        assert!(app.explorer.is_some());
        assert_eq!(app.focus, Focus::Explorer);
        assert!(app.viewer.is_some(), "le lecteur reste ouvert à droite");
    }

    #[test]
    fn pasting_an_already_prefixed_path_is_not_prefixed_twice() {
        let (mut app, dir) = app_with_mention_fixture();
        // A file whose name genuinely starts with `@`: it resolves, so only
        // the prefix guard keeps the paste from coming back as `@@ref.md`.
        std::fs::write(dir.path().join("@ref.md"), "x").unwrap();
        app.on_event(&Event::Paste("@ref.md".to_string()));
        assert_eq!(app.input, "@ref.md");

        app.input.clear();
        let mention = format!("@{}", dir.path().join("README.md").display());
        app.on_event(&Event::Paste(mention.clone()));
        assert_eq!(app.input, mention);
    }

    #[test]
    fn pasting_a_missing_path_stays_verbatim() {
        let (mut app, dir) = app_with_mention_fixture();
        let missing = dir.path().join("absent.md").to_string_lossy().into_owned();
        app.on_event(&Event::Paste(missing.clone()));
        assert_eq!(app.input, missing);
    }

    #[test]
    fn pasting_multiple_lines_collapses_to_one() {
        let (mut app, _dir) = app_with_mention_fixture();
        app.on_event(&Event::Paste(
            "ligne un\r\nligne deux\nligne trois".to_string(),
        ));
        assert_eq!(app.input, "ligne un ligne deux ligne trois");
    }

    #[test]
    fn enter_mid_turn_with_dropdown_completes_instead_of_queueing() {
        let (mut app, _dir) = app_with_mention_fixture();
        app.turn_active = true;
        for c in "@rea".chars() {
            app.on_event(&key(KeyCode::Char(c)));
        }
        let action = app.on_event(&key(KeyCode::Enter));
        assert_eq!(action, Action::None);
        assert_eq!(app.input, "@README.md");
        assert_eq!(app.steer_len(), 0, "completion is not a steer message");
    }

    #[test]
    fn esc_mid_turn_with_dropdown_dismisses_it_without_canceling_the_turn() {
        let (mut app, _dir) = app_with_mention_fixture();
        app.turn_active = true;
        for c in "@rea".chars() {
            app.on_event(&key(KeyCode::Char(c)));
        }
        let action = app.on_event(&key(KeyCode::Esc));
        assert_eq!(action, Action::None, "Esc ferme la liste, pas le tour");
        assert!(!app.mention_dropdown_visible());
        assert_eq!(app.input, "@rea", "le fragment reste");
        // Un second Esc, la liste fermée, annule bien le tour.
        assert_eq!(app.on_event(&key(KeyCode::Esc)), Action::CancelTurn);
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

    /// FIX 1: `palette_matches` used to filter on `self.input` raw — a
    /// trailing/leading space broke the `starts_with` prefix match and
    /// closed the palette even though the Enter dispatch below trims before
    /// comparing, so the visible state and the actual behavior disagreed.
    #[test]
    fn palette_stays_visible_with_surrounding_whitespace() {
        let mut app = App::new(None);
        for c in "/quit".chars() {
            app.on_event(&key(KeyCode::Char(c)));
        }
        app.on_event(&key(KeyCode::Char(' ')));
        assert!(
            app.palette_visible(),
            "trailing space must not close the palette"
        );
        let names: Vec<_> = app.palette_matches().iter().map(|c| c.name).collect();
        assert_eq!(names, vec!["/quit"]);

        let mut app2 = App::new(None);
        app2.on_event(&key(KeyCode::Char(' ')));
        for c in "/sdd".chars() {
            app2.on_event(&key(KeyCode::Char(c)));
        }
        assert!(
            app2.palette_visible(),
            "leading space must not close the palette"
        );
        let names2: Vec<_> = app2.palette_matches().iter().map(|c| c.name).collect();
        assert_eq!(names2, vec!["/sdd"]);
    }

    /// FIX 1: the UI must not lie — if the palette is what the user sees
    /// (surrounding whitespace kept it open), Enter must go through the
    /// palette-selection branch, not the legacy trim-then-exact-match
    /// fallback. Same end action either way for `/quit`, but the assertion
    /// on `palette_visible()` right before Enter proves which path ran.
    #[test]
    fn palette_enter_with_trailing_space_executes_the_visible_selection() {
        let mut app = App::new(None);
        for c in "/quit ".chars() {
            app.on_event(&key(KeyCode::Char(c)));
        }
        assert!(
            app.palette_visible(),
            "palette must still be visible at Enter time"
        );
        let action = app.on_event(&key(KeyCode::Enter));
        assert_eq!(action, Action::Quit);
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

    fn agent_message(app: &mut App, message: Message) {
        app.apply_agent_event(&kaji::agents::AgentEvent::Message(message));
    }

    #[test]
    fn a_provider_error_is_rendered_with_its_taxonomy_label() {
        let mut app = App::new(None);
        let error = kaji_providers::errors::ProviderError::RateLimitExceeded {
            details: "slow down".to_string(),
            retry_delay: None,
        };

        agent_message(&mut app, Message::from_provider_error(&error));

        let line = app.chat.last().expect("error line");
        assert_eq!(line.sender, Sender::System);
        assert!(line.text.contains("limite de débit"), "{}", line.text);
        assert!(line.text.starts_with('✗'), "{}", line.text);
        assert!(line.rendered.is_some(), "error lines carry their own style");
    }

    #[test]
    fn an_inline_notification_is_rendered_in_the_system_register() {
        let mut app = App::new(None);

        agent_message(
            &mut app,
            Message::assistant().with_system_notification(
                SystemNotificationType::InlineMessage,
                "Context limit reached",
            ),
        );

        let line = app.chat.last().expect("system line");
        assert_eq!(line.sender, Sender::System);
        assert_eq!(line.text, "Context limit reached");
        assert!(line.rendered.is_none(), "plain dim system register");
    }

    #[test]
    fn a_thinking_notification_stays_out_of_the_transcript() {
        let mut app = App::new(None);

        agent_message(
            &mut app,
            Message::assistant()
                .with_system_notification(SystemNotificationType::ThinkingMessage, "réflexion"),
        );

        assert!(app.chat.is_empty());
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

    fn agent_errors(app: &mut App, id: &str, message: &str) {
        let mut m = Message::assistant().with_error(MessageErrorKind::Other, message);
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
    fn drift_verdict_with_valide_mentioned_in_reasoning_does_not_lock() {
        let mut app = App::new(Some(spec()));
        app.start_pass();
        app.gate_approve();
        app.turn_active = true;
        agent_says(&mut app, "m1", "c'est fait");
        app.turn_end();

        app.turn_active = true;
        agent_says(
            &mut app,
            "m2",
            "Exigence 1 : ok.\n\
             Exigence 2 : on ne peut pas conclure VERDICT: VALIDE ici, il manque X.\n\
             VERDICT: DRIFT",
        );
        assert!(app.turn_end().is_none());

        assert!(app.pass.drifted());
        assert!(!app.pass.is_complete());
    }

    #[test]
    fn valide_only_on_last_line_locks_the_spec() {
        let mut app = App::new(Some(spec()));
        app.start_pass();
        app.gate_approve();
        app.turn_active = true;
        agent_says(&mut app, "m1", "c'est fait");
        app.turn_end();

        app.turn_active = true;
        agent_says(
            &mut app,
            "m2",
            "Exigence 1 : satisfaite.\n\
             Exigence 2 : satisfaite.\n\
             VERDICT: VALIDE",
        );
        assert!(app.turn_end().is_none());

        assert!(app.pass.is_complete());
        assert!(!app.pass.drifted());
    }

    #[test]
    fn trailing_punctuation_on_verdict_line_still_parses() {
        let mut app = App::new(Some(spec()));
        app.start_pass();
        app.gate_approve();
        app.turn_active = true;
        agent_says(&mut app, "m1", "fait autre chose");
        app.turn_end();

        app.turn_active = true;
        agent_says(
            &mut app,
            "m2",
            "Exigence 1 : non satisfaite.\nVERDICT: DRIFT.",
        );
        app.turn_end();

        assert!(app.pass.drifted());
        assert!(!app.pass.is_complete());
        assert!(app.chat.iter().any(|l| l.text.contains("drift détecté")));
        assert!(!app.chat.iter().any(|l| l.text.contains("verdict absent")));
    }

    #[test]
    fn validate_prompt_demands_refutation_before_verdict() {
        let mut app = App::new(Some(spec()));
        app.start_pass();
        app.gate_approve();
        app.turn_active = true;
        agent_says(&mut app, "m1", "c'est fait");
        let validate_prompt = app.turn_end().expect("prompt validate");

        assert!(validate_prompt.contains("défaut"));
        assert!(validate_prompt.contains("DRIFT"));
        assert!(validate_prompt.contains("exigence par exigence"));
        assert!(validate_prompt.contains("CHAQUE exigence"));
        assert!(validate_prompt.contains("avant la ligne finale"));
        assert!(validate_prompt.contains("VERDICT: VALIDE"));
        assert!(validate_prompt.contains("VERDICT: DRIFT"));
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
        app.push_system(&format!("{} outil", theme::TOOL_GLYPH));
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

        let running = format!("{} shell", theme::TOOL_GLYPH);
        assert!(app.chat.iter().any(|l| l.text.contains(&running)));
        assert!(app.chat.iter().any(|l| l.tool.is_some()));

        let resp_msg =
            Message::user().with_tool_response("t1", Ok(CallToolResult::success(vec![])));
        app.apply_agent_event(&AgentEvent::Message(resp_msg));

        assert!(!app.chat.iter().any(|l| l.text.contains(&running)));
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
        assert_eq!(
            app.on_event(&key(KeyCode::Enter)),
            Action::Cost(report::CostView::Windows)
        );
        assert!(app.input.is_empty());
    }

    #[test]
    fn slash_cost_with_a_view_carries_it_in_the_action() {
        for (typed, expected) in [
            ("/cost modèles", report::CostView::Models),
            ("/cost mois", report::CostView::Month),
            ("/cost cache", report::CostView::Cache),
            ("/cost projection", report::CostView::Projection),
        ] {
            let mut app = App::new(None);
            for c in typed.chars() {
                app.on_event(&key(KeyCode::Char(c)));
            }
            assert_eq!(
                app.on_event(&key(KeyCode::Enter)),
                Action::Cost(expected),
                "{typed}"
            );
        }
    }

    #[test]
    fn slash_cost_with_an_unknown_view_shows_the_usage_line() {
        let mut app = App::new(None);
        for c in "/cost bidule".chars() {
            app.on_event(&key(KeyCode::Char(c)));
        }
        assert_eq!(app.on_event(&key(KeyCode::Enter)), Action::None);
        assert!(
            app.chat
                .last()
                .is_some_and(|line| line.text.contains("usage : /cost")),
            "dernière ligne : {:?}",
            app.chat.last().map(|l| &l.text)
        );
    }

    /// `/costume` ne doit pas être lu comme un `/cost` dont la vue serait
    /// `ume` — même garde de mot entier que les autres commandes à argument.
    #[test]
    fn cost_command_arg_keeps_cost_a_whole_word() {
        assert_eq!(cost_command_arg("/cost"), Some(""));
        assert_eq!(cost_command_arg("/cost mois"), Some("mois"));
        assert_eq!(cost_command_arg("/costume"), None);
    }

    #[test]
    fn slash_context_returns_context_action() {
        let mut app = App::new(None);
        for c in "/context".chars() {
            app.on_event(&key(KeyCode::Char(c)));
        }
        assert_eq!(app.on_event(&key(KeyCode::Enter)), Action::Context);
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
            vec![RoledSpan::title("/cost")],
            vec![RoledSpan::text("session : 10")],
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
        assert_eq!(
            app.on_event(&key(KeyCode::Char('y'))),
            Action::ToolAnswer(Permission::AllowOnce)
        );
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
        assert_eq!(
            app.on_event(&key(KeyCode::Char('n'))),
            Action::ToolAnswer(Permission::DenyOnce)
        );
    }

    fn open_approval(app: &mut App, tool_name: &str, arguments: JsonObject) {
        let msg = Message::assistant().with_action_required(
            "req-args".to_string(),
            tool_name.to_string(),
            arguments,
            None,
        );
        app.apply_agent_event(&AgentEvent::Message(msg));
        assert!(
            app.tool_approval.is_some(),
            "test setup: modal must be open"
        );
    }

    fn approval_detail(tool_name: &str, arguments: JsonObject) -> String {
        let mut app = App::new(None);
        open_approval(&mut app, tool_name, arguments);
        app.on_event(&key(KeyCode::Tab));
        app.approval_detail
            .clone()
            .expect("Tab must open the detail panel")
    }

    #[test_case(KeyCode::Char('y'), Permission::AllowOnce; "y_allows_once")]
    #[test_case(KeyCode::Char('s'), Permission::AllowSession; "s_allows_for_the_session")]
    #[test_case(KeyCode::Char('a'), Permission::AlwaysAllow; "a_allows_always")]
    #[test_case(KeyCode::Char('n'), Permission::DenyOnce; "n_denies")]
    #[test_case(KeyCode::Esc, Permission::DenyOnce; "esc_denies")]
    fn every_approval_key_carries_its_permission(code: KeyCode, expected: Permission) {
        let mut app = App::new(None);
        open_approval(&mut app, "shell", object!({ "command": "cargo test" }));
        assert_eq!(app.on_event(&key(code)), Action::ToolAnswer(expected));
    }

    /// `Ctrl+S` flushes the steer queue and `Ctrl+W` deletes a word — reading
    /// the bare letter out of a chord would turn either reflex into a
    /// session-wide grant on a modal the user hasn't answered yet.
    #[test]
    fn chorded_letters_are_not_approval_answers() {
        let mut app = App::new(None);
        open_approval(&mut app, "shell", object!({ "command": "cargo test" }));
        assert_eq!(app.on_event(&ctrl_key(KeyCode::Char('s'))), Action::None);
        assert_eq!(app.on_event(&ctrl_key(KeyCode::Char('a'))), Action::None);
        assert!(app.tool_approval.is_some(), "the modal must stay open");
    }

    /// The label must be the core's own serialized grant, not a TUI
    /// paraphrase: `s`/`a` persist exactly what this line promises.
    #[test]
    fn the_grant_label_is_the_derivation_the_core_would_persist() {
        let mut app = App::new(None);
        open_approval(
            &mut app,
            "shell",
            object!({ "command": "cargo test -p kaji" }),
        );
        assert_eq!(
            app.tool_approval.as_ref().expect("open").grant_label(),
            "cargo test *"
        );

        let mut app = App::new(None);
        open_approval(&mut app, "write", object!({ "path": "src/main.rs" }));
        assert_eq!(
            app.tool_approval.as_ref().expect("open").grant_label(),
            "src/main.rs"
        );

        let mut app = App::new(None);
        open_approval(&mut app, "other__tool", object!({ "x": 1 }));
        assert_eq!(
            app.tool_approval.as_ref().expect("open").grant_label(),
            "tout l'outil other__tool"
        );
    }

    #[test]
    fn tab_toggles_the_detail_panel_and_the_approval_owns_its_lifetime() {
        let mut app = App::new(None);
        open_approval(&mut app, "shell", object!({ "command": "rm -rf /tmp/x" }));

        assert_eq!(app.on_event(&key(KeyCode::Tab)), Action::None);
        assert_eq!(app.approval_detail.as_deref(), Some("rm -rf /tmp/x"));
        app.on_event(&key(KeyCode::Tab));
        assert!(app.approval_detail.is_none(), "Tab must toggle back off");

        app.on_event(&key(KeyCode::Tab));
        app.take_tool_approval();
        assert!(
            app.approval_detail.is_none(),
            "the panel must not outlive the approval it describes"
        );
    }

    #[test]
    fn the_detail_panel_diffs_an_edit_line_by_line() {
        let detail = approval_detail(
            "developer__edit",
            object!({ "path": "a.rs", "before": "let a = 1;\nkeep", "after": "let a = 2;\nkeep" }),
        );
        assert_eq!(detail, "-let a = 1;\n+let a = 2;\n keep");
    }

    #[test]
    fn the_detail_panel_diffs_a_write_against_the_file_it_replaces() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("notes.txt");
        std::fs::write(&path, "old\nkeep\n").expect("seed file");
        let detail = approval_detail(
            "write",
            object!({ "path": path.to_str().expect("utf-8 path"), "content": "new\nkeep\n" }),
        );
        assert_eq!(detail, "-old\n+new\n keep");
    }

    #[test]
    fn the_detail_panel_announces_a_write_to_a_path_that_does_not_exist() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("fresh.txt");
        let detail = approval_detail(
            "write",
            object!({ "path": path.to_str().expect("utf-8 path"), "content": "hello" }),
        );
        assert!(
            detail.starts_with("nouveau fichier "),
            "a creation must not be dressed up as a diff, got: {detail}"
        );
        assert!(detail.ends_with("hello"), "got: {detail}");
    }

    /// The tool writes relative to the session's `working_dir` (`Agent::reply`),
    /// which a resumed session no longer necessarily shares with the process
    /// cwd — resolving the preview against the cwd would announce a creation
    /// for a file the write is about to overwrite.
    #[test]
    fn the_detail_panel_diffs_a_relative_write_against_the_session_working_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("notes.md"), "old\n").expect("seed file");
        let mut app = App::new(None);
        app.set_working_dir(dir.path().to_path_buf());
        open_approval(
            &mut app,
            "write",
            object!({ "path": "notes.md", "content": "new\n" }),
        );

        app.on_event(&key(KeyCode::Tab));

        assert_eq!(app.approval_detail.as_deref(), Some("-old\n+new"));
    }

    /// The panel is a preview: a huge file must be read up to the cap and no
    /// further, and the text handed to the renderer stays bounded whatever the
    /// file's size.
    #[test]
    fn the_detail_panel_reads_a_bounded_slice_of_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("big.txt");
        let line = "x".repeat(63);
        let contents = std::iter::repeat_n(line.as_str(), 8_000)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(contents.len() > DETAIL_READ_LIMIT as usize, "test setup");
        std::fs::write(&path, &contents).expect("seed file");

        assert_eq!(
            read_bounded(&path).expect("readable").len(),
            DETAIL_READ_LIMIT as usize
        );
        let detail = approval_detail(
            "write",
            object!({ "path": path.to_str().expect("utf-8 path"), "content": "small" }),
        );
        assert!(
            detail.chars().count() <= DETAIL_MAX_CHARS + 32,
            "got {} chars",
            detail.chars().count()
        );
        assert!(
            detail.contains("car.)"),
            "the cut must be marked, got: {detail}"
        );
    }

    #[test]
    fn the_detail_panel_falls_back_to_the_raw_arguments() {
        let detail = approval_detail("other__tool", object!({ "depth": 3 }));
        assert!(detail.contains("\"depth\": 3"), "got: {detail}");
    }

    #[test_case(KajiMode::Approve, KajiMode::SmartApprove; "approve_ramps_to_smart")]
    #[test_case(KajiMode::SmartApprove, KajiMode::Auto; "smart_ramps_to_auto")]
    #[test_case(KajiMode::Auto, KajiMode::Approve; "auto_wraps_back_to_approve")]
    #[test_case(KajiMode::Chat, KajiMode::Approve; "chat_is_outside_the_cycle")]
    fn shift_tab_cycles_the_kaji_mode(from: KajiMode, expected: KajiMode) {
        let mut app = App::new(None);
        app.kaji_mode = from;
        assert_eq!(app.on_event(&key(KeyCode::BackTab)), Action::Mode(expected));
        assert_eq!(app.kaji_mode, expected, "the badge must switch immediately");
        assert!(app
            .chat
            .last()
            .expect("system line pushed")
            .text
            .contains(&mode_line(expected)));
    }

    /// Le kanji seul ne se traduit pas : le changement de mode déplie le mot à
    /// côté du sceau, le temps de le lire.
    #[test]
    fn shift_tab_unfolds_the_seal() {
        let mut app = App::new(None);

        assert!(!app.seal_unfolded(), "replié au repos");
        app.on_event(&key(KeyCode::BackTab));

        assert!(app.seal_unfolded());
    }

    #[test]
    fn the_unfolded_seal_folds_back_once_its_delay_has_run_out() {
        let mut app = App::new(None);
        app.unfold_seal();

        app.seal_unfolded_until = Some(Instant::now() - Duration::from_secs(1));

        assert!(!app.seal_unfolded());
    }

    #[test_case(KajiMode::Approve, "approve", "承", "kaji demande avant chaque outil"; "approve")]
    #[test_case(KajiMode::SmartApprove, "smart", "智", "kaji demande pour les outils risqués"; "smart")]
    #[test_case(KajiMode::Auto, "auto", "自", "kaji agit sans demander"; "auto")]
    #[test_case(KajiMode::Chat, "chat", "話", "aucun outil"; "chat")]
    fn mode_line_says_the_word_the_kanji_and_what_the_mode_allows(
        mode: KajiMode,
        word: &str,
        kanji: &str,
        promise: &str,
    ) {
        assert_eq!(
            mode_line(mode),
            format!("mode : {word} {kanji} — {promise}")
        );
    }

    /// Le feu de la barre d'état dit l'outil en cours — le dernier demandé qui
    /// attend encore sa réponse, pas le premier de la file.
    #[test]
    fn current_tool_names_the_last_request_still_waiting() {
        let mut app = App::new(None);
        assert_eq!(app.current_tool(), None);

        app.apply_agent_event(&AgentEvent::Message(
            Message::assistant().with_tool_request("t1", Ok(CallToolRequestParams::new("shell"))),
        ));
        app.apply_agent_event(&AgentEvent::Message(
            Message::assistant().with_tool_request("t2", Ok(CallToolRequestParams::new("write"))),
        ));

        assert_eq!(app.current_tool(), Some("write"));

        app.apply_agent_event(&AgentEvent::Message(
            Message::user().with_tool_response("t2", Ok(CallToolResult::success(vec![]))),
        ));

        assert_eq!(app.current_tool(), Some("shell"));
    }

    /// Shift+Tab is the reflex for "previous item" while a list is open, and
    /// the mention dropdown owns its sibling Tab — ramping the permission
    /// mode behind the overlay is a mode switch the user never asked for.
    #[test]
    fn shift_tab_is_inert_while_the_mention_dropdown_is_open() {
        let (mut app, _dir) = app_with_mention_fixture();
        for c in "@rea".chars() {
            app.on_event(&key(KeyCode::Char(c)));
        }
        assert!(app.mention_dropdown_visible(), "test setup");

        assert_eq!(app.on_event(&key(KeyCode::BackTab)), Action::None);
        assert_eq!(app.kaji_mode, KajiMode::default());
    }

    /// Tab drives the palette selection while it is open — its sibling chord
    /// must not switch modes under a list the user is navigating.
    #[test]
    fn shift_tab_is_inert_while_the_palette_is_open() {
        let mut app = App::new(None);
        for c in "/th".chars() {
            app.on_event(&key(KeyCode::Char(c)));
        }
        assert!(app.palette_visible(), "test setup");
        assert_eq!(app.on_event(&key(KeyCode::BackTab)), Action::None);
        assert_eq!(app.kaji_mode, KajiMode::default());
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
        let _theme = theme::test_guard();
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

    #[test]
    fn tab_accepts_the_next_prompt_suggestion_into_the_empty_input() {
        let mut app = App::new(None);
        app.suggestion = Some("refactoriser le module shell en deux".to_string());
        let action = app.on_event(&key(KeyCode::Tab));
        assert_eq!(action, Action::None);
        assert_eq!(app.input, "refactoriser le module shell en deux");
        assert!(app.suggestion.is_none(), "le ghost est consommé par Tab");
    }

    #[test]
    fn suggestion_is_not_accepted_when_the_input_is_not_empty() {
        let mut app = App::new(None);
        app.input = "draft en cours".to_string();
        app.suggestion = Some("suggestion".to_string());
        app.on_event(&key(KeyCode::Tab));
        assert_eq!(
            app.input, "draft en cours",
            "input non vide → Tab n'écrase pas le draft"
        );
        assert_eq!(
            app.suggestion,
            Some("suggestion".to_string()),
            "et le ghost reste disponible"
        );
    }

    #[test]
    fn tab_without_a_suggestion_is_a_noop() {
        let mut app = App::new(None);
        assert_eq!(app.on_event(&key(KeyCode::Tab)), Action::None);
        assert!(app.input.is_empty());
    }

    #[test]
    fn editing_the_input_clears_a_pending_suggestion() {
        let mut app = App::new(None);
        app.suggestion = Some("suggestion".to_string());
        app.on_event(&key(KeyCode::Char('a')));
        assert!(app.suggestion.is_none(), "typer invalide le ghost");
        app.on_event(&key(KeyCode::Tab));
        assert_eq!(app.input, "a");
    }

    #[test]
    fn starting_a_turn_clears_a_pending_suggestion() {
        let mut app = App::new(None);
        app.suggestion = Some("suggestion".to_string());
        app.reset_turn_visibility();
        assert!(app.suggestion.is_none() && !app.suggestion_loading);
    }

    #[test]
    fn accept_suggestion_when_loading_but_none_stays_quiet() {
        let mut app = App::new(None);
        app.suggestion_loading = true;
        app.accept_suggestion();
        assert!(!app.suggestion_loading && app.input.is_empty());
    }

    fn goal_events(app: &mut App) -> Vec<(&'static str, String)> {
        app.take_goal_events()
    }

    /// ⛔ BARRIÈRE — `/goal` a un homonyme côté core (self-nudge, sans
    /// évaluateur) : la TUI doit l'intercepter, jamais l'envoyer à l'agent
    /// comme message (`Action::Submit`).
    #[test]
    fn slash_goal_never_reaches_the_agent_as_a_message() {
        let mut app = App::new(None);

        let action = submit(&mut app, "/goal les tests passent");

        assert_eq!(action, Action::GoalSet("les tests passent".to_string()));
    }

    #[test]
    fn goal_command_arg_keeps_goal_a_whole_word() {
        assert_eq!(goal_command_arg("/goal"), Some(""));
        assert_eq!(goal_command_arg("/goal fais X"), Some("fais X"));
        assert_eq!(goal_command_arg("/goalx"), None);
        assert_eq!(goal_command_arg("/goals fais X"), None);
        assert_eq!(goal_command_arg("/restore a1"), None);
    }

    #[test]
    fn bare_slash_goal_asks_for_the_status() {
        let mut app = App::new(None);

        assert_eq!(submit(&mut app, "/goal"), Action::GoalStatus);

        app.push_goal_status();
        assert!(app.chat.iter().any(|l| l.text.contains("aucun but")));
    }

    #[test]
    fn goal_set_starts_the_first_work_turn() {
        let mut app = App::new(None);

        let prompt = app
            .goal_set("les tests passent", 10)
            .expect("prompt de travail");

        assert!(prompt.contains("Objectif : les tests passent"));
        assert_eq!(app.goal_driver(), GoalDriver::Working);
        let goal = app.goal.as_ref().expect("un but");
        assert_eq!(goal.iteration, 1);
        assert_eq!(goal.max_iterations, 10);
        let events = goal_events(&mut app);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, "goal_start");
        assert!(events[0].1.contains("les tests passent"));
    }

    fn has_no_control_char(line: &ChatLine) -> bool {
        !line.text.contains('\x1b') && !line.text.contains('\r')
    }

    #[test]
    fn goal_set_sanitizes_a_hostile_condition_in_the_chat_line() {
        let mut app = App::new(None);

        app.goal_set("safe\x1b[31m\rcondition", 10);

        let last = app.chat.last().expect("ligne système poussée");
        assert!(has_no_control_char(last), "{:?}", last.text);
    }

    #[test]
    fn goal_status_sanitizes_a_hostile_condition_in_the_chat_line() {
        let mut app = App::new(None);
        app.goal_set("safe\x1b[31m\rcondition", 10);

        app.push_goal_status();

        let last = app.chat.last().expect("ligne de statut");
        assert!(has_no_control_char(last), "{:?}", last.text);
    }

    #[test]
    fn an_unreachable_verdict_sanitizes_the_evaluator_feedback_in_the_chat_line() {
        let mut app = App::new(None);
        app.goal_set("les tests passent", 10);
        app.turn_active = true;
        agent_says(&mut app, "m1", "j'ai essayé");
        app.turn_end();

        app.turn_active = true;
        agent_says(
            &mut app,
            "m2",
            "hors\x1b[31m\rpérimètre\nVERDICT: UNREACHABLE",
        );
        app.turn_end();

        let last = app.chat.last().expect("ligne de fin de but");
        assert!(has_no_control_char(last), "{:?}", last.text);
    }

    /// La sanitisation vit au point d'interpolation : les prompts envoyés à
    /// l'agent et à l'évaluateur gardent la condition et le retour bruts.
    #[test]
    fn goal_prompts_keep_the_raw_condition_and_feedback() {
        let mut app = App::new(None);

        let work = app
            .goal_set("safe\x1b[31m\rcondition", 10)
            .expect("prompt de travail");
        assert!(work.contains("safe\x1b[31m\rcondition"), "{work:?}");

        app.turn_active = true;
        agent_says(&mut app, "m1", "j'ai travaillé");
        app.turn_end();
        app.turn_active = true;
        agent_says(&mut app, "m2", "il manque\x1b[31m\rX\nVERDICT: CONTINUE");

        let continuation = app.turn_end().expect("prompt de continuation");
        assert!(
            continuation.contains("il manque\x1b[31m\rX"),
            "{continuation:?}"
        );
        assert!(
            continuation.contains("safe\x1b[31m\rcondition"),
            "{continuation:?}"
        );
    }

    #[test]
    fn a_finished_work_turn_hands_over_to_the_evaluator() {
        let mut app = App::new(None);
        app.goal_set("les tests passent", 10);
        app.turn_active = true;
        agent_says(&mut app, "m1", "j'ai corrigé le test");

        let prompt = app.turn_end().expect("prompt d'évaluation");

        assert!(prompt.contains("VERDICT: MET"));
        assert!(prompt.contains("les tests passent"));
        assert_eq!(app.goal_driver(), GoalDriver::Evaluating);
    }

    #[test]
    fn a_continue_verdict_relaunches_the_work_with_the_feedback() {
        let mut app = App::new(None);
        app.goal_set("les tests passent", 10);
        app.turn_active = true;
        agent_says(&mut app, "m1", "j'ai corrigé le test");
        app.turn_end();
        goal_events(&mut app);

        app.turn_active = true;
        agent_says(&mut app, "m2", "il manque le cas nul\nVERDICT: CONTINUE");
        let prompt = app.turn_end().expect("prompt de continuation");

        assert!(prompt.contains("il manque le cas nul"));
        assert!(prompt.contains("Continue le travail vers : les tests passent"));
        let goal = app.goal.as_ref().expect("un but");
        assert_eq!(goal.iteration, 2);
        assert_eq!(app.goal_driver(), GoalDriver::Working);
        let events = goal_events(&mut app);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, "goal_iteration");
        assert!(events[0].1.contains("continue"));
        assert!(events[0].1.contains("il manque le cas nul"));
    }

    #[test]
    fn a_met_verdict_ends_the_goal() {
        let mut app = App::new(None);
        app.goal_set("les tests passent", 10);
        app.turn_active = true;
        agent_says(&mut app, "m1", "fait");
        app.turn_end();

        app.turn_active = true;
        agent_says(&mut app, "m2", "tout vérifié\nVERDICT: MET");

        assert!(app.turn_end().is_none());
        assert_eq!(app.goal_driver(), GoalDriver::Idle);
        assert_eq!(
            app.goal.as_ref().and_then(|g| g.outcome),
            Some(kaji_core::goal::GoalOutcome::Met)
        );
        assert!(app.chat.iter().any(|l| l.text.contains("but atteint")));
        let events = goal_events(&mut app);
        assert_eq!(events.last().expect("goal_end").0, "goal_end");
    }

    #[test]
    fn an_unreachable_verdict_ends_the_goal() {
        let mut app = App::new(None);
        app.goal_set("réécrire le noyau Linux", 10);
        app.turn_active = true;
        agent_says(&mut app, "m1", "j'ai essayé");
        app.turn_end();

        app.turn_active = true;
        agent_says(&mut app, "m2", "hors périmètre\nVERDICT: UNREACHABLE");

        assert!(app.turn_end().is_none());
        assert_eq!(
            app.goal.as_ref().and_then(|g| g.outcome),
            Some(kaji_core::goal::GoalOutcome::Unreachable)
        );
        assert!(app.chat.iter().any(|l| l.text.contains("inatteignable")));
    }

    /// ⛔ BARRIÈRE — fail-closed : un évaluateur qui oublie la ligne de
    /// verdict ne doit jamais clore le but ; la boucle continue.
    #[test]
    fn an_absent_verdict_continues_by_caution() {
        let mut app = App::new(None);
        app.goal_set("les tests passent", 10);
        app.turn_active = true;
        agent_says(&mut app, "m1", "fait");
        app.turn_end();

        app.turn_active = true;
        agent_says(&mut app, "m2", "ça a l'air bon");
        let prompt = app.turn_end().expect("prompt de continuation");

        assert!(prompt.contains("ça a l'air bon"));
        assert!(app.chat.iter().any(|l| l.text.contains("verdict absent")));
        assert_eq!(app.goal.as_ref().expect("un but").iteration, 2);
    }

    /// ⛔ BARRIÈRE — un provider en échec livre un bloc `Error` sur un stream
    /// qui se termine normalement : sans garde-fou, la boucle enchaîne un
    /// tour d'évaluation contre le provider qui vient de rater.
    #[test]
    fn a_provider_error_during_a_work_turn_interrupts_the_goal() {
        let mut app = App::new(None);
        app.goal_set("les tests passent", 10);
        app.turn_active = true;
        agent_errors(&mut app, "m1", "rate limit atteint");

        assert!(app.turn_end().is_none());
        assert_eq!(app.goal_driver(), GoalDriver::Idle);
        assert_eq!(
            app.goal.as_ref().and_then(|g| g.outcome),
            Some(GoalOutcome::Interrupted)
        );
    }

    /// ⛔ BARRIÈRE — le buffer évaluateur reste vide quand le provider rate,
    /// et « verdict absent » relancerait alors la boucle jusqu'au cap.
    #[test]
    fn a_provider_error_during_the_evaluator_turn_interrupts_the_goal() {
        let mut app = App::new(None);
        app.goal_set("les tests passent", 10);
        app.turn_active = true;
        agent_says(&mut app, "m1", "j'ai corrigé le test");
        app.turn_end();

        app.reset_turn_visibility();
        app.turn_active = true;
        agent_errors(&mut app, "m2", "rate limit atteint");

        assert!(app.turn_end().is_none());
        assert_eq!(app.goal_driver(), GoalDriver::Idle);
        assert!(
            !app.chat.iter().any(|l| l.text.contains("verdict absent")),
            "un échec provider n'est pas un évaluateur bavard"
        );
    }

    #[test]
    fn a_provider_error_during_an_sdd_turn_interrupts_the_pass() {
        let mut app = App::new(Some(spec()));
        app.start_pass();
        app.gate_approve();
        app.turn_active = true;
        agent_errors(&mut app, "m1", "rate limit atteint");

        assert!(app.turn_end().is_none());
        assert_eq!(app.driver, PassDriver::Idle);
        assert!(!app.pass.is_running());
    }

    /// ⛔ BARRIÈRE — backstop de la boucle non supervisée : un évaluateur
    /// qui répond CONTINUE sans fin s'arrête au cap.
    #[test]
    fn the_iteration_cap_ends_the_goal() {
        let mut app = App::new(None);
        app.goal_set("les tests passent", 1);

        app.turn_active = true;
        agent_says(&mut app, "m1", "fait");
        app.turn_end();
        app.turn_active = true;
        agent_says(&mut app, "m2", "il reste X\nVERDICT: CONTINUE");

        assert!(app.turn_end().is_none());
        assert_eq!(
            app.goal.as_ref().and_then(|g| g.outcome),
            Some(kaji_core::goal::GoalOutcome::IterationCap)
        );
        assert!(app.chat.iter().any(|l| l.text.contains("cap")));
    }

    #[test]
    fn esc_during_a_goal_turn_interrupts_the_goal() {
        let mut app = App::new(None);
        app.goal_set("les tests passent", 10);
        app.turn_active = true;

        assert_eq!(app.on_event(&key(KeyCode::Esc)), Action::CancelTurn);
        app.goal_abort("目標 tour annulé — but interrompu");

        assert_eq!(app.goal_driver(), GoalDriver::Idle);
        assert_eq!(
            app.goal.as_ref().and_then(|g| g.outcome),
            Some(kaji_core::goal::GoalOutcome::Interrupted)
        );
        let events = goal_events(&mut app);
        assert_eq!(events.last().expect("goal_end").0, "goal_end");
        assert!(events.last().expect("goal_end").1.contains("interrupted"));
    }

    /// ⛔ BARRIÈRE — exclusivité des deux drivers : deux boucles qui
    /// enchaînent des tours sur le même `turn_end` se voleraient les prompts.
    #[test]
    fn a_goal_is_refused_while_an_sdd_pass_runs() {
        let mut app = App::new(Some(spec()));
        app.start_pass();

        assert!(app.goal_set("les tests passent", 10).is_none());
        assert!(app.goal.is_none());
        assert!(app.chat.iter().any(|l| l.text.contains("passe SDD")));
    }

    #[test]
    fn an_sdd_pass_is_refused_while_a_goal_runs() {
        let mut app = App::new(Some(spec()));
        app.goal_set("les tests passent", 10);

        app.start_pass();

        assert!(!app.pass.is_running());
        assert!(!app.gate_open);
        assert!(app.chat.iter().any(|l| l.text.contains("but en cours")));
    }

    #[test]
    fn slash_goal_clear_stops_the_goal_and_cancels_the_turn() {
        let mut app = App::new(None);
        app.goal_set("les tests passent", 10);
        app.turn_active = true;
        goal_events(&mut app);

        let action = submit(&mut app, "/goal clear");

        assert_eq!(action, Action::GoalClear);
        assert_eq!(app.goal_driver(), GoalDriver::Idle);
        assert_eq!(
            app.goal.as_ref().and_then(|g| g.outcome),
            Some(kaji_core::goal::GoalOutcome::Cleared)
        );
        let events = goal_events(&mut app);
        assert!(events.last().expect("goal_end").1.contains("cleared"));
    }

    #[test]
    fn slash_goal_clear_without_a_goal_leaves_the_turn_alone() {
        let mut app = App::new(None);
        app.turn_active = true;

        assert_eq!(submit(&mut app, "/goal clear"), Action::None);
        assert!(app.chat.iter().any(|l| l.text.contains("aucun but")));
    }

    /// Ruling 6 : un but ne se fixe pas au milieu d'un tour (il faut le
    /// premier prompt de travail) — sauf `/goal clear`, testé ci-dessus.
    #[test]
    fn setting_a_goal_mid_turn_is_refused_rather_than_queued_as_steering() {
        let mut app = App::new(None);
        app.turn_active = true;

        let action = submit(&mut app, "/goal les tests passent");

        assert_eq!(action, Action::None);
        assert!(app.steer_queue.is_empty(), "ni file de steering");
        assert!(app.goal.is_none());
        assert!(app
            .chat
            .iter()
            .any(|l| l.text.contains("tour est en cours")));
    }

    #[test]
    fn goal_status_reports_the_live_iteration_and_phase() {
        let mut app = App::new(None);
        app.goal_set("les tests passent", 10);
        app.turn_active = true;
        agent_says(&mut app, "m1", "fait");
        app.turn_end();

        app.push_goal_status();

        let status = app.chat.last().expect("une ligne de statut");
        assert!(status.text.contains("les tests passent"));
        assert!(status.text.contains("1/10"));
        assert!(status.text.contains("évaluation"));
    }

    #[test]
    fn setting_a_goal_replaces_the_previous_one() {
        let mut app = App::new(None);
        app.goal_set("premier but", 10);
        goal_events(&mut app);

        let prompt = app.goal_set("second but", 10).expect("prompt de travail");

        assert!(prompt.contains("second but"));
        assert_eq!(app.goal.as_ref().expect("un but").condition, "second but");
        assert_eq!(app.goal.as_ref().expect("un but").iteration, 1);
        let kinds: Vec<&str> = goal_events(&mut app).iter().map(|(k, _)| *k).collect();
        assert_eq!(kinds, vec!["goal_end", "goal_start"]);
    }

    #[expect(deprecated)]
    fn logging_notification(data: serde_json::Value) -> AgentEvent {
        AgentEvent::McpNotification((
            "req-1".to_string(),
            ServerNotification::LoggingMessageNotification(rmcp::model::Notification::new(
                rmcp::model::LoggingMessageNotificationParam::new(
                    rmcp::model::LoggingLevel::Info,
                    data,
                ),
            )),
        ))
    }

    #[test]
    fn subagent_tool_request_feeds_the_forge() {
        let mut app = App::new(None);

        app.apply_agent_event(&logging_notification(serde_json::json!({
            "type": SUBAGENT_TOOL_REQUEST_TYPE,
            "subagent_id": "task_1",
            "tool_call": { "name": "developer__shell", "arguments": { "command": "ls" } },
        })));

        let task = app.forge.tasks.get("task_1").expect("une tâche de forge");
        assert_eq!(task.current_tool.as_deref(), Some("developer__shell"));
    }

    #[test_case(serde_json::json!({
        "type": "autre_chose",
        "subagent_id": "task_1",
        "tool_call": { "name": "developer__shell" },
    }) ; "un autre type de payload")]
    #[test_case(serde_json::json!({
        "type": SUBAGENT_TOOL_REQUEST_TYPE,
        "tool_call": { "name": "developer__shell" },
    }) ; "sans subagent_id")]
    #[test_case(serde_json::json!("pas un objet") ; "data qui n'est pas un objet")]
    fn foreign_payloads_leave_the_forge_untouched(data: serde_json::Value) {
        let mut app = App::new(None);

        app.apply_agent_event(&logging_notification(data));

        assert!(app.forge.tasks.is_empty());
    }

    fn blade(id: &str, description: &str, status: forge::ForgeStatus) -> forge::ForgeTask {
        forge::ForgeTask {
            id: id.to_string(),
            description: description.to_string(),
            status,
            current_tool: None,
            elapsed_secs: 7,
            turns: 2,
            result: None,
            error: None,
            seq: 0,
        }
    }

    fn snapshot(
        id: &str,
        description: &str,
        status: SubagentTaskStatus,
        turns: u32,
    ) -> SubagentTaskSnapshot {
        SubagentTaskSnapshot {
            id: id.to_string(),
            description: description.to_string(),
            status,
            turns,
            elapsed_secs: 7,
            result: None,
            error: None,
        }
    }

    /// Les tâches sont posées à la main : `ForgeView::Auto` reste intact, donc
    /// le volet n'est visible que si une lame tourne encore. L'ordre du `Vec`
    /// est l'ordre d'arrivée.
    fn app_at_the_forge(blades: Vec<forge::ForgeTask>) -> App {
        let mut app = App::new(None);
        for (rank, mut blade) in blades.into_iter().enumerate() {
            blade.seq = rank as u64;
            app.forge.tasks.insert(blade.id.clone(), blade);
        }
        app
    }

    #[test]
    fn ctrl_f_opens_the_forge_then_gives_the_composer_its_keys_back() {
        let mut app = app_at_the_forge(vec![blade(
            "t1",
            "auditer les tests",
            forge::ForgeStatus::Done,
        )]);
        assert!(!app.forge.visible());

        assert_eq!(app.on_event(&ctrl_key(KeyCode::Char('f'))), Action::None);
        assert_eq!(app.forge.view, forge::ForgeView::ForcedOpen);
        assert_eq!(app.focus, Focus::Forge);

        app.on_event(&ctrl_key(KeyCode::Char('f')));
        assert_eq!(app.forge.view, forge::ForgeView::ForcedClosed);
        assert_eq!(app.focus, Focus::Composer);
        assert!(app.input.is_empty(), "Ctrl+F ne tape rien dans le composer");
    }

    #[test]
    fn slash_forge_toggles_the_panel_like_the_chord() {
        let mut app = app_at_the_forge(vec![blade(
            "t1",
            "auditer les tests",
            forge::ForgeStatus::Done,
        )]);

        for c in "/forge".chars() {
            app.on_event(&key(KeyCode::Char(c)));
        }
        assert_eq!(app.on_event(&key(KeyCode::Enter)), Action::None);

        assert_eq!(app.forge.view, forge::ForgeView::ForcedOpen);
        assert_eq!(app.focus, Focus::Forge);
        assert!(app.input.is_empty());
    }

    /// `f` depuis le volet ouvre la vue plein écran ; `q` la referme et rend
    /// le volet — la forge est la même, seule la surface change.
    #[test]
    fn f_from_the_forge_panel_opens_the_mission_control_and_q_comes_back() {
        let mut app = app_at_the_forge(vec![blade(
            "t1",
            "auditer les tests",
            forge::ForgeStatus::Running,
        )]);
        app.focus = Focus::Forge;

        app.on_event(&key(KeyCode::Char('f')));
        assert!(app.mission.open);
        assert!(app.input.is_empty(), "f ne tape rien dans le composer");

        app.on_event(&key(KeyCode::Char('q')));
        assert!(!app.mission.open);
        assert_eq!(app.focus, Focus::Forge, "le volet reprend la main");
        assert!(app.input.is_empty());
    }

    #[test]
    fn slash_forge_full_opens_the_mission_control() {
        let mut app = app_at_the_forge(vec![blade(
            "t1",
            "auditer les tests",
            forge::ForgeStatus::Running,
        )]);

        for c in "/forge full".chars() {
            app.on_event(&key(KeyCode::Char(c)));
        }
        assert_eq!(app.on_event(&key(KeyCode::Enter)), Action::None);

        assert!(app.mission.open);
        assert!(app.input.is_empty());
    }

    #[test]
    fn an_unknown_forge_argument_says_its_usage() {
        let mut app = App::new(None);

        assert_eq!(app.run_forge_command("plein"), Action::None);

        assert!(!app.mission.open);
        assert!(app
            .chat
            .last()
            .expect("une ligne système")
            .text
            .contains("usage : /forge [full]"));
    }

    /// Plein écran veut dire plein écran : tant qu'il est ouvert, la vue prend
    /// la touche avant les accords de volets — `Ctrl+F` replierait une forge
    /// qu'on ne verrait plus — et une lettre n'atterrit jamais dans le
    /// composer derrière.
    #[test]
    fn the_mission_control_swallows_the_pane_chords_and_the_composer() {
        let mut app = app_at_the_forge(vec![blade(
            "t1",
            "auditer les tests",
            forge::ForgeStatus::Running,
        )]);
        app.open_mission_control();
        let view = app.forge.view;

        app.on_event(&ctrl_key(KeyCode::Char('f')));
        app.on_event(&ctrl_key(KeyCode::Char('e')));
        app.on_event(&key(KeyCode::Char('z')));

        assert!(app.mission.open);
        assert_eq!(app.forge.view, view);
        assert!(app.explorer.is_none());
        assert!(app.input.is_empty());
    }

    /// h/l changent de stage, j/k de carte, et changer de colonne repose l'œil
    /// en tête : la carte 3 d'un stage n'a rien à voir avec celle du suivant.
    #[test]
    fn the_mission_control_navigates_stages_and_cards() {
        use kaji::workflow::{AgentState, AgentStatus, StageState, StageStatus, WorkflowState};
        use kaji_core::workflow::Gate;

        let agents = |names: &[&str]| -> Vec<AgentStatus> {
            names
                .iter()
                .map(|name| AgentStatus {
                    name: name.to_string(),
                    state: AgentState::Running,
                    session_id: None,
                    tokens: 0,
                    duration_ms: 0,
                })
                .collect()
        };
        let mut app = App::new(None);
        app.apply_workflow_snapshot(Some(WorkflowState {
            workflow: "revue".to_string(),
            stages: vec![
                StageStatus {
                    name: "collecte".to_string(),
                    state: StageState::Running,
                    gate: Gate::Auto,
                    agents: agents(&["a", "b", "c"]),
                },
                StageStatus {
                    name: "synthese".to_string(),
                    state: StageState::Pending,
                    gate: Gate::Auto,
                    agents: agents(&["d"]),
                },
            ],
        }));
        app.open_mission_control();

        app.on_event(&key(KeyCode::Char('j')));
        app.on_event(&key(KeyCode::Char('j')));
        assert_eq!(app.mission.card, 2);
        app.on_event(&key(KeyCode::Char('j')));
        assert_eq!(app.mission.card, 2, "jamais sous la dernière carte");

        app.on_event(&key(KeyCode::Char('l')));
        assert_eq!(app.mission.stage, 1);
        assert_eq!(app.mission.card, 0, "changer de stage repose l'œil en tête");

        app.on_event(&key(KeyCode::Char('l')));
        assert_eq!(app.mission.stage, 1, "jamais au-delà du dernier stage");

        app.on_event(&key(KeyCode::Char('h')));
        assert_eq!(app.mission.stage, 0);
        app.on_event(&key(KeyCode::Char('h')));
        assert_eq!(app.mission.stage, 0, "jamais avant le premier");
    }

    /// Un workflow qui rétrécit ne doit pas laisser la sélection désigner un
    /// stage disparu — le prochain rendu lirait dans le vide.
    #[test]
    fn a_shrinking_workflow_clamps_the_mission_selection() {
        use kaji::workflow::{AgentState, AgentStatus, StageState, StageStatus, WorkflowState};
        use kaji_core::workflow::Gate;

        let stage = |name: &str, count: usize| StageStatus {
            name: name.to_string(),
            state: StageState::Running,
            gate: Gate::Auto,
            agents: (0..count)
                .map(|rank| AgentStatus {
                    name: format!("{name}-{rank}"),
                    state: AgentState::Running,
                    session_id: None,
                    tokens: 0,
                    duration_ms: 0,
                })
                .collect(),
        };
        let mut app = App::new(None);
        app.apply_workflow_snapshot(Some(WorkflowState {
            workflow: "revue".to_string(),
            stages: vec![stage("a", 3), stage("b", 3)],
        }));
        app.open_mission_control();
        app.mission.stage = 1;
        app.mission.card = 2;

        app.apply_workflow_snapshot(Some(WorkflowState {
            workflow: "revue".to_string(),
            stages: vec![stage("a", 1)],
        }));

        assert_eq!(app.mission.stage, 0);
        assert_eq!(app.mission.card, 0);
    }

    /// Soixante kanji valent cent-vingt cellules : un titre borné en `chars()`
    /// débordait de l'en-tête du lecteur, qui le rognait sans le dire.
    #[test]
    fn a_sheet_title_is_bounded_in_cells_not_chars() {
        let title = forge_sheet_title(&"監".repeat(80));

        assert!(
            gitstatus::display_width(&title) <= FORGE_SHEET_TITLE + 3,
            "{title:?} fait {} cellules",
            gitstatus::display_width(&title)
        );
        assert!(title.starts_with(theme::SUBAGENT_GLYPH), "{title:?}");
        assert!(title.ends_with('…'), "la coupe se dit : {title:?}");
    }

    /// Le repli se compte en cellules : une prose japonaise repliée sur des
    /// `chars()` rend des lignes deux fois trop larges.
    #[test]
    fn wrapping_counts_cells_not_chars() {
        let text = "監査 実行 結果 確認 報告 検証 修正 完了";

        for line in wrap_words(text, 20) {
            assert!(
                gitstatus::display_width(&line) <= 20,
                "{line:?} fait {} cellules",
                gitstatus::display_width(&line)
            );
        }
    }

    /// Un mot plus large que la largeur part seul sur sa ligne — le lecteur
    /// rogne, il ne coupe pas un chemin en deux.
    #[test]
    fn wrapping_leaves_an_oversized_word_on_its_own_line() {
        let long = "x".repeat(30);

        let lines = wrap_words(&format!("a {long} b"), 10);

        assert_eq!(lines, vec!["a".to_string(), long, "b".to_string()]);
    }

    /// Sans extension summon, aucune lame n'a jamais existé : ouvrir donnerait
    /// un cadre muet pour toute réponse.
    #[test]
    fn opening_an_empty_forge_says_there_is_nothing_to_show() {
        let mut app = App::new(None);

        app.on_event(&ctrl_key(KeyCode::Char('f')));

        assert_eq!(app.forge.view, forge::ForgeView::Auto);
        assert!(!app.forge.visible());
        assert_eq!(app.focus, Focus::Composer);
        assert!(app
            .chat
            .last()
            .expect("une ligne système")
            .text
            .contains("forge : aucune tâche"));
    }

    #[test]
    fn the_forge_arrows_are_clamped_to_the_blades() {
        let mut app = app_at_the_forge(vec![
            blade("t1", "auditer les tests", forge::ForgeStatus::Running),
            blade("t2", "relire le diff", forge::ForgeStatus::Running),
        ]);
        app.focus = Focus::Forge;

        app.on_event(&key(KeyCode::Up));
        assert_eq!(app.forge.selected, 0, "jamais au-dessus de la première");

        app.on_event(&key(KeyCode::Down));
        assert_eq!(app.forge.selected, 1);
        app.on_event(&key(KeyCode::Char('j')));
        assert_eq!(app.forge.selected, 1, "jamais au-delà de la dernière");
        app.on_event(&key(KeyCode::Char('k')));
        assert_eq!(app.forge.selected, 0);
        assert!(app.input.is_empty(), "rien ne fuit dans le composer");
    }

    #[test]
    fn enter_opens_the_blade_sheet_in_the_reader() {
        let mut app = app_at_the_forge(vec![blade(
            "t1",
            "auditer les tests",
            forge::ForgeStatus::Running,
        )]);
        app.forge.tasks.get_mut("t1").unwrap().current_tool = Some("developer__shell".to_string());
        app.focus = Focus::Forge;

        assert_eq!(app.on_event(&key(KeyCode::Enter)), Action::None);

        let viewer = app.viewer.as_ref().expect("fiche ouverte");
        assert!(viewer.path.contains("auditer les tests"), "{}", viewer.path);
        let sheet = viewer.lines.join("\n");
        for expected in ["tâche", "statut", "en cours", "7s", "tours", "2", "outil"] {
            assert!(sheet.contains(expected), "{expected} manquant :\n{sheet}");
        }
        assert_eq!(app.focus, Focus::Viewer);
    }

    #[test]
    fn a_finished_blade_sheet_carries_its_verdict() {
        let mut app = app_at_the_forge(vec![blade(
            "t1",
            "auditer les tests",
            forge::ForgeStatus::Failed,
        )]);
        app.forge.tasks.get_mut("t1").unwrap().error = Some("compilation cassée".to_string());
        app.forge.view = forge::ForgeView::ForcedOpen;
        app.focus = Focus::Forge;

        app.on_event(&key(KeyCode::Enter));

        let sheet = app.viewer.as_ref().expect("fiche").lines.join("\n");
        assert!(sheet.contains("échec"), "{sheet}");
        assert!(sheet.contains("erreur"), "{sheet}");
        assert!(sheet.contains("compilation cassée"), "{sheet}");
        assert!(!sheet.contains("outil"), "une lame morte ne brûle rien");
    }

    /// Le lecteur rogne les lignes trop longues sans le dire : une description
    /// de délégation, qui fait une phrase, doit être repliée avant d'y entrer.
    #[test]
    fn a_long_description_is_folded_under_the_value_column() {
        let long = "auditer la couverture des tests du volet forge et de sa barre \
                    d'état, puis relire chaque assertion une à une";
        let mut app = app_at_the_forge(vec![blade("t1", long, forge::ForgeStatus::Running)]);
        app.focus = Focus::Forge;

        app.on_event(&key(KeyCode::Enter));

        let lines = &app.viewer.as_ref().expect("fiche").lines;
        assert!(lines[0].starts_with("tâche    : "), "{:?}", lines[0]);
        assert!(
            lines.iter().all(|line| line.chars().count() <= 76 + 11),
            "{lines:?}"
        );
        assert!(
            lines[1].starts_with("           ") && !lines[1].trim().is_empty(),
            "la suite se range sous la valeur : {:?}",
            lines[1]
        );
        assert_eq!(
            format!("{}{}", lines[0].trim_start_matches("tâche    : "), lines[1])
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" "),
            long,
            "repliée, pas tronquée"
        );
        assert!(lines[2].starts_with("statut"), "{:?}", lines[2]);
    }

    /// La fiche est une vue, pas une copie : le tick la réécrit sous les yeux
    /// plutôt que de laisser un statut périmé à l'écran.
    #[test]
    fn the_tick_rewrites_the_open_sheet_and_keeps_the_reading_position() {
        let mut app = app_at_the_forge(vec![blade(
            "t1",
            "auditer les tests",
            forge::ForgeStatus::Running,
        )]);
        app.focus = Focus::Forge;
        app.on_event(&key(KeyCode::Enter));
        app.viewer.as_mut().expect("fiche").scroll = 2;

        app.forge.tasks.get_mut("t1").unwrap().status = forge::ForgeStatus::Done;
        app.refresh_forge_sheet();

        let viewer = app.viewer.as_ref().expect("fiche toujours ouverte");
        assert_eq!(viewer.scroll, 2);
        assert!(viewer.lines.join("\n").contains("terminé"));
    }

    #[test]
    fn a_file_in_the_reader_is_left_alone_by_the_forge_tick() {
        let (mut app, _dir) = app_with_open_viewer();
        let before = app.viewer.as_ref().expect("lecteur").lines.clone();

        app.refresh_forge_sheet();

        assert_eq!(app.viewer.as_ref().expect("lecteur").lines, before);
    }

    /// Le lecteur montre un fichier ou une fiche, et ses touches de fichier ne
    /// valent que pour un fichier : `e` ouvrirait un éditeur sur un chemin
    /// fantôme (et le créerait), `a` attacherait `遣 …` en @mention, `r`
    /// relirait un fichier qui n'existe pas.
    #[test]
    fn the_file_keys_of_the_reader_do_nothing_on_a_forge_sheet() {
        let mut app = app_at_the_forge(vec![blade(
            "t1",
            "auditer les tests",
            forge::ForgeStatus::Running,
        )]);
        app.focus = Focus::Forge;
        app.on_event(&key(KeyCode::Enter));
        let sheet = app.viewer.as_ref().expect("fiche").lines.clone();
        let chat = app.chat.len();

        assert_eq!(app.on_event(&key(KeyCode::Char('e'))), Action::None);
        app.on_event(&key(KeyCode::Char('a')));
        app.on_event(&key(KeyCode::Char('r')));

        assert!(app.input.is_empty(), "rien n'est attaché au composer");
        assert_eq!(app.focus, Focus::Viewer, "la fiche garde le clavier");
        assert_eq!(app.viewer.as_ref().expect("fiche").lines, sheet);
        assert_eq!(app.chat.len(), chat, "aucune ligne système inventée");
    }

    #[test]
    fn the_file_keys_of_the_reader_still_answer_on_a_file() {
        let (mut app, _dir) = app_with_open_viewer();

        app.on_event(&key(KeyCode::Char('a')));

        assert_eq!(app.input, "@long.txt ");
    }

    /// Une description multi-ligne mettrait un vrai saut de ligne dans le titre
    /// du cadre du lecteur, qui n'en attend pas.
    #[test]
    fn a_multiline_description_never_breaks_the_sheet_title() {
        let mut app = app_at_the_forge(vec![blade(
            "t1",
            "auditer les tests\npuis relire le diff",
            forge::ForgeStatus::Running,
        )]);
        app.focus = Focus::Forge;

        app.on_event(&key(KeyCode::Enter));

        let path = &app.viewer.as_ref().expect("fiche").path;
        assert!(!path.contains('\n'), "{path:?}");
        assert!(path.contains('␊'), "{path:?}");
    }

    /// Le snapshot bat une fois par seconde, la notification d'outil arrive
    /// entre deux : la fiche ouverte doit dire l'outil courant, pas celui du
    /// tick précédent.
    #[test]
    fn a_tool_notification_refreshes_the_open_sheet() {
        let mut app = app_at_the_forge(vec![blade(
            "t1",
            "auditer les tests",
            forge::ForgeStatus::Running,
        )]);
        app.focus = Focus::Forge;
        app.on_event(&key(KeyCode::Enter));
        assert!(!app
            .viewer
            .as_ref()
            .expect("fiche")
            .lines
            .join("\n")
            .contains("outil"));

        app.apply_agent_event(&logging_notification(serde_json::json!({
            "type": SUBAGENT_TOOL_REQUEST_TYPE,
            "subagent_id": "t1",
            "tool_call": { "name": "developer__shell" },
        })));

        let sheet = app.viewer.as_ref().expect("fiche").lines.join("\n");
        assert!(sheet.contains("outil    : developer__shell"), "{sheet}");
    }

    /// Une fiche ouverte puis remplacée par un fichier : le tick n'a plus rien à
    /// suivre, et surtout pas à réécrire le fichier avec une fiche.
    #[test]
    fn opening_a_file_over_a_sheet_ends_the_forge_tick_s_claim_on_the_reader() {
        let (mut app, dir) = app_with_open_viewer();
        std::fs::write(dir.path().join("note.md"), "une note\n").unwrap();
        app.forge.tasks.insert(
            "t1".to_string(),
            blade("t1", "auditer les tests", forge::ForgeStatus::Running),
        );
        app.forge.selected = 0;
        app.focus = Focus::Forge;
        app.on_event(&key(KeyCode::Enter));
        assert!(app.viewer.as_ref().expect("fiche").path.contains("auditer"));

        app.open_viewer("note.md");
        app.refresh_forge_sheet();

        let viewer = app.viewer.as_ref().expect("le fichier");
        assert_eq!(viewer.path, "note.md");
        assert_eq!(viewer.lines, vec!["une note"]);

        app.close_viewer();
        app.refresh_forge_sheet();
        assert!(app.viewer.is_none(), "rien ne rouvre le lecteur tout seul");
    }

    /// Deux délégations peuvent porter la même description — c'est même le cas
    /// courant d'un fan-out. La fiche suit la tâche ouverte, pas son libellé.
    #[test]
    fn two_blades_sharing_a_description_keep_their_own_sheet() {
        let mut app = app_at_the_forge(vec![
            blade("t1", "auditer les tests", forge::ForgeStatus::Running),
            blade("t2", "auditer les tests", forge::ForgeStatus::Running),
        ]);
        app.focus = Focus::Forge;
        app.on_event(&key(KeyCode::Down));
        app.on_event(&key(KeyCode::Enter));

        app.forge.apply_snapshot(vec![
            snapshot("t1", "auditer les tests", SubagentTaskStatus::Running, 9),
            snapshot("t2", "auditer les tests", SubagentTaskStatus::Running, 3),
        ]);
        app.refresh_forge_sheet();

        let sheet = app.viewer.as_ref().expect("fiche").lines.join("\n");
        assert!(
            sheet.contains("tours    : 3"),
            "la fiche doit porter t2, pas sa jumelle :\n{sheet}"
        );
    }

    /// Une lame vue par sa notification d'outil n'a que son id pour nom, jusqu'au
    /// premier snapshot. La fiche ouverte entre les deux suit le renommage au
    /// lieu de geler sur un titre que plus rien n'alimente.
    #[test]
    fn a_sheet_opened_before_the_first_snapshot_follows_the_renaming() {
        let mut app = App::new(None);
        app.forge
            .apply_tool_notification("task_7", "developer__shell");
        app.focus = Focus::Forge;
        app.on_event(&key(KeyCode::Enter));
        assert_eq!(app.viewer.as_ref().expect("fiche").path, "遣 task_7");

        app.forge.apply_snapshot(vec![snapshot(
            "task_7",
            "relire le diff",
            SubagentTaskStatus::Running,
            4,
        )]);
        app.refresh_forge_sheet();

        let viewer = app.viewer.as_ref().expect("fiche");
        assert_eq!(viewer.path, "遣 relire le diff");
        let sheet = viewer.lines.join("\n");
        assert!(sheet.contains("relire le diff"), "{sheet}");
        assert!(sheet.contains("tours    : 4"), "{sheet}");
    }

    /// Un résultat ou une erreur arrive d'un agent : rien ne garantit qu'il soit
    /// déjà replié. Non replié ici, le lecteur le rogne sans le dire — et la
    /// fiche n'existe que pour rendre ça lisible.
    #[test]
    fn a_long_result_is_folded_like_the_rest_of_the_sheet() {
        let mut app = app_at_the_forge(vec![blade(
            "t1",
            "auditer les tests",
            forge::ForgeStatus::Done,
        )]);
        app.forge.tasks.get_mut("t1").unwrap().result =
            Some(["verdict"; 25].join(" une ligne de deux cents caractères "));
        app.forge.view = forge::ForgeView::ForcedOpen;
        app.focus = Focus::Forge;

        app.on_event(&key(KeyCode::Enter));

        let lines = &app.viewer.as_ref().expect("fiche").lines;
        assert!(
            lines.iter().all(|line| line.chars().count() <= 76 + 11),
            "{lines:?}"
        );
        assert!(lines.iter().any(|line| line.starts_with("résultat")));
        assert!(lines.iter().filter(|line| line.contains("verdict")).count() > 1);
    }

    #[test]
    fn x_on_a_live_blade_asks_before_it_cancels() {
        let mut app = app_at_the_forge(vec![blade(
            "t1",
            "auditer les tests",
            forge::ForgeStatus::Running,
        )]);
        app.focus = Focus::Forge;

        assert_eq!(app.on_event(&key(KeyCode::Char('x'))), Action::None);

        assert_eq!(app.pending_forge_cancel.as_deref(), Some("t1"));
        let line = &app.chat.last().expect("la question").text;
        assert!(
            line.contains("annuler") && line.contains("auditer les tests") && line.contains("y/n"),
            "{line}"
        );

        assert_eq!(app.on_event(&key(KeyCode::Char('y'))), Action::ForgeCancel);
        assert_eq!(app.take_pending_forge_cancel().as_deref(), Some("t1"));
    }

    #[test]
    fn x_on_a_finished_blade_says_so_instead_of_arming_anything() {
        let mut app = app_at_the_forge(vec![blade(
            "t1",
            "auditer les tests",
            forge::ForgeStatus::Done,
        )]);
        app.forge.view = forge::ForgeView::ForcedOpen;
        app.focus = Focus::Forge;

        app.on_event(&key(KeyCode::Char('x')));

        assert!(app.pending_forge_cancel.is_none());
        assert!(app
            .chat
            .last()
            .expect("une ligne système")
            .text
            .contains("déjà terminée"));
    }

    #[test]
    fn anything_but_y_disarms_the_cancel_and_leaves_the_blade_running() {
        let mut app = app_at_the_forge(vec![blade(
            "t1",
            "auditer les tests",
            forge::ForgeStatus::Running,
        )]);
        app.focus = Focus::Forge;
        app.on_event(&key(KeyCode::Char('x')));

        assert_eq!(app.on_event(&key(KeyCode::Char('n'))), Action::None);

        assert!(app.pending_forge_cancel.is_none());
        assert_eq!(app.forge.tasks["t1"].status, forge::ForgeStatus::Running);
        assert!(app.input.is_empty(), "la réponse ne se tape pas non plus");
    }

    #[test]
    fn ctrl_o_reaches_the_forge_only_while_it_is_open() {
        let mut app = app_at_the_forge(vec![blade(
            "t1",
            "auditer les tests",
            forge::ForgeStatus::Running,
        )]);
        assert!(app.forge.visible());

        app.on_event(&ctrl_key(KeyCode::Char('o')));
        assert_eq!(app.focus, Focus::Forge);
        app.on_event(&ctrl_key(KeyCode::Char('o')));
        assert_eq!(app.focus, Focus::Composer);

        app.forge.view = forge::ForgeView::ForcedClosed;
        app.on_event(&ctrl_key(KeyCode::Char('o')));
        assert_eq!(app.focus, Focus::Composer, "volet fermé : on l'enjambe");
    }

    /// Le repli automatique ne demande la permission à personne : le volet peut
    /// disparaître sous le focus, et les touches doivent revenir au composer
    /// plutôt qu'à un volet que personne ne voit.
    #[test]
    fn a_forge_that_folds_under_the_focus_hands_the_keys_back() {
        let mut app = app_at_the_forge(vec![blade(
            "t1",
            "auditer les tests",
            forge::ForgeStatus::Done,
        )]);
        app.forge.folds_at = Some(Instant::now() - Duration::from_secs(1));
        app.focus = Focus::Forge;

        app.on_event(&key(KeyCode::Char('j')));

        assert_eq!(app.focus, Focus::Composer);
        assert_eq!(app.input, "j");
    }
}
