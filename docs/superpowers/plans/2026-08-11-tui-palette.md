# TUI Palette de commandes « / » — Implementation Plan (T5 UX v3)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Palette lazyvim-like ancrée au-dessus de l'input : `/` ouvre, filtre préfixe live, ↑/↓ cyclique, Enter exécute la sélection, Tab complète, Esc annule — spec `docs/superpowers/specs/2026-08-11-tui-palette-design.md`.

**Architecture:** Table unique `COMMANDS` dans app.rs (nom/desc/action) dont dérivent dispatch, /help et palette. État palette minimal dans `App` (`palette_selected` + méthodes dérivées de `input`). Rendu overlay dans ui.rs ancré au Rect de l'input, après le chat — zéro impact sur la mesure scroll.

**Tech Stack:** ratatui 0.30 (Clear, Block Rounded, title_bottom), pas de nouvelle dépendance.

## Global Constraints

- Base : HEAD **post-T4** (curseur ninja commité). Relever la baseline verte `cargo test -p kaji-cli` AVANT de commencer et la reporter.
- `source bin/activate-hermit` ; cargo foreground, un seul à la fois, jamais background.
- TDD strict par étape ; fin de chaque tâche : `cargo fmt`, `cargo clippy -p kaji-cli --all-targets -- -D warnings`, suite kaji-cli verte.
- Un commit par tâche, message français `kaji: tui — …` + trailer `Claude-Session: https://claude.ai/code/session_014ngoE4sNSgzrZPdgb7qC2r`.
- Les numéros de ligne cités datent d'avant T4 — se repérer aux noms de symboles, pas aux numéros.
- Spec = source de vérité en cas de doute.

---

### Task 5a: Table `COMMANDS` unique (dispatch + /help dérivés)

**Files:**
- Modify: `crates/kaji-cli/src/tui/app.rs` (table + dispatch du Enter)
- Modify: `crates/kaji-cli/src/tui/mod.rs` (`push_welcome` dérive la liste des commandes)

**Interfaces:**
- Produces: `pub struct Command { pub name: &'static str, pub desc: &'static str, run: fn(&mut App) -> Action }` ; `pub const COMMANDS: &[Command]` (ordre d'affichage palette) ; `impl Command { pub fn run(&self, app: &mut App) -> Action }`. Tasks 5b/5c consomment `COMMANDS`, `Command::name/desc/run`.

- [ ] **Step 1 : tests RED** — dans `mod tests` de app.rs :

```rust
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
```

Dans `mod tests` de mod.rs (helper `welcome_text` existant) :

```rust
#[test]
fn push_welcome_lists_every_command_from_the_table() {
    let mut app = App::new(None);
    push_welcome(&mut app);
    let text = welcome_text(&app);
    for cmd in crate::tui::app::COMMANDS {
        assert!(text.contains(cmd.name), "{} absent du welcome/help", cmd.name);
    }
}
```

- [ ] **Step 2 : vérifier le RED** — `cargo test -p kaji-cli every_command_in_the_table` → FAIL (COMMANDS inexistant = erreur de compil, ça compte comme RED).
- [ ] **Step 3 : implémentation** — dans app.rs, au-dessus de `pub struct App` :

```rust
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
    Command { name: "/sdd", desc: "lance une passe SDD", run: |_| Action::StartPass },
    Command { name: "/spec", desc: "panneau spec on/off", run: |app| { app.toggle_spec_panel(); Action::None } },
    Command { name: "/think", desc: "affiche/masque le thinking", run: |app| { app.toggle_thinking(); Action::None } },
    Command { name: "/cost", desc: "usage et coût de la session", run: |_| Action::Cost },
    Command { name: "/docker", desc: "état des conteneurs", run: |_| Action::Docker },
    Command { name: "/help", desc: "commandes et raccourcis", run: |_| Action::Help },
    Command { name: "/quit", desc: "quitte kaji", run: |_| Action::Quit },
];
```

(Reprendre les descs de l'existant si /help en a déjà — ne pas inventer de sémantique. Si le compilateur refuse la coercion closure→fn dans le const, extraire des `fn` nommées privées.)

Remplacer la chaîne if/else du Enter (branche non-vide, après `self.push_history(&text)`) par :

```rust
if let Some(cmd) = COMMANDS.iter().find(|c| c.name == text) {
    cmd.run(self)
} else {
    Action::Submit(text)
}
```

Dans mod.rs `push_welcome` : remplacer toute ligne hardcodée listant des commandes par une dérivation :

```rust
for cmd in crate::tui::app::COMMANDS {
    app.push_system(&format!("{}  {}", cmd.name, cmd.desc));
}
```

(La placer là où les commandes étaient déjà listées ; garder les lignes raccourcis clavier telles quelles.)

- [ ] **Step 4 : GREEN ciblé puis suite complète** — `cargo test -p kaji-cli` : tous les tests slash existants (`slash_sdd_submits_start_pass`, `slash_help_returns_help_action`, `slash_cost…`, `slash_docker…`, `slash_think…`) doivent rester verts SANS modification — c'est la preuve que le refactor est iso-comportement.
- [ ] **Step 5 : fmt + clippy + commit** — `kaji: tui — table COMMANDS unique (dispatch et /help dérivés)`.

### Task 5b: État palette + clavier

**Files:**
- Modify: `crates/kaji-cli/src/tui/app.rs`

**Interfaces:**
- Consumes: `COMMANDS`, `Command::run` (Task 5a).
- Produces: `App::palette_visible(&self) -> bool` ; `App::palette_matches(&self) -> Vec<&'static Command>` ; `pub palette_selected: usize` ; `App::modal_active(&self) -> bool` (extraction du `let modal_active = …` existant du fix round T3). Task 5c consomme les trois premiers.

- [ ] **Step 1 : tests RED** (un run pour les voir tous échouer) :

```rust
#[test]
fn typing_slash_opens_the_palette_and_filters_by_prefix() {
    let mut app = App::new(None);
    app.on_event(&key(KeyCode::Char('/')));
    assert!(app.palette_visible());
    assert_eq!(app.palette_matches().len(), COMMANDS.len());
    app.on_event(&key(KeyCode::Char('s')));
    let names: Vec<_> = app.palette_matches().iter().map(|c| c.name).collect();
    assert_eq!(names, vec!["/sdd", "/spec"]);
    assert!(!App::new(None).palette_visible(), "input vide → pas de palette");
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
    assert_eq!(app.palette_selected, 0, "l'édition resélectionne le premier");
}

#[test]
fn palette_enter_runs_the_selected_command_not_the_typed_text() {
    let mut app = App::new(None);
    for c in "/th".chars() {
        app.on_event(&key(KeyCode::Char(c)));
    }
    assert_eq!(app.palette_matches()[0].name, "/think");
    let action = app.on_event(&key(KeyCode::Enter));
    assert_eq!(action, Action::None);
    assert!(app.show_thinking, "la sélection /think a bien été exécutée");
    assert!(app.input.is_empty());
    app.on_event(&key(KeyCode::Up));
    assert_eq!(app.input, "/think", "la commande exécutée entre dans l'historique");
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
    assert_eq!(app.input, "/", "↑ navigue la palette, ne rappelle pas l'historique");
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
```

(`app.turn_active` / `app.show_thinking` / `app.driver` : si un champ n'est pas accessible ainsi depuis les tests, utiliser le setter/l'état réellement exposé — regarder comment les tests existants du fichier posent `turn_active` et lisent le toggle thinking, et copier cet idiome.)

- [ ] **Step 2 : vérifier le RED** — `cargo test -p kaji-cli palette_` → tout FAIL.
- [ ] **Step 3 : implémentation** — dans App :

```rust
pub palette_selected: usize,   // + init 0 dans App::new

pub fn modal_active(&self) -> bool {
    self.tool_approval.is_some() || self.gate_open
}

pub fn palette_matches(&self) -> Vec<&'static Command> {
    if !self.input.starts_with('/') {
        return Vec::new();
    }
    COMMANDS.iter().filter(|c| c.name.starts_with(&self.input)).collect()
}

pub fn palette_visible(&self) -> bool {
    !self.modal_active() && !self.palette_matches().is_empty()
}

fn reset_palette_selection(&mut self) {
    self.palette_selected = 0;
}
```

Remplacer le `let modal_active = …` du fix round par `let modal_active = self.modal_active();`. Appeler `reset_palette_selection` partout où `input` change : arms `Char`/`Backspace`, `delete_last_word`, `history_prev`, `history_next`, et les chemins Enter/Esc/Tab de la palette.

Dans `on_event`, arms plain `KeyCode::Up`/`Down` existants — la palette passe en tête du if :

```rust
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
```

(`Down` symétrique avec `(self.palette_selected + 1) % n`.) Nouvel arm Tab (près de Enter) :

```rust
KeyCode::Tab if self.palette_visible() => {
    let name = self.palette_matches()[self.palette_selected.min(self.palette_matches().len() - 1)].name;
    self.input = name.to_string();
    self.exit_history_navigation();
    self.reset_palette_selection();
    Action::None
}
```

Nouvel arm Esc AVANT l'arm `KeyCode::Esc if self.turn_active || self.turn_pending` existant :

```rust
KeyCode::Esc if self.palette_visible() => {
    self.input.clear();
    self.reset_palette_selection();
    Action::None
}
```

Début de l'arm Enter (avant le `std::mem::take`) :

```rust
if self.palette_visible() {
    let cmd = self.palette_matches()[self.palette_selected.min(self.palette_matches().len() - 1)];
    self.input.clear();
    self.reset_palette_selection();
    self.push_history(cmd.name);
    return cmd.run(self);
}
```

Garde-fou index : `palette_selected` peut pointer au-delà après un filtre réduit si un chemin d'édition a raté le reset — le `.min(len-1)` aux points de consommation rend ça inoffensif ; le test `reset_on_edit` verrouille le comportement nominal.

- [ ] **Step 4 : GREEN complet** — `cargo test -p kaji-cli` : nouveaux verts + AUCUNE régression (historique, scroll, modals, CancelTurn).
- [ ] **Step 5 : fmt + clippy + commit** — `kaji: tui — palette de commandes : état, filtre préfixe et navigation clavier`.

### Task 5c: Rendu de la palette

**Files:**
- Modify: `crates/kaji-cli/src/tui/ui.rs` (`draw_palette` + appels dans les deux branches de `draw`)
- Modify: `crates/kaji-cli/src/tui/theme.rs` (uniquement si un style manque — VERMILLON existe après T4)

**Interfaces:**
- Consumes: `App::palette_visible/palette_matches/palette_selected` (5b), `theme::VERMILLON`.
- Produces: `fn draw_palette(frame: &mut Frame, app: &App, input_area: Rect)` — rien d'autre n'en dépend.

- [ ] **Step 1 : tests RED** — dans `mod tests` de ui.rs (idiome TestBackend existant) :

```rust
#[test]
fn palette_renders_filtered_commands_above_the_input() {
    let mut app = App::new(None);
    app.on_event(&Event::Key(/* '/' puis 's' — reprendre le helper key() de app::tests ou construire les KeyEvent localement comme les autres tests ui */));
    // …frappe "/s" via on_event…
    let backend = TestBackend::new(80, 14);
    let mut terminal = Terminal::new(backend).expect("test backend terminal");
    terminal.draw(|f| draw(f, &app)).expect("draw");
    let content = buffer_as_string(terminal.backend().buffer());
    assert!(content.contains("commandes"));
    assert!(content.contains("/sdd"));
    assert!(content.contains("/spec"));
    assert!(!content.contains("/quit"), "filtré hors de la liste");
    assert!(content.contains("▸"), "marqueur de sélection visible");
}

#[test]
fn palette_is_absent_without_slash_input_and_without_matches() {
    for input in ["", "hello", "/xyz"] {
        let mut app = App::new(None);
        for c in input.chars() { /* …frappe via on_event… */ }
        let backend = TestBackend::new(80, 14);
        let mut terminal = Terminal::new(backend).expect("test backend terminal");
        terminal.draw(|f| draw(f, &app)).expect("draw");
        let content = buffer_as_string(terminal.backend().buffer());
        assert!(!content.contains("commandes"), "input {input:?} ne doit pas ouvrir la palette");
    }
}
```

(Écrire le petit helper `buffer_as_string(buffer) -> String` s'il n'existe pas : concat des symbols par rangée. Les commentaires `/* …frappe… */` sont à remplacer par la vraie séquence `app.on_event(&Event::Key(KeyEvent{…}))` — même construction que les tests app.rs.)

- [ ] **Step 2 : vérifier le RED** — `cargo test -p kaji-cli palette_renders` → FAIL.
- [ ] **Step 3 : implémentation** :

```rust
fn draw_palette(frame: &mut Frame, app: &App, input_area: Rect) {
    if !app.palette_visible() {
        return;
    }
    let matches = app.palette_matches();
    let name_w = matches.iter().map(|c| c.name.len()).max().unwrap_or(0);
    let inner_w = matches
        .iter()
        .map(|c| name_w + 2 + c.desc.chars().count() + 4)
        .max()
        .unwrap_or(0) as u16;
    let width = (inner_w + 2).min(input_area.width.saturating_sub(2)).max(20);
    let height = (matches.len() as u16 + 2).min(input_area.y);
    if height < 3 {
        return; // pas la place d'afficher ne serait-ce qu'un item bordé
    }
    let rows = (height - 2) as usize;
    // fenêtre glissante : la sélection reste visible quand la liste est tronquée
    let first = app.palette_selected.saturating_sub(rows.saturating_sub(1));
    let area = Rect {
        x: input_area.x + 1,
        y: input_area.y - height,
        width,
        height,
    };
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(" commandes ")
        .title_bottom(Line::from(" ↑↓ choisir · ⏎ valider · esc ").style(theme::DIM));
    let lines: Vec<Line> = matches
        .iter()
        .enumerate()
        .skip(first)
        .take(rows)
        .map(|(i, cmd)| {
            let selected = i == app.palette_selected;
            let marker = if selected { "▸ " } else { "  " };
            let name_style = if selected { theme::VERMILLON_STYLE } else { Style::default() };
            Line::from(vec![
                Span::styled(marker.to_string(), theme::VERMILLON_STYLE),
                Span::styled(format!("{:<name_w$}", cmd.name), name_style),
                Span::styled(format!("  {}", cmd.desc), theme::DIM),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(Text::from(lines)).block(block), area);
}
```

(`theme::DIM` / `theme::VERMILLON_STYLE` : utiliser les noms RÉELS de theme.rs post-T4 — les lire d'abord ; si seul une couleur `VERMILLON` existe, construire `Style::default().fg(theme::VERMILLON)`. Ne pas créer de doublon de style.)

Appeler `draw_palette(frame, app, left[1]);` juste après `draw_input(frame, app, left[1]);` dans **les deux** branches de `draw`.

- [ ] **Step 4 : GREEN complet** — `cargo test -p kaji-cli` ; vérifier notamment que les tests d'offsets exacts (`draw_chat_records_a_row_offset_for_every_user_turn`) restent verts : la palette ne doit rien changer à la mesure.
- [ ] **Step 5 : fmt + clippy + commit** — `kaji: tui — palette de commandes : rendu ancré à l'input (sélection vermillon, footer hint)`.

---

## Self-review (à la rédaction)

Spec↔tasks : cycle de vie (ouverture `/`, Esc annule sans CancelTurn, fermeture sans match) → 5b ✓ ; interaction (préfixe, cyclique, Enter sélection, Tab, no-match) → 5b ✓ ; table unique + exhaustivité dispatch/help → 5a ✓ ; rendu (arrondi, ▸ vermillon, desc dim, footer, clamp hauteur + fenêtre sur sélection, pas de boîte vide) → 5c ✓ ; priorités modal/historique/Ctrl+↑↓ → 5b ✓ ; non-objectifs absents ✓. Types cohérents 5a→5b→5c (`COMMANDS`, `Command::name/desc/run`, `palette_matches() -> Vec<&'static Command>`) ✓.
