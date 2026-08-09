# TUI Ratatui MVP — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Remplacer le délégateur `kaji tui` (TUI JS dépréciée via npx) par une TUI Ratatui **in-process** : chat streamé sur `Agent::reply` + panneau SPEC pilotant une passe SDD (Intent → SPEC → Gate → Exec → Validate → Drift lock) de bout en bout.

**Architecture:** Conforme à l'ADR IPC 2026-08-08 : le Core reste une lib embarquable, la TUI est un mode du binaire unique (`kaji tui`) qui appelle `Agent::reply(...) -> BoxStream<Result<AgentEvent>>` par appels de fonction — zéro réseau. L'état pur de la passe SDD vit dans `kaji-core` (`sdd` module, stdlib only) ; la TUI (kaji-cli) ne fait que rendre et déclencher les transitions. La mémoire KAJI est câblée à l'intérieur de `Agent::reply` (les deux chemins legacy/state-machine) : la TUI en bénéficie sans aucun code.

**Tech Stack:** ratatui (seule dépendance nouvelle, crossterm via ré-export `ratatui::crossterm`), tokio (présent), futures (présent), kaji-core (stdlib+rusqlite, inchangé côté deps).

## Global Constraints

- Dépendances : `cargo add` obligatoire pour l'entrée de dep (AGENTS.md) ; le câblage de feature `tui = ["dep:ratatui"]` est une édition manuelle légitime ; `Cargo.lock` doit rester cohérent.
- La feature `tui` est DÉJÀ dans `default` (`crates/kaji-cli/Cargo.toml:84`) et doit y rester.
- Interdits AGENTS.md : jamais de commentaire qui paraphrase le code ; `anyhow::Result` pour les erreurs ; jamais écraser le binaire `target/*/kaji` vivant en place ; jamais skip `cargo fmt` ; clippy `-D warnings` avant tout commit.
- **Parité agent-loop** : ce plan ne touche PAS à la boucle agent (la TUI est un client pur). Si une tâche semble exiger un changement dans `agents/agent.rs` ou `agents/state_machine/`, STOP — remonter au planificateur.
- 8 échecs préexistants dans `kaji --lib` (compaction ×2, gcpauth ×4, snapshot prompt_manager, cutoff) : ne pas les réparer, ne pas s'en inquiéter — vérifier seulement que le compte n'augmente pas.
- Textes UI et messages de commit en français, préfixe commit `kaji: `.
- Chaque tâche : `cargo fmt` avant commit ; commit sur la branche courante `feat/kaji-init` (pas de nouvelle branche).

## Références exactes (vérifiées dans le code)

- `Agent::reply(&self, user_message: Message, session_config: SessionConfig, cancel_token: Option<CancellationToken>) -> Result<BoxStream<'_, Result<AgentEvent>>>` — `crates/kaji/src/agents/agent.rs:1811`
- `AgentEvent { Message(Message), Usage(ProviderUsage), MessageUsage{..}, McpNotification(..), HistoryReplaced(Conversation) }` — `agent.rs:277-286`, ré-exporté `kaji::agents::AgentEvent`
- `SessionConfig { id: String, schedule_id: Option<String>, max_turns: Option<u32>, retry_config: Option<RetryConfig> }` — construction modèle `crates/kaji-cli/src/session/mod.rs:1310-1315`, import `kaji::agents::SessionConfig`
- `CliSession { agent: Agent, messages: Conversation, session_id: String, .. }` (champs privés) — `crates/kaji-cli/src/session/mod.rs:200-213`
- `build_session(SessionBuilderConfig) -> CliSession` — `crates/kaji-cli/src/session/builder.rs:499` ; `SessionBuilderConfig` a un `impl Default` manuel (`builder.rs:126-149`)
- `get_or_create_session_id(identifier: Option<Identifier>, resume: bool, fork: bool, kaji_mode: KajiMode) -> Result<Option<String>>` — `crates/kaji-cli/src/cli.rs:394` (vérifier la visibilité, passer `pub(crate)` si nécessaire)
- Variante clap actuelle `Command::Tui { args: Vec<String> }` — `cli.rs:1090-1108` ; bras de dispatch `cli.rs:2373-2374` ; nom dans `get_command_name()` `cli.rs:1376-1404`
- Handler npx à supprimer : `crates/kaji-cli/src/commands/tui.rs` (96 lignes + 2 tests npx)
- Le texte incrémental arrive en `AgentEvent::Message` successifs partageant le même `message.id` (fusion modèle : `Conversation::push`, `crates/kaji-provider-types/src/conversation.rs:72-90`)
- Blocs de contenu : `MessageContentBlock::{Text, ToolRequest, ToolResponse, Thinking, ...}` — `crates/kaji-provider-types/src/conversation/message.rs:243-255`

---

### Task 1: Module `sdd` dans kaji-core (état pur de la passe)

**Files:**
- Create: `crates/kaji-core/src/sdd.rs`
- Modify: `crates/kaji-core/src/lib.rs` (ajouter `pub mod sdd;`)
- Test: `crates/kaji-core/tests/sdd_test.rs`

**Interfaces:**
- Consumes: rien (stdlib only — kaji-core ne doit PAS gagner de dépendance).
- Produces (utilisé par Task 3/5 via `kaji_core::sdd::*`):
  - `enum SddStage { Intent, Spec, Gate, Exec, Validate, DriftLock }` + `SddStage::ALL: [SddStage; 6]` + `fn label(&self) -> &'static str`
  - `enum StageStatus { Pending, Running, Done, Failed }`
  - `struct SpecDoc { pub path: PathBuf, pub title: String, pub body: String }` + `SpecDoc::load(&Path) -> io::Result<SpecDoc>` + `SpecDoc::parse(PathBuf, &str) -> SpecDoc` + `fn is_empty(&self) -> bool`
  - `struct SddPass` + `new()`, `start()`, `advance()`, `fail_current()`, `current() -> Option<SddStage>`, `stages() -> [(SddStage, StageStatus); 6]`, `is_running() -> bool`, `is_complete() -> bool`, `drifted() -> bool`

- [ ] **Step 1: Écrire les tests qui échouent** — `crates/kaji-core/tests/sdd_test.rs` :

```rust
use kaji_core::sdd::{SddPass, SddStage, SpecDoc, StageStatus};
use std::path::PathBuf;

#[test]
fn parse_extracts_title_from_first_h1() {
    let doc = SpecDoc::parse(PathBuf::from("SPEC.md"), "intro\n# Ma Spec\ncorps");
    assert_eq!(doc.title, "Ma Spec");
    assert!(doc.body.contains("corps"));
}

#[test]
fn parse_falls_back_to_file_stem_without_h1() {
    let doc = SpecDoc::parse(PathBuf::from("demo-spec.md"), "pas de titre ici");
    assert_eq!(doc.title, "demo-spec");
}

#[test]
fn load_missing_file_errors() {
    assert!(SpecDoc::load(std::path::Path::new("/nonexistent/SPEC.md")).is_err());
}

#[test]
fn empty_spec_is_detected() {
    assert!(SpecDoc::parse(PathBuf::from("s.md"), "  \n\t").is_empty());
}

#[test]
fn new_pass_is_all_pending_and_idle() {
    let pass = SddPass::new();
    assert!(pass.current().is_none());
    assert!(!pass.is_running());
    assert!(pass.stages().iter().all(|(_, s)| *s == StageStatus::Pending));
}

#[test]
fn start_puts_intent_running() {
    let mut pass = SddPass::new();
    pass.start();
    assert_eq!(pass.current(), Some(SddStage::Intent));
    assert_eq!(pass.stages()[0], (SddStage::Intent, StageStatus::Running));
}

#[test]
fn advance_walks_all_stages_to_completion() {
    let mut pass = SddPass::new();
    pass.start();
    for _ in 0..6 {
        pass.advance();
    }
    assert!(pass.is_complete());
    assert!(!pass.drifted());
    assert!(pass.current().is_none());
    assert!(pass.stages().iter().all(|(_, s)| *s == StageStatus::Done));
}

#[test]
fn fail_current_stops_the_pass_and_marks_drift() {
    let mut pass = SddPass::new();
    pass.start();
    pass.advance();
    pass.advance();
    assert_eq!(pass.current(), Some(SddStage::Gate));
    pass.fail_current();
    assert!(pass.drifted());
    assert!(!pass.is_running());
    assert_eq!(pass.stages()[2], (SddStage::Gate, StageStatus::Failed));
    pass.advance();
    assert!(pass.current().is_none());
}

#[test]
fn start_twice_is_a_noop_while_running() {
    let mut pass = SddPass::new();
    pass.start();
    pass.advance();
    pass.start();
    assert_eq!(pass.current(), Some(SddStage::Spec));
}
```

- [ ] **Step 2: Vérifier l'échec** — Run: `cargo test -p kaji-core --test sdd_test` → Expected: erreur de compilation « unresolved module `sdd` ».

- [ ] **Step 3: Implémenter** — `crates/kaji-core/src/sdd.rs` :

```rust
//! SDD pass state (ADR 2026-08-07 architecture, ADR 2026-08-08 IPC):
//! pure state owned by the core, clients render and trigger transitions.

use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SddStage {
    Intent,
    Spec,
    Gate,
    Exec,
    Validate,
    DriftLock,
}

impl SddStage {
    pub const ALL: [SddStage; 6] = [
        SddStage::Intent,
        SddStage::Spec,
        SddStage::Gate,
        SddStage::Exec,
        SddStage::Validate,
        SddStage::DriftLock,
    ];

    pub fn label(&self) -> &'static str {
        match self {
            SddStage::Intent => "Intent",
            SddStage::Spec => "SPEC",
            SddStage::Gate => "Gate",
            SddStage::Exec => "Exec",
            SddStage::Validate => "Validate",
            SddStage::DriftLock => "Drift lock",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageStatus {
    Pending,
    Running,
    Done,
    Failed,
}

#[derive(Debug, Clone)]
pub struct SpecDoc {
    pub path: PathBuf,
    pub title: String,
    pub body: String,
}

impl SpecDoc {
    pub fn parse(path: PathBuf, content: &str) -> Self {
        let title = content
            .lines()
            .find_map(|line| line.strip_prefix("# ").map(|t| t.trim().to_string()))
            .unwrap_or_else(|| {
                path.file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "SPEC".to_string())
            });
        Self {
            path,
            title,
            body: content.to_string(),
        }
    }

    pub fn load(path: &Path) -> io::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        Ok(Self::parse(path.to_path_buf(), &content))
    }

    pub fn is_empty(&self) -> bool {
        self.body.trim().is_empty()
    }
}

#[derive(Debug, Clone)]
pub struct SddPass {
    statuses: [StageStatus; 6],
    current: Option<usize>,
}

impl SddPass {
    pub fn new() -> Self {
        Self {
            statuses: [StageStatus::Pending; 6],
            current: None,
        }
    }

    pub fn start(&mut self) {
        if self.current.is_none() && self.statuses.iter().all(|s| *s == StageStatus::Pending) {
            self.current = Some(0);
            self.statuses[0] = StageStatus::Running;
        }
    }

    pub fn advance(&mut self) {
        let Some(idx) = self.current else { return };
        self.statuses[idx] = StageStatus::Done;
        let next = idx + 1;
        if next < self.statuses.len() {
            self.current = Some(next);
            self.statuses[next] = StageStatus::Running;
        } else {
            self.current = None;
        }
    }

    pub fn fail_current(&mut self) {
        if let Some(idx) = self.current {
            self.statuses[idx] = StageStatus::Failed;
            self.current = None;
        }
    }

    pub fn current(&self) -> Option<SddStage> {
        self.current.map(|idx| SddStage::ALL[idx])
    }

    pub fn stages(&self) -> [(SddStage, StageStatus); 6] {
        let mut out = [(SddStage::Intent, StageStatus::Pending); 6];
        for (i, stage) in SddStage::ALL.iter().enumerate() {
            out[i] = (*stage, self.statuses[i]);
        }
        out
    }

    pub fn is_running(&self) -> bool {
        self.current.is_some()
    }

    pub fn is_complete(&self) -> bool {
        self.statuses.iter().all(|s| *s == StageStatus::Done)
    }

    pub fn drifted(&self) -> bool {
        self.statuses.iter().any(|s| *s == StageStatus::Failed)
    }
}

impl Default for SddPass {
    fn default() -> Self {
        Self::new()
    }
}
```

Dans `crates/kaji-core/src/lib.rs`, ajouter `pub mod sdd;` à côté de `pub mod memory;`.

- [ ] **Step 4: Vérifier le vert** — Run: `cargo test -p kaji-core` → Expected: 13 tests memory existants + 9 nouveaux, tous verts (22 total).

- [ ] **Step 5: Commit**

```bash
cargo fmt
git add crates/kaji-core/src/sdd.rs crates/kaji-core/src/lib.rs crates/kaji-core/tests/sdd_test.rs
git commit -m "kaji: module sdd dans kaji-core — état pur de la passe SDD (stages, SpecDoc, SddPass)"
```

---

### Task 2: Dépendance ratatui + bascule de `kaji tui` sur un squelette natif

**Files:**
- Modify: `crates/kaji-cli/Cargo.toml` (dep optionnelle + feature)
- Modify: `crates/kaji-cli/src/cli.rs:1090-1108` (variante `Tui`), `cli.rs:2373-2374` (dispatch)
- Rewrite: `crates/kaji-cli/src/commands/tui.rs` (supprimer intégralement le délégateur npx et ses 2 tests)
- Create: `crates/kaji-cli/src/tui/mod.rs`, `crates/kaji-cli/src/tui/ui.rs`
- Modify: `crates/kaji-cli/src/lib.rs` ou `main.rs` selon où les modules top-level sont déclarés (chercher `mod commands` ; ajouter `#[cfg(feature = "tui")] pub mod tui;` au même endroit)

**Interfaces:**
- Consumes: rien de nouveau.
- Produces: `crate::tui::run(spec: Option<PathBuf>) -> anyhow::Result<()>` (signature provisoire — Task 4 la remplace par la version avec Agent) ; variante clap `Command::Tui { spec: Option<PathBuf> }`.

- [ ] **Step 1: Ajouter ratatui** :

```bash
cargo add ratatui --package kaji-cli --optional
```

Puis dans `crates/kaji-cli/Cargo.toml`, remplacer la ligne `tui = []` (ligne 103) par :

```toml
tui = ["dep:ratatui"]
```

Run: `cargo check -p kaji-cli` → Expected: OK (ratatui compile, feature `tui` active par défaut).

- [ ] **Step 2: Remplacer la variante clap** — dans `cli.rs`, remplacer le bloc lignes 1089-1108 par :

```rust
    /// Launch the kaji terminal UI (TUI)
    #[cfg(feature = "tui")]
    #[command(
        about = "Launch the kaji terminal UI (ratatui, in-process)",
        long_about = "Interface terminal native de kaji : chat streamé sur le Core in-process\n\
                      et panneau SPEC pilotant une passe SDD.\n\
                      \n\
                      --spec <FILE> : fichier SPEC affiché dans le panneau SDD (défaut : ./SPEC.md s'il existe)."
    )]
    Tui {
        /// Fichier SPEC affiché dans le panneau SDD
        #[arg(long, value_name = "FILE")]
        spec: Option<PathBuf>,
    },
```

(`use std::path::PathBuf;` est déjà importé dans `cli.rs` — vérifier, sinon l'ajouter.)
Remplacer le bras de dispatch (`cli.rs:2373-2374`) par :

```rust
        #[cfg(feature = "tui")]
        Some(Command::Tui { spec }) => crate::commands::tui::handle_tui(spec).await,
```

Vérifier `get_command_name()` (`cli.rs:1376-1404`) : si `Command::Tui` y est pattern-matché avec `{ .. }`, rien à changer ; sinon adapter le pattern.

- [ ] **Step 3: Réécrire le handler** — remplacer TOUT le contenu de `crates/kaji-cli/src/commands/tui.rs` par :

```rust
use anyhow::Result;
use std::path::PathBuf;

pub async fn handle_tui(spec: Option<PathBuf>) -> Result<()> {
    crate::tui::run(spec).await
}
```

- [ ] **Step 4: Squelette TUI** — `crates/kaji-cli/src/tui/mod.rs` :

```rust
pub mod ui;

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use std::path::PathBuf;
use std::time::Duration;

pub async fn run(spec: Option<PathBuf>) -> Result<()> {
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, spec);
    ratatui::restore();
    result
}

fn event_loop(terminal: &mut ratatui::DefaultTerminal, _spec: Option<PathBuf>) -> Result<()> {
    loop {
        terminal.draw(ui::draw_placeholder)?;
        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press && key.code == KeyCode::Char('q') {
                    return Ok(());
                }
            }
        }
    }
}
```

`crates/kaji-cli/src/tui/ui.rs` :

```rust
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

pub fn draw_placeholder(frame: &mut Frame) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(frame.area());
    frame.render_widget(
        Paragraph::new("kaji tui — q pour quitter").block(Block::default().borders(Borders::ALL).title(" chat ")),
        cols[0],
    );
    frame.render_widget(
        Block::default().borders(Borders::ALL).title(" SPEC "),
        cols[1],
    );
}
```

Déclarer le module au niveau top de la crate (même fichier que `mod commands` — chercher `grep -rn "mod commands" crates/kaji-cli/src/`) : `#[cfg(feature = "tui")] pub mod tui;`

Si l'API ratatui installée diffère (version plus récente que 0.29 : `ratatui::init`/`DefaultTerminal`/`frame.area()` sont l'API stable depuis 0.28-0.29), consulter la doc de la version résolue dans `Cargo.lock` et adapter — ne pas downgrader la dep.

- [ ] **Step 5: Vérifier** — Run: `cargo clippy -p kaji-cli --all-targets -- -D warnings && cargo test -p kaji-cli` → Expected: clippy clean ; les 2 anciens tests npx ont disparu, le reste des 258 tests kaji-cli vert. Puis smoke test manuel : `cargo build -p kaji-cli --bin kaji && ./target/debug/kaji tui` → l'écran alternatif s'ouvre avec 2 panneaux, `q` restaure le terminal proprement.

- [ ] **Step 6: Commit**

```bash
cargo fmt
git add -A crates/kaji-cli
git commit -m "kaji: kaji tui natif ratatui — squelette in-process, suppression du délégateur npx déprécié"
```

---

### Task 3: État `App` pur — saisie, chat, application des AgentEvent

**Files:**
- Create: `crates/kaji-cli/src/tui/app.rs`
- Modify: `crates/kaji-cli/src/tui/mod.rs` (ajouter `pub mod app;`)

**Interfaces:**
- Consumes: `kaji::agents::AgentEvent`, `kaji_core::sdd::{SddPass, SpecDoc}` (Task 1). kaji-cli dépend déjà de `kaji` ; ajouter la dep chemin `kaji-core` à kaji-cli si absente : `cargo add kaji-core --package kaji-cli --path crates/kaji-core` (vérifier d'abord `grep kaji-core crates/kaji-cli/Cargo.toml`).
- Produces (consommé par Task 4/5) :
  - `struct App` avec champs publics `input: String`, `chat: Vec<ChatLine>`, `status: String`, `turn_active: bool`, `spec: Option<SpecDoc>`, `pass: SddPass`, `gate_open: bool`
  - `App::new(spec: Option<SpecDoc>) -> App`
  - `App::on_event(&mut self, ev: &Event) -> Action` avec `enum Action { None, Submit(String), CancelTurn, Quit, StartPass, GateApprove, GateReject }`
  - `App::push_user(&mut self, text: &str)`, `App::push_system(&mut self, text: &str)`
  - `App::apply_agent_event(&mut self, ev: &AgentEvent)`
  - `struct ChatLine { pub sender: Sender, pub text: String }`, `enum Sender { User, Agent, System }`

- [ ] **Step 1: Tests d'abord** — module `#[cfg(test)] mod tests` en bas de `app.rs` (pattern `commands/memory.rs`) :

```rust
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
        let agent_lines: Vec<_> = app.chat.iter().filter(|l| matches!(l.sender, Sender::Agent)).collect();
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
```

⚠ Ajustements autorisés (Règle 14 côté implémenteur) : les chemins d'import exacts (`kaji::conversation::message::Message` vs ré-export, existence de `Message::assistant()`, mutabilité de `message.id`, `Conversation::default()`) doivent être vérifiés dans `crates/kaji-provider-types/src/conversation/message.rs` et les ré-exports de `crates/kaji/src/lib.rs` — adapter les imports/constructeurs des tests à ce qui existe (le CLI en construit déjà : chercher `Message::assistant()` dans `crates/kaji-cli/`). Le comportement testé, lui, ne change pas.

- [ ] **Step 2: Vérifier l'échec** — Run: `cargo test -p kaji-cli tui::app` → Expected: compilation échoue (module absent).

- [ ] **Step 3: Implémenter `App`** — `crates/kaji-cli/src/tui/app.rs` :

```rust
use kaji::agents::AgentEvent;
use kaji_core::sdd::{SddPass, SpecDoc};
use ratatui::crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};

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
        let Event::Key(key) = ev else { return Action::None };
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
}
```

`apply_message` (même fichier) : itérer `message.content` ; pour un bloc texte, si `message.id` (cloné en `Option<String>`) égale `self.last_agent_msg_id` → append au dernier `ChatLine` `Agent`, sinon nouvelle ligne `Agent` + mémoriser l'id ; pour un bloc tool-request → `push_system("⚙ appel d'outil")` (si le nom de l'outil est trivialement accessible sur le type — vérifier `ToolRequest` dans `crates/kaji-provider-types/src/conversation/message.rs` — l'inclure : `⚙ {name}`) ; pour un bloc tool-response → `push_system("✓ outil terminé")` ; ignorer les autres blocs. Ne traiter que les messages dont le rôle est assistant (comparer comme le fait `crates/kaji-cli/src/session/output.rs` — chercher `role` dans ce fichier et répliquer la comparaison exacte).

- [ ] **Step 4: Vérifier le vert** — Run: `cargo test -p kaji-cli tui::app` → Expected: 7 tests verts.

- [ ] **Step 5: Commit**

```bash
cargo fmt
git add crates/kaji-cli/src/tui/ crates/kaji-cli/Cargo.toml Cargo.lock
git commit -m "kaji: tui — état App pur (saisie, chat, fusion des chunks AgentEvent par message id)"
```

---

### Task 4: Câblage session réelle — chat streamé in-process

**Files:**
- Modify: `crates/kaji-cli/src/session/mod.rs` (ajouter `into_parts` dans `impl CliSession`, après `session_id()` ligne ~335)
- Modify: `crates/kaji-cli/src/cli.rs:394` (visibilité `get_or_create_session_id`)
- Modify: `crates/kaji-cli/src/commands/tui.rs` (handler complet)
- Modify: `crates/kaji-cli/src/tui/mod.rs` (boucle async réelle), `crates/kaji-cli/src/tui/ui.rs` (rendu chat/input/statut)

**Interfaces:**
- Consumes: `App`/`Action` (Task 3), `build_session`/`SessionBuilderConfig` (`crate::session::builder`), `SessionConfig` (`kaji::agents`), `CancellationToken` (`tokio_util::sync`), `futures::StreamExt`.
- Produces:
  - `CliSession::into_parts(self) -> (Agent, String, Conversation)`
  - `crate::tui::run(agent: Agent, session_id: String, conversation: Conversation, spec: Option<PathBuf>) -> Result<()>` (remplace la signature provisoire de Task 2)

- [ ] **Step 1: Accesseur** — dans `impl CliSession` (`session/mod.rs`, après `session_id()`) :

```rust
    pub fn into_parts(self) -> (Agent, String, Conversation) {
        (self.agent, self.session_id, self.messages)
    }
```

Et dans `cli.rs:394`, préfixer `get_or_create_session_id` de `pub(crate)` si ce n'est pas déjà le cas.

- [ ] **Step 2: Handler complet** — `crates/kaji-cli/src/commands/tui.rs` :

```rust
use anyhow::Result;
use std::path::PathBuf;

use crate::session::builder::{build_session, SessionBuilderConfig};
use kaji::config::Config;

pub async fn handle_tui(spec: Option<PathBuf>) -> Result<()> {
    let kaji_mode = Config::global().get_kaji_mode().unwrap_or_default();
    let session_id = crate::cli::get_or_create_session_id(None, false, false, kaji_mode).await?;
    let session = build_session(SessionBuilderConfig {
        session_id,
        interactive: true,
        ..Default::default()
    })
    .await;
    let (agent, session_id, conversation) = session.into_parts();
    crate::tui::run(agent, session_id, conversation, spec).await
}
```

(Adapter les chemins d'import à ce qui existe : `Config` est utilisé dans `cli.rs` — copier son `use` exact ; idem pour le module réel de `builder` : `grep -n "pub mod builder" crates/kaji-cli/src/session/mod.rs`. `build_session` imprime éventuellement pendant la construction — c'est voulu : elle s'exécute AVANT `ratatui::init()`, le terminal est encore normal.)

- [ ] **Step 3: Boucle async réelle** — remplacer `crates/kaji-cli/src/tui/mod.rs` :

```rust
pub mod app;
pub mod ui;

use anyhow::Result;
use app::{Action, App};
use futures::stream::BoxStream;
use futures::StreamExt;
use kaji::agents::{Agent, AgentEvent, SessionConfig};
use kaji::conversation::message::Message;
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
    let (input_tx, input_rx) = mpsc::channel::<Event>(64);
    std::thread::spawn(move || input_thread(input_tx));
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, &agent, &session_id, conversation, spec_path, input_rx).await;
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
    let mut cancel = CancellationToken::new();

    loop {
        terminal.draw(|frame| ui::draw(frame, &app))?;
        tokio::select! {
            ev = input_rx.recv() => {
                let Some(ev) = ev else { break };
                match app.on_event(&ev) {
                    Action::Quit => break,
                    Action::CancelTurn => cancel.cancel(),
                    Action::Submit(text) => {
                        app.push_user(&text);
                        cancel = CancellationToken::new();
                        match start_turn(agent, &session_config, &text, &cancel).await {
                            Ok(stream) => {
                                app.turn_active = true;
                                turn = Some(stream);
                            }
                            Err(e) => app.push_system(&format!("erreur: {e}")),
                        }
                    }
                    Action::StartPass | Action::GateApprove | Action::GateReject => {
                        app.push_system("passe SDD : câblée à la tâche 5");
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
                        app.turn_active = false;
                    }
                    None => {
                        turn = None;
                        app.turn_active = false;
                    }
                }
            }
        }
    }
    Ok(())
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
```

Points de vigilance (vérifier, ne pas supposer) : le type d'erreur du stream de `Agent::reply` (`agent.rs:1811` — si c'est `kaji::Result`/autre alias, adapter `TurnStream`) ; `Conversation::messages()` (utilisé par le pont mémoire, existe) ; `seed_chat` n'affichera que les messages assistant — compléter `apply_message` (Task 3) ou `seed_chat` pour rendre aussi les messages user en `ChatLine::User` si le rôle est user.

- [ ] **Step 4: Rendu réel** — `ui.rs` : remplacer `draw_placeholder` par `pub fn draw(frame: &mut Frame, app: &App)` : colonne gauche = `Paragraph` du chat (préfixes `vous ▸ ` / `kaji ▸ ` / `· `, `Wrap { trim: false }`, scroll collé en bas : calculer l'offset depuis le nombre de lignes wrappées vs hauteur), 3 lignes du bas = input avec curseur (`frame.set_cursor_position`) et bordure titrée `statut` (« ⏳ tour en cours (Esc annule) » si `turn_active`) ; colonne droite = panneau SPEC (titre du SpecDoc ou « aucune SPEC ») + les 6 étages `pass.stages()` avec symboles `·`/`▶`/`✓`/`✗`. Garder ce rendu SANS test unitaire (rendu = vérification manuelle).

- [ ] **Step 5: Vérifier** — Run: `cargo clippy -p kaji-cli --all-targets -- -D warnings && cargo test -p kaji-cli` → Expected: clean + tests verts. Puis E2E manuel avec le setup prouvé du repo (ollama local) :

```bash
cargo build -p kaji-cli --bin kaji
KAJI_PROVIDER=ollama KAJI_MODEL=qwen3.5:4b ./target/debug/kaji tui
```

Expected: taper un message + Enter → la réponse streame dans le panneau chat ; Esc pendant le tour l'annule ; Ctrl+C quitte proprement (terminal restauré). Refaire un run avec `KAJI_STATE_MACHINE=1` → comportement identique (le dispatch est interne à `Agent::reply`).

- [ ] **Step 6: Commit**

```bash
cargo fmt
git add -A crates/kaji-cli
git commit -m "kaji: tui — chat streamé in-process sur Agent::reply (session réelle, annulation Esc)"
```

---

### Task 5: Panneau SPEC actif — passe SDD de bout en bout

**Files:**
- Modify: `crates/kaji-cli/src/tui/app.rs` (driver de passe + tests), `crates/kaji-cli/src/tui/mod.rs` (câblage des actions), `crates/kaji-cli/src/tui/ui.rs` (modale gate)

**Interfaces:**
- Consumes: `SddPass`/`SpecDoc` (Task 1), boucle Task 4.
- Produces (sur `App`) :
  - `enum PassDriver { Idle, AwaitingGate, Executing, Validating }` + champ `pub driver: PassDriver`
  - `App::start_pass(&mut self)` — avance Intent+Spec, ouvre la gate (`gate_open = true`, `driver = AwaitingGate`) ; erreur système si pas de spec, spec vide, ou passe déjà active
  - `App::gate_approve(&mut self) -> Option<String>` — ferme la gate, avance vers Exec, retourne le prompt d'exécution
  - `App::gate_reject(&mut self)` — `fail_current` (Gate ✗), retour Idle
  - `App::turn_end(&mut self) -> Option<String>` — appelée par la boucle quand un stream se termine : `Executing` → avance vers Validate et retourne le prompt de validation ; `Validating` → lit le verdict accumulé et clôt (VALIDE → Validate ✓ + Drift lock ✓ ; DRIFT → Drift lock ✗) ; `Idle` → None

- [ ] **Step 1: Tests d'abord** (ajouter au mod tests de `app.rs`) :

```rust
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
```

- [ ] **Step 2: Vérifier l'échec** — Run: `cargo test -p kaji-cli tui::app` → Expected: échec de compilation (`start_pass` absent).

- [ ] **Step 3: Implémenter le driver** dans `app.rs` :

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PassDriver {
    Idle,
    AwaitingGate,
    Executing,
    Validating,
}
```

- Champs à ajouter sur `App` : `pub driver: PassDriver` (init `Idle`), `validate_buffer: String` (privé).
- `start_pass` : garde (spec présente et non vide via `is_empty()`, `!self.pass.is_running()`, sinon `push_system` explicite) ; `pass.start()` [Intent ▶] ; `push_system(&format!("Intent : {}", titre))` ; `pass.advance()` [SPEC ▶] ; `pass.advance()` [Gate ▶] ; `gate_open = true` ; `driver = AwaitingGate`.
- `gate_approve` : `pass.advance()` [Exec ▶] ; `driver = Executing` ; retourne `Some(format!("Exécute la SPEC suivante. Réponds directement, sans sortir du périmètre.\n\n{}", spec.body))`.
- `gate_reject` : `pass.fail_current()` ; `driver = Idle` ; `push_system("gate refusée — passe interrompue")`.
- `turn_end` : selon `driver` — `Executing` → `pass.advance()` [Validate ▶], `driver = Validating`, `validate_buffer.clear()`, retourne `Some(format!("Vérifie que ta réponse précédente respecte la SPEC ci-dessous. Première ligne : exactement `VERDICT: VALIDE` ou `VERDICT: DRIFT`, puis justifie en une phrase.\n\n{}", spec.body))` ; `Validating` → `pass.advance()` [Drift lock ▶] puis si `validate_buffer.to_uppercase().contains("VERDICT: DRIFT")` → `pass.fail_current()` + `push_system("⚠ drift détecté — spec non verrouillée")` sinon `pass.advance()` + `push_system("✓ passe SDD complète — spec verrouillée")` ; `driver = Idle` ; `None` ; autres → `None`. Toujours `turn_active = false` en tête.
- Dans `apply_message` : quand `driver == Validating`, accumuler le texte assistant dans `validate_buffer` en plus du chat.

- [ ] **Step 4: Câbler la boucle** (`tui/mod.rs`) — remplacer le bras placeholder de Task 4 :

```rust
                    Action::StartPass => app.start_pass(),
                    Action::GateApprove => {
                        if let Some(prompt) = app.gate_approve() {
                            app.push_system("Exec : envoi de la SPEC à l'agent");
                            cancel = CancellationToken::new();
                            match start_turn(agent, &session_config, &prompt, &cancel).await {
                                Ok(stream) => {
                                    app.turn_active = true;
                                    turn = Some(stream);
                                }
                                Err(e) => app.push_system(&format!("erreur: {e}")),
                            }
                        }
                    }
                    Action::GateReject => app.gate_reject(),
```

et dans le bras `None =>` (fin de stream) : après `turn = None;`, remplacer `app.turn_active = false;` par :

```rust
                        if let Some(prompt) = app.turn_end() {
                            cancel = CancellationToken::new();
                            match start_turn(agent, &session_config, &prompt, &cancel).await {
                                Ok(stream) => {
                                    app.turn_active = true;
                                    turn = Some(stream);
                                }
                                Err(e) => app.push_system(&format!("erreur: {e}")),
                            }
                        }
```

(Factoriser l'envoi de tour en une closure/fn locale si le borrow checker le permet proprement — sinon la duplication ×3 est acceptée pour le MVP.)
Dans `ui.rs` : si `app.gate_open`, rendre une modale centrée par-dessus (« Gate — approuver la SPEC ? (y/n) », `Clear` widget + `Block` bordé).

- [ ] **Step 5: Vérifier** — Run: `cargo clippy -p kaji-cli --all-targets -- -D warnings && cargo test -p kaji-cli` → Expected: clean, tests Task 3 + 4 nouveaux verts. E2E manuel : créer un `SPEC.md` de démo (« # Démo\nRéponds avec un haïku sur les renards. ») puis :

```bash
KAJI_PROVIDER=ollama KAJI_MODEL=qwen3.5:4b ./target/debug/kaji tui --spec SPEC.md
```

`/sdd` + Enter → Intent/SPEC passent ✓, modale gate s'affiche, `y` → l'exec streame, puis le tour de validation streame, et le panneau finit soit tout ✓ (« spec verrouillée ») soit Drift lock ✗.

- [ ] **Step 6: Commit**

```bash
cargo fmt
git add -A crates/kaji-cli
git commit -m "kaji: tui — passe SDD bout-en-bout (gate humaine, exec + validation LLM, drift lock)"
```

---

### Task 6: Gates finaux du vertical slice

**Files:**
- Modify: rien de nouveau — vérifications + éventuels fixes clippy/fmt résiduels.

- [ ] **Step 1: Suite complète** — Run:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test -p kaji-core && cargo test -p kaji-cli
cargo test -p kaji --lib 2>&1 | tail -5
```

Expected: fmt/clippy clean ; kaji-core 22 verts ; kaji-cli ≥ 269 verts (258 - 2 npx + ~13 nouveaux) ; `kaji --lib` : mêmes 8 échecs préexistants, pas un de plus.

- [ ] **Step 2: Build binaire de distribution** — Run: `cargo build -p kaji-cli --bin kaji` (le rebuild du binaire NE se fait PAS via `-p kaji` — piège documenté). Ne jamais `cp` sur un binaire en cours d'exécution.

- [ ] **Step 3: Démo complète scriptée** — dérouler manuellement et cocher : lancement `kaji tui`, chat simple (mémoire inter-session incluse : demander « de quoi on a parlé la dernière fois ? » après une session antérieure), `/sdd` happy path, `/sdd` avec `n` à la gate, Esc pendant un tour, Ctrl+C. Consigner le résultat dans le message de commit final ou le rapport de fin.

- [ ] **Step 4: Note self-test** — `kaji-self-test.yaml` (règle AGENTS.md) : la TUI plein-écran n'est pas exerçable par recipe headless ; ne pas modifier le yaml, le noter dans le rapport final.

- [ ] **Step 4bis: Installer `kaji` sur le PATH** (exigence user : `kaji` doit se lancer directement, sans cargo build) — Run:

```bash
cargo build --release -p kaji-cli --bin kaji
rm -f ~/.local/bin/kaji
cp -p target/release/kaji ~/.local/bin/kaji
kaji --version
```

Expected: `kaji --version` répond depuis le PATH. Le `rm -f` avant `cp` est obligatoire (règle AGENTS.md : jamais écraser un binaire vivant en place — SIGKILL Code Signature macOS). Build release : ~5-10 min.

- [ ] **Step 5: Commit final (si des fixes ont eu lieu)**

```bash
git add -A
git commit -m "kaji: tui — gates finaux du vertical slice (fmt, clippy, suites vertes)"
```
