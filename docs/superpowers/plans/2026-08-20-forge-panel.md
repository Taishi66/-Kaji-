# Volet 炉 forge — « qui fait quoi » des subagents — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

> Contrôleur : Fable (planifie, review, rulings). Exécuteur : Opus. Décisions user 2026-08-20 : volet latéral, Enter = fiche détail, auto-forge (ouverture/repli auto), double alimentation snapshot + notifications.
>
> **État 2026-08-21 : plan terminé.** T1-T4 livrées et reviewées (commits `f9add4df1`..`50e2aa810`, 7 commits), review finale de branche « With fixes » puis vague corrigée et re-reviewée propre (spinner sur l'horloge de la tâche, garde e/a/r sur la fiche, titre sans newline, refresh sur notification, fiche suivie par id, troncature en cellules). 903 tests kaji-cli + 1909 kaji verts, clippy propre. Suivi ouvert : unifier les troncatures chars→cellules (goal_badge, truncate_tool_name, truncate_for_modal, forge_sheet_title, wrap_words) — brouillon d'issue prêt, PAT local sans scope createIssue.

**Goal:** Un volet latéral droit qui montre en temps réel l'agent principal et chaque subagent (statut, outil en cours, durée), s'ouvre seul quand un subagent démarre, se replie 5 s après la fin du dernier, et permet fiche détail (Enter) et annulation (x).

**Architecture:** Trois étages. Core : deux méthodes par défaut sur le trait `McpClientTrait` (`subagent_snapshot`, `cancel_subagent` — pattern `get_moim`, `mcp_client.rs:152-158`) overridées par `SummonClient`, exposées via `ExtensionManager` puis `Agent`. État TUI : module `forge.rs` (réconciliation deux sources par `subagent_id` : snapshot autoritaire sur le statut, notification `subagent_tool_request` sur l'outil courant). Rendu TUI : volet colonne droite gabarit explorateur, `Focus::Forge`, fiche dans le lecteur existant, `遣 N` en barre d'état.

**Tech Stack:** Rust 2021, ratatui 0.30, rmcp 3.0.0, tokio, tokio-util (`CancellationToken`).

**Spec:** `docs/superpowers/specs/2026-08-20-forge-panel-design.md`

## Global Constraints (valables pour toutes les Tasks)

- **cargo TOUJOURS foreground**, `timeout: 600000` explicite sur CHAQUE commande cargo, JAMAIS `run_in_background`, JAMAIS `&`. Un seul cargo à la fois. Si l'outil Bash bascule quand même en background au timeout : boucler en petits appels foreground `ps aux | grep -cE "[c]argo|[r]ustc"` jusqu'à `0`, puis relancer (build incrémental).
- `source bin/activate-hermit` avant tout cargo.
- Formatage : **jamais `cargo fmt`**. Par fichier touché : `rustfmt --edition 2021 --style-edition 2024 --check <f>` puis `--style-edition 2021 --check <f>` → appliquer le style qui laisse le fichier propre ; `tui/mod.rs` est sale dans les deux styles → formater tes hunks à la main. `git diff --stat` ne doit contenir que tes changements logiques.
- Clippy scoped : `cargo clippy -p kaji -p kaji-cli --all-targets -- -D warnings`. Tests : `cargo test -p <crate> --lib [filtre]` en itération, suite complète des crates touchés une fois avant commit.
- Règles AGENTS.md : code auto-documenté, zéro commentaire qui paraphrase, `anyhow::Result`, pas de code défensif inutile, pas de logs ajoutés hors erreurs. Doc-comments dans la langue du fichier touché (summon.rs/mcp_client.rs = anglais ; tui/* = français là où c'est déjà le cas).
- Aucune couleur littérale hors palette `theme.rs` ; aucun emoji (kanji et symboles texte seulement).
- Aucun changement aux boucles d'agent (`agent.rs` reply loop, `state_machine/`) : elles émettent déjà `AgentEvent::McpNotification` des deux côtés — la parité AGENTS.md est acquise par construction. Si tu crois devoir toucher une boucle, STOP et remonte au contrôleur.
- Commit : `git add <fichiers touchés>` explicites (jamais `git add -A`), message français `feat(...): …`. Ne jamais commiter `.superpowers/` ni `docs/superpowers/plans/`.
- Zéro subagent côté exécuteur.

---

## Task 1 : Core — `subagent_snapshot()` et `cancel_subagent()` sur le trait, summon, ExtensionManager, Agent

**Files:**
- Modify: `crates/kaji/src/agents/mcp_client.rs` (types + 2 méthodes par défaut sur le trait, ~ligne 152-158 pour le pattern)
- Modify: `crates/kaji/src/agents/platform_extensions/summon.rs` (overrides + tests)
- Modify: `crates/kaji/src/agents/extension_manager.rs` (2 méthodes pub)
- Modify: `crates/kaji/src/agents/agent.rs` (2 délégations pub)
- Modify: `crates/kaji/src/agents/mod.rs` (re-export des nouveaux types si nécessaire pour kaji-cli)

**Interfaces:**
- Consumes: `SummonClient { background_tasks: Mutex<HashMap<String, BackgroundTask>>, completed_tasks: Mutex<HashMap<String, CompletedTask>> }` (`summon.rs:468-475`), `BackgroundTask { id, description, started_at: Instant, turns: Arc<AtomicU32>, cancellation_token: CancellationToken, … }` (`summon.rs:67-76`), `CompletedTask { id, description, result: Result<String, String>, turns_taken: u32, duration: Duration, … }` (`summon.rs:78-85`), `cleanup_completed_tasks()` (`summon.rs:1754-1803`), `ExtensionManager::get_server_client` (privée, `extension_manager.rs:2151-2158`), `Agent.extension_manager: Arc<ExtensionManager>` (`agent.rs:257`).
- Produces (les Tasks 2-4 s'appuient dessus) :
  ```rust
  // mcp_client.rs
  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  pub enum SubagentTaskStatus { Running, Completed, Failed }

  #[derive(Debug, Clone)]
  pub struct SubagentTaskSnapshot {
      pub id: String,
      pub description: String,
      pub status: SubagentTaskStatus,
      pub turns: u32,
      pub elapsed_secs: u64,
      pub result: Option<String>,
      pub error: Option<String>,
  }
  // sur le trait McpClientTrait (mêmes qualificateurs async que get_moim) :
  async fn subagent_snapshot(&self) -> Vec<SubagentTaskSnapshot> { Vec::new() }
  async fn cancel_subagent(&self, _task_id: &str) -> bool { false }
  // Agent
  pub async fn subagent_snapshot(&self) -> Vec<SubagentTaskSnapshot>
  pub async fn cancel_subagent(&self, task_id: &str) -> bool
  ```

**Spécification.**

1. **Types + trait** (`mcp_client.rs`) : les deux types ci-dessus près du trait ; deux méthodes par défaut sur `McpClientTrait`, copies conformes du style `get_moim`/`update_working_dir` (`mcp_client.rs:152-158`). Doc-comment une ligne chacun : ce que rend le défaut (vide / false) et qui override (summon).
2. **Override summon** (`summon.rs`) :
   - `subagent_snapshot` : `self.cleanup_completed_tasks().await` d'abord (draine les handles finis, `summon.rs:1754`), puis mappe `background_tasks` → `status: Running`, `turns: t.turns.load(Ordering::Relaxed)`, `elapsed_secs: t.started_at.elapsed().as_secs()`, `result: None`, `error: None` ; puis `completed_tasks` → `Ok(o)` = `Completed` + `result: Some(o)`, `Err(e)` = `Failed` + `error: Some(e)`, `turns: turns_taken`, `elapsed_secs: duration.as_secs()`. Tri final par `id` (ordre stable).
   - `cancel_subagent` : miroir exact du chemin cancel existant (`summon.rs:1021-1025`) — `background_tasks.lock().await.remove(task_id)` ; si absent → `false` ; sinon `task.cancellation_token.cancel()` puis `true`. Pas de `handle.abort()` (le chemin existant n'en fait pas), pas de réinsertion dans `completed_tasks` (idem existant).
   - Si la conversion est incommode à tester via un `SummonClient` complet (construction de `PlatformExtensionContext` lourde), extraire deux fonctions libres testables `fn snapshot_of_running(id: &str, task: &BackgroundTask) -> SubagentTaskSnapshot` et `fn snapshot_of_completed(task: &CompletedTask) -> SubagentTaskSnapshot`, l'override ne faisant que boucler dessus.
3. **ExtensionManager** : deux méthodes pub qui résolvent le client par nom et délèguent :
   ```rust
   pub async fn subagent_snapshot(&self) -> Vec<SubagentTaskSnapshot> {
       match self.get_server_client(summon::EXTENSION_NAME).await {
           Some(client) => client.subagent_snapshot().await,
           None => Vec::new(),
       }
   }
   pub async fn cancel_subagent(&self, task_id: &str) -> bool {
       match self.get_server_client(summon::EXTENSION_NAME).await {
           Some(client) => client.cancel_subagent(task_id).await,
           None => false,
       }
   }
   ```
   (`EXTENSION_NAME = "summon"`, `summon.rs:36` ; vérifier la normalisation de nom que `get_server_client` attend et l'aligner.)
4. **Agent** : deux délégations une-ligne vers `self.extension_manager`.
5. **Re-exports** : `SubagentTaskSnapshot`/`SubagentTaskStatus` accessibles depuis kaji-cli (suivre le patron de `SUBAGENT_TOOL_REQUEST_TYPE`, `agents/mod.rs:37`).

**Tests (TDD — écrire les tests d'abord, les voir échouer, implémenter, les voir passer).**

- summon.rs (module `#[cfg(test)]` existant ou créé) :
  (a) une `BackgroundTask` fabriquée (`handle: tokio::spawn(std::future::pending())` ou équivalent, `turns` à 7) → snapshot `Running`, turns 7, `result`/`error` `None` ;
  (b) une `CompletedTask` `Ok("done")` → `Completed` + `result Some("done")` ; une `Err("boom")` → `Failed` + `error Some("boom")` ;
  (c) `cancel_subagent("inconnu")` → `false` ; sur une tâche présente → `true`, token `is_cancelled()`, tâche retirée de `background_tasks` ;
  (d) tri : deux tâches insérées dans le désordre → snapshot trié par id.
- Suite : `cargo test -p kaji --lib summon` puis suite complète `cargo test -p kaji` avant commit ; clippy vert.

**Commit.** `feat(agents): instantané et annulation des tâches subagents — subagent_snapshot/cancel_subagent sur McpClientTrait, override summon, exposition ExtensionManager et Agent`

**Rapport.** Coller la sortie des tests (a)-(d) et les signatures exactes ajoutées au trait.

---

## Task 2 : TUI — module `forge.rs` (état + réconciliation) et double alimentation (tick 1 s + notifications)

**Files:**
- Create: `crates/kaji-cli/src/tui/forge.rs`
- Modify: `crates/kaji-cli/src/tui/app.rs` (champs App, bras `McpNotification`, parse)
- Modify: `crates/kaji-cli/src/tui/mod.rs` (déclaration module, `forge_tick` 1 s)

**Interfaces:**
- Consumes: `kaji::agents::{SubagentTaskSnapshot, SubagentTaskStatus, SUBAGENT_TOOL_REQUEST_TYPE}` (Task 1 + existant `agents/mod.rs:37`), `Agent::subagent_snapshot()` (Task 1), `rmcp::model::ServerNotification` (payload construit à `subagent_handler.rs:286-311` : `LoggingMessageNotification`, data JSON `{ "type": "subagent_tool_request", "subagent_id": …, "tool_call": { "name": … } }`, accès Rust `n.params.data`).
- Produces (Tasks 3-4 s'appuient dessus) :
  ```rust
  // forge.rs
  pub enum ForgeStatus { Running, Done, Failed, Cancelled }
  pub struct ForgeTask {
      pub id: String,
      pub description: String,
      pub status: ForgeStatus,
      pub current_tool: Option<String>,
      pub elapsed_secs: u64,
      pub turns: u32,
      pub result: Option<String>,
      pub error: Option<String>,
  }
  pub enum ForgeView { Auto, ForcedOpen, ForcedClosed }
  pub struct ForgeState {
      pub tasks: BTreeMap<String, ForgeTask>,
      pub selected: usize,
      pub view: ForgeView,
      pub folds_at: Option<Instant>,
  }
  impl ForgeState {
      pub fn apply_snapshot(&mut self, snap: Vec<SubagentTaskSnapshot>);
      pub fn apply_tool_notification(&mut self, subagent_id: &str, tool_name: &str);
      pub fn mark_cancelled(&mut self, id: &str);
      pub fn visible(&self) -> bool;
      pub fn toggle(&mut self);
      pub fn running_count(&self) -> usize;
      pub fn selected_task(&self) -> Option<&ForgeTask>;
  }
  // App
  pub forge: forge::ForgeState,
  ```

**Spécification.**

1. **Réconciliation `apply_snapshot`** (le snapshot fait autorité sur le statut, la notification sur `current_tool`) :
   - Entrée présente dans le snapshot : créer/mettre à jour `description`, `turns`, `elapsed_secs`, `result`, `error` ; statut `Running→Running` (conserver `current_tool` local), `Completed→Done`, `Failed→Failed` ; une tâche localement `Cancelled` ne repasse jamais `Running` (l'annulation locale gagne le temps que summon la retire).
   - Entrée locale `Running` absente du snapshot → `Done` (jamais de ligne qui s'évapore sans état final) ; `current_tool = None`.
   - **Nouvelle tâche** (id inconnu jusqu'ici, statut `Running`) → `view = Auto`, `folds_at = None` (le volet réapparaît même s'il était forcé fermé — décision spec).
   - Quand plus aucune tâche `Running` et qu'il y en avait : `folds_at = Some(Instant::now() + FORGE_FOLD)` avec `FORGE_FOLD: Duration = 5 s` (posé une seule fois — pas re-décalé à chaque snapshot).
   - `selected` clampé à la taille de la liste après chaque application.
2. **`apply_tool_notification`** : entrée existante → `current_tool = Some(tool_name)`, statut inchangé ; id inconnu → créer `ForgeTask { id, description: id.to_string(), status: Running, current_tool: Some(tool), … }` (le prochain snapshot remplira la description).
3. **`visible()`** : `ForcedOpen → true` ; `ForcedClosed → false` ; `Auto → !tasks.is_empty() && (running_count() > 0 || folds_at.is_some_and(|t| t > Instant::now()))`.
4. **`toggle()`** (Ctrl+F, `/forge`) : si `visible()` → `ForcedClosed` ; sinon → `ForcedOpen`.
5. **Bras `McpNotification`** dans `apply_agent_event` (`app.rs:3148-3159`, remplace le `_ => {}` pour cette variante — garder un bras explicite par variante restante) :
   ```rust
   AgentEvent::McpNotification((_, notification)) => self.apply_mcp_notification(notification),
   ```
   `apply_mcp_notification` : ne traite que `ServerNotification::LoggingMessageNotification(n)` dont `n.params.data["type"] == SUBAGENT_TOOL_REQUEST_TYPE` ; extrait `subagent_id` et `tool_call.name` (`as_str()` prudents) ; tout champ manquant ou payload d'un autre type → return silencieux, jamais de panic.
6. **`forge_tick`** dans `event_loop` (`mod.rs`, à côté de `git_tick` `mod.rs:900`) :
   ```rust
   let mut forge_tick = tokio::time::interval(Duration::from_secs(1));
   forge_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
   // dans le select! :
   _ = forge_tick.tick(), if app.turn_active || !app.forge.tasks.is_empty() || app.forge.visible() => {
       app.forge.apply_snapshot(agent.subagent_snapshot().await);
   }
   ```
   La garde `app.turn_active` est ce qui fait découvrir la **première** tâche (le `delegate` se produit pendant un tour) ; ensuite `!tasks.is_empty()` prend le relais parent idle.

**Tests (TDD).**

- `forge.rs` : (a) snapshot Running puis notification → `current_tool` posé, statut intact ; (b) notification avant tout snapshot → entrée créée, description = id, puis snapshot → description remplacée ; (c) tâche Running absente du snapshot suivant → `Done` ; (d) `Cancelled` local + snapshot Running même id → reste `Cancelled` ; (e) dernier Running terminé → `folds_at` posé une seule fois (deux snapshots successifs sans Running → même instant) ; nouvelle tâche → `folds_at = None`, `view = Auto` même depuis `ForcedClosed` ; (f) `visible()` sur les 3 × états ; `toggle()` depuis visible et caché ; (g) `selected` clampé quand la liste rétrécit.
- `app.rs` : (h) `McpNotification` payload valide → `forge.tasks` mis à jour ; (i) payload d'un autre `type`, payload sans `subagent_id`, data non-objet → ignorés sans panic (3 cas).
- `cargo test -p kaji-cli --lib forge` en itération ; suite complète + clippy avant commit.

**Commit.** `feat(tui): état forge — réconciliation snapshot/notifications des subagents, tick 1 s, parse subagent_tool_request`

**Rapport.** Sortie des tests (a)-(i) ; confirmer qu'aucun fichier des boucles d'agent n'est touché.

---

## Task 3 : TUI — rendu du volet (layout, auto-ouverture/repli, deux lignes par tâche, `遣 N` barre d'état)

**Files:**
- Modify: `crates/kaji-cli/src/tui/ui.rs` (layout + `draw_forge` + `forge_width`)
- Modify: `crates/kaji-cli/src/tui/statusbar.rs` (`遣 N`)
- Modify: `crates/kaji-cli/src/tui/theme.rs` (glyphe `遣` : `pub const SUBAGENT_GLYPH: &str = "遣";` près de `FIRE_GLYPH`, + au test de largeur 2 cellules)

**Interfaces:**
- Consumes: `App.forge: ForgeState` (Task 2), `app.current_tool()` (`app.rs:2478`), `theme::{blade_frame, accent, muted_color, seal, FIRE_GLYPH, THINKING_GLYPH}`, patron layout explorateur (`ui.rs:42-50`, `explorer_width` `ui.rs:856`), marges intérieures T1 (une ligne haut/bas, une colonne gauche).
- Produces: `fn draw_forge(frame, app, area)` appelé depuis `draw` ; `fn forge_width(total: u16) -> u16` ; `statusbar` rend `遣 N` (muted) quand `running_count() > 0 && !forge.visible()`.

**Spécification.**

1. **Layout** (`ui.rs::draw`, zone lignes 25-50) : quand `app.forge.visible()`, découper une colonne `forge_cols` au bord droit du body (après le carve explorateur), `forge_width(total)` = 32 clampé façon `explorer_width` (cède si le chat passerait sous son plancher ; à <90 colonnes le volet prend le pas sur le chat, même arbitrage que l'explorateur — réutiliser les mêmes constantes de plancher). Le volet coexiste avec lecteur/SPEC : ordre gauche→droite `explorateur | chat | lecteur-ou-SPEC | forge`.
2. **`draw_forge`** : bloc bordé titre ` 炉 forge `, marges T1 (contenu à `inner.y + 1`, colonne gauche). Contenu :
   - Ligne 0 (non sélectionnable) : ` {sceau du mode} 鍛冶 ` + seconde ligne `   火 {current_tool()}` pendant `turn_active`, `   思` sinon (styles : sceau via `theme::seal(mode_color)` — réutiliser `statusbar::mode_color` en la rendant `pub(crate)` —, outil en `theme::accent()`).
   - Par tâche (ordre du BTreeMap), deux lignes : `{glyphe} 遣 {description}` (description tronquée à la largeur restante par `…`) puis `   {détail} · {durée}` où glyphe/détail/style par statut : `Running` → `blade_frame(elapsed)` + `火 {current_tool}` ou `思` (accent) ; `Done` → `✓` + `terminé` (muted) ; `Failed` → `✗` + `échec` (accent) ; `Cancelled` → `✗` + `annulé` (muted). Durée `{elapsed_secs}s` sous 60 s, `{m}m{s:02}s` au-delà.
   - Sélection (`app.forge.selected`, Task 4 pour les touches) : les deux lignes de la tâche sélectionnée sur fond `theme` utilisé par la sélection explorateur (réutiliser exactement le même style).
   - Pied `title_bottom` : `↑/↓ · Enter fiche · x annule` (muted), seulement si ≥1 tâche.
3. **Barre d'état** (`statusbar.rs`) : groupe `遣 {running_count}` (style muted) inséré avant le groupe `火` dans `telemetry_spans`, rendu seulement si `running_count() > 0 && !app.forge.visible()` ; participe à `fits()` comme les autres groupes.
4. **Hauteurs/planchers** : toutes soustractions en `saturating_sub` ; hauteur 12 → pas de panic ; volet plus court que la liste → défilement implicite calé sur `selected` (la fenêtre glisse pour garder la sélection visible, patron explorateur).

**Tests (TDD).**

- `ui.rs` : (a) `rendered(&app, 130, 24)` avec 2 tâches Running + 1 Done → le volet contient `炉 forge`, `遣`, les descriptions, `✓` ; (b) volet absent quand `visible() == false` ; (c) tâche sélectionnée stylée différemment (assert sur le style de span) ; (d) largeur 60 → pas de débordement ni panic, le chat cède la place ; (e) hauteur 12 → pas de panic ; (f) description longue → tronquée par `…` dans la largeur du volet.
- `statusbar.rs` : (g) `遣 2` présent quand 2 Running et volet fermé ; absent volet ouvert ; absent quand 0 Running.
- `theme.rs` : `遣` ajouté au test des glyphes 2 cellules.
- Suite kaji-cli complète + clippy avant commit.

**Commit.** `feat(tui): volet 炉 forge — colonne droite auto-ouverte, deux lignes par subagent, 遣 N en barre d'état`

**Rapport.** Coller un rendu texte 130×24 avec 3 tâches (Running avec outil, Running en réflexion, Done) et le rendu 60 colonnes.

---

## Task 4 : TUI — interactions (Focus::Forge, Ctrl+F, `/forge`, ↑/↓, Enter fiche, x annulation confirmée, `/help`)

**Files:**
- Modify: `crates/kaji-cli/src/tui/app.rs` (Focus, touches, commande, fiche, `pending_forge_cancel`)
- Modify: `crates/kaji-cli/src/tui/mod.rs` (consommation du pending y/n, `/help`, appel `cancel_subagent`)

**Interfaces:**
- Consumes: `ForgeState::{toggle, selected_task, mark_cancelled}` (Task 2), `Viewer { path, lines, scroll, truncated, binary }` construit à la main (`viewer.rs:22-33`), `Agent::cancel_subagent` (Task 1), patron `pending_restore` (`app.rs:694, 2500-2505` + consommation event_loop), `Focus`/`cycle_focus` (`app.rs:43, 2035-2050`), `explorer_key` comme modèle de routage (`app.rs:2055`).
- Produces: `Focus::Forge` ; `App::toggle_forge()` ; `App::forge_key(&KeyEvent) -> Action` ; `App::take_pending_forge_cancel() -> Option<String>` ; commande `/forge` ; lignes `/help`.

**Spécification.**

1. **Focus** : variante `Forge` dans `Focus` (`app.rs:43`) ; `cycle_focus` ORDER devient `[Composer, Explorer, Viewer, Forge]`, `Forge` ouvert ssi `self.forge.visible()`. Fermer le volet pendant qu'il a le focus → focus `Composer` (miroir `close_explorer`, `app.rs:2027-2028`).
2. **Ctrl+F global** (même étage de routage que Ctrl+E) : `app.toggle_forge()` = `self.forge.toggle()` ; si devenu visible → `focus = Focus::Forge` ; si devenu caché et focus était `Forge` → `Composer`. Sans extension summon ni tâche connue (`tasks.is_empty()` et toggle vers ouvert) : pousser ligne système `forge : aucune tâche` et ne PAS forcer l'ouverture (rester dans l'état courant).
3. **Commande `/forge`** : même effet que Ctrl+F ; entrée dans la table des commandes du welcome (`mod.rs:~256`, tableau 14→15 lignes : `("/forge", "(Ctrl+F) volet forge — subagents en cours")`) et dans la palette si les commandes y sont déclarées (suivre le patron `/explorer`).
4. **`forge_key`** (focus Forge, miroir de `explorer_key`) : `↑`/`k` et `↓`/`j` déplacent `selected` (clamp, ligne 0 = première **tâche** — l'agent principal n'est pas sélectionnable) ; `Enter` → fiche ; `x` → armement annulation ; `Esc`/`Ctrl+F` → ferme (ForcedClosed) + focus Composer ; le reste ignoré.
5. **Fiche détail** (Enter) : construire `Viewer` à la main — `path: format!("遣 {}", task.description)` (tronqué 60 chars), `lines` : `tâche    : {description}` (wrap manuel à ~76), `statut   : {statut} · {durée}`, `tours    : {turns}`, `outil    : {current_tool}` si Running, ligne vide, `résultat :` + lignes du résultat (ou `erreur :` + erreur) si terminé — `sanitize_for_display` sur chaque ligne ; `scroll: 0, truncated: false, binary: false`. Poser `app.viewer = Some(v)` + `focus = Focus::Viewer`. Tant que la fiche est ouverte ET que la tâche existe encore, le tick forge la régénère à chaque `apply_snapshot` si `viewer.path` correspond (garder `scroll`).
6. **Annulation `x`** (uniquement statut `Running`, sinon ligne système `forge : tâche déjà terminée`) : `pending_forge_cancel = Some(id)` + ligne système `annuler 遣 {description} ? y/n` — miroir exact du patron `pending_restore` (`app.rs:2500-2505`) : `y` → event_loop `take_pending_forge_cancel()` puis `agent.cancel_subagent(&id).await` ; `true` → `app.forge.mark_cancelled(&id)` + ligne `遣 {description} — annulée` ; `false` → ligne `forge : tâche déjà terminée` ; `n`/autre touche → désarme sans rien faire.
7. **`/help`** (`mod.rs`, formes tableau ET texte) : après la ligne `Ctrl+E`, ajouter `("Ctrl+F", "volet forge (/forge) — qui fait quoi : subagents, statut, outil en cours")` ; mettre à jour la ligne `Ctrl+O` (`composer → explorateur → lecteur → forge`) ; la ligne `barre d'état` de T2-hanko gagne ` · 遣 subagents actifs` en queue. Adapter les tests existants qui comptent les lignes.

**Tests (TDD).**

- `app.rs` : (a) Ctrl+F ouvre (ForcedOpen + focus Forge) puis ferme (ForcedClosed + focus Composer) ; (b) `↑/↓` clampés ; (c) Enter → `app.viewer` non-None, `path` contient la description, lines contiennent `statut` ; (d) `x` sur Running arme `pending_forge_cancel` et pousse la ligne y/n ; `x` sur Done pousse `déjà terminée` sans armer ; (e) `cycle_focus` inclut Forge ssi visible ; (f) `/forge` équivaut à Ctrl+F ; toggle sans tâche → `forge : aucune tâche`, pas d'ouverture.
- `mod.rs` : (g) `/help` contient `Ctrl+F` et `forge` dans les deux formes ; (h) welcome table 15 lignes.
- Suite complète kaji-cli + kaji, clippy, avant commit.

**Commit.** `feat(tui): forge interactive — Focus::Forge, Ctrl+F//forge, fiche détail dans le lecteur, annulation confirmée y/n, /help`

**Rapport.** Rendu texte : volet avec sélection + fiche ouverte ; transcription du flux x → y → ligne `annulée`.

---

## Self-review du plan (fait à l'écriture)

- Couverture spec : constat→T1/T2 ; volet/auto-forge→T2 (état) + T3 (rendu) ; fiche/x/`遣 N`/help→T3-T4 ; bords (pas de summon, 60 col, hauteur 12, tâche disparue, cancel tardif) → répartis T1(c)/T2(c)/T3(d,e)/T4(d,f). Hors périmètre respecté (pas de flux live, pas d'arbre, pas d'ACP).
- Écart spec assumé : le snapshot expose `elapsed_secs` plutôt que `started/last_activity/finished` (le rendu n'a besoin que de la durée) ; `SubagentTaskStatus` n'a pas de variante `Cancelled` (summon retire les tâches annulées des deux maps — l'état `Annulé` vit côté TUI via `mark_cancelled`).
- Types cohérents T1→T4 : `SubagentTaskSnapshot`/`SubagentTaskStatus` (T1) consommés par `apply_snapshot` (T2) ; `ForgeState` (T2) consommé par T3/T4 ; `mode_color` rendue `pub(crate)` en T3.
