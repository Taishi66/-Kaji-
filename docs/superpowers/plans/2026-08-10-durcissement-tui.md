# Plan — Durcissement TUI post-MVP (5 tâches séquentielles)

> **⚠ Errata post-exécution (2026-08-11, review finale)** — deux erreurs dans ce plan, corrigées à l'implémentation ; ne pas réintroduire en cas de relecture :
> 1. **Task 2, snippet verdict** : déplacer l'unique `pass.advance()` dans la branche VALIDE est FAUX — la machine exige deux `advance()` pour franchir Validate→DriftLock→Done ; le snippet aurait laissé DriftLock en Running (`is_complete()` jamais vrai). Code livré : `advance()` inconditionnel + `advance()`/`fail_current()` conditionnel (vérifié par trace sdd.rs:102-119). L'annexe A porte la même erreur.
> 2. **Task 2, placement de `resolve_spec`** : le plan le mettait dans `tui::run()`, APRÈS la création de session par `handle_tui` → session vide orpheline sur `--spec` cassé. Corrigé en fix wave (`1b884331e`) : résolution dans `handle_tui` avant `get_or_create_session_id`.

## Contexte

Résidus de review du MVP TUI (roadmap KAJI, item « Durcissement TUI post-MVP »). Recherche
préalable exhaustive (workflow 5 agents, file:line vérifiés au BASE `10e6908cd`) archivée en
annexes JSON dans `2026-08-10-durcissement-tui-annexes/` (à côté de ce plan) :

- `ANNEXE_A` = `recherche-verdict-et-spec.json` (⚠ porte le snippet verdict erroné — voir errata)
- `ANNEXE_B` = `recherche-resume-tui.json`
- `ANNEXE_C` = `recherche-input-wrap-scroll.json`
- `ANNEXE_D` = `recherche-freeze-setup-tour.json` (cartographie option A pour le backlog)
- `ANNEXE_E` = `recherche-trim-ratatui.json`

Chaque tâche cite son annexe : l'implémenteur la lit AVANT de coder — elle contient le
comportement actuel exact (file:line), le design retenu et les tests attendus.

## Global Constraints

- Scope : `crates/kaji-cli` uniquement (Cargo.toml, src/cli.rs, src/commands/tui.rs, src/tui/*). **Aucun changement dans `crates/kaji`** (pas de signature `Agent::reply`, pas de parité agent-loop à gérer — c'est un choix de design, voir Task 5).
- Un commit par tâche, message français préfixé `kaji: tui — `, trailer `Claude-Session: https://claude.ai/code/session_014ngoE4sNSgzrZPdgb7qC2r`.
- TDD quand faisable ; tests dans les `mod tests` inline existants (`app.rs`, `cli.rs`, `tui/mod.rs`), style imité des tests voisins.
- Baseline : kaji-cli 310 tests verts au départ (`10e6908cd`) ; ne rien casser. `cargo fmt` obligatoire. Clippy scoped : `cargo clippy -p kaji-cli --all-targets -- -D warnings` (le `--all-targets` workspace est rouge, cause préexistante acp_fixtures hors scope).
- Cargo en foreground, un seul à la fois (verrou target/ partagé).
- Code self-documenting, commentaires « why » rares.

## Task 1 — Trim ratatui (default-features=false)

Annexe : `ANNEXE_E`. Fichier : `crates/kaji-cli/Cargo.toml:70`.

1. `cargo add -p kaji-cli ratatui@0.30.2 --no-default-features --features crossterm --optional` → cible : `ratatui = { version = "0.30.2", default-features = false, features = ["crossterm"], optional = true }`. Vérifier au `git diff` que `optional = true` est préservé (sinon éditer à la main) et que rien d'autre n'a bougé dans Cargo.toml.
2. Justification vérifiée (annexe) : seul `crossterm` est requis — `ratatui::crossterm::*`, `init()/restore()/DefaultTerminal` sont gatés dessus ; `style/text/layout/widgets` core (Block/Borders/Clear/Paragraph/Wrap) sont inconditionnels ; zéro usage de macros/underline-color/serde/all-widgets (greps exhaustifs en annexe) ; aucun autre crate du workspace ne dépend de ratatui (pas d'unification qui réactiverait les defaults).
3. Vérifier `git diff Cargo.lock` : disparition attendue de `ratatui-macros` et `critical-section`, lien `ratatui-widgets → time` coupé (`time` reste présent via cookie/yasna — attendu, pas un échec).
4. Ne pas toucher aux features crate `tui` (:104) ni `ratatui` (:127) — hors scope.

Vérification : `cargo build -p kaji-cli` ; `cargo test -p kaji-cli` ; clippy scoped ; fmt.
Commit : `kaji: tui — trim ratatui (default-features=false, crossterm seul)`

## Task 2 — SDD fail-closed : verdict absent → DRIFT + `--spec` introuvable → erreur

Annexe : `ANNEXE_A`. Fichiers : `crates/kaji-cli/src/tui/app.rs`, `crates/kaji-cli/src/tui/mod.rs`.

### 2a. Verdict (app.rs, turn_end, bras `PassDriver::Validating`, ~188-203)

Aujourd'hui : test unique négatif `contains("VERDICT: DRIFT")` → tout le reste (buffer vide, verdict tronqué, texte hors format) passe VALIDE (fail-open, confirmé annexe). Remplacer par un test POSITIF sur VALIDE, fail-closed sinon :

```rust
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
```

Garder driver → Idle et le retour `None` inchangés. Attention à l'ordre actuel : `pass.advance()` est appelé AVANT le test (ligne 189) — le déplacer dans la branche VALIDE. Aucun changement kaji-core (le parsing vit entièrement dans app.rs, vérifié).

### 2b. `--spec` (tui/mod.rs)

Aujourd'hui : `resolve_spec` (:384-390) fait `SpecDoc::load(&path).ok()` — un `--spec` explicite introuvable/illisible est avalé, indistinguable de « pas de spec » (symptôme : `/sdd` répond « aucune SPEC chargée »). Changement :

1. `resolve_spec(spec_path: Option<PathBuf>) -> anyhow::Result<Option<SpecDoc>>` : si `Some(path)` (flag explicite) → `SpecDoc::load(&path).map(Some).with_context(|| format!("--spec {}", path.display()))` (erreur dure) ; si `None` → auto-détection `SPEC.md` inchangée (soft, `.ok()` conservé).
2. Déplacer l'appel dans `run()` AVANT `ratatui::init()` (:34) pour bail proprement sans toucher le terminal ; `event_loop` prend `spec: Option<SpecDoc>` au lieu de `spec_path` (supprimer l'appel :203).
3. `use anyhow::Context;` dans les imports.

### Tests (TDD)

- `turn_end` Validating avec buffer sans aucun token → passe échouée + message « DRIFT par prudence » (le test qui aurait échoué avant — RED d'abord).
- Buffer avec `VERDICT: DRIFT` → échouée + « drift détecté ».
- Happy path `VERDICT: VALIDE` existant (:672) reste vert.
- `resolve_spec` : chemin explicite inexistant → Err ; `None` sans SPEC.md dans cwd → Ok(None). (Tests de `resolve_spec` : fonction dans tui/mod.rs — la rendre testable telle quelle ; utiliser tempdir si besoin d'un fichier réel, dev-dep `tempfile` déjà présente dans le workspace kaji-cli ? vérifier, sinon chemin inexistant suffit pour le cas Err.)

Vérification : `cargo test -p kaji-cli` ; clippy scoped ; fmt.
Commit : `kaji: tui — SDD fail-closed (verdict absent → DRIFT) et --spec introuvable → erreur`

## Task 3 — `kaji tui --resume`

Annexe : `ANNEXE_B`. Fichiers : `crates/kaji-cli/src/cli.rs`, `crates/kaji-cli/src/commands/tui.rs`, `crates/kaji-cli/src/tui/mod.rs`, `crates/kaji-cli/src/tui/app.rs` (si nécessaire pour l'état outil interrompu).

Constat clé (annexe) : `CliSession::new` charge DÉJÀ l'historique inconditionnellement depuis `session_id` ; `seed_chat` (:392) fonctionne déjà. Ce qui manque : un moyen de pointer une session existante + le flag `resume` dans `SessionBuilderConfig` (sans lui, provider/modèle/extensions sauvegardés ne sont pas restaurés — builder.rs:269/421/537).

1. `cli.rs` `Command::Tui` (~1098) : ajouter `#[command(flatten)] identifier: Option<Identifier>` + `#[arg(short, long)] resume: bool` — pattern exact de `Command::Session` (:934-948). Validation `--session-id`/`--name`/`--path` sans `--resume` → erreur, même pattern que les sites existants (cli.rs:1678-1687 / 1915-1924), au point de dispatch.
2. Dispatch (~2372) : `handle_tui(spec, identifier, resume)`.
3. `commands/tui.rs` : `get_or_create_session_id(identifier, resume, false, kaji_mode)` (réutilise « dernière session User » si resume sans identifiant, cli.rs:406-416) + `SessionBuilderConfig { resume, session_id, interactive: true, ..Default::default() }`.
4. `handle_default_session` (:2226) inchangé : `kaji` nu = nouvelle session.
5. `tui/mod.rs` : `push_welcome` seulement si la conversation seedée est vide.
6. Outils orphelins : après `seed_chat`, toute ligne outil encore dans `pending_tools` (ToolRequest sans ToolResponse — session interrompue mi-appel) est clôturée en état « ✗ interrompu » (pas de spinner fantôme figé). Implémentation minimale dans App (méthode appelée post-seed).
7. Hors scope explicite : `--fork`, `--history` (seed_chat le remplace), positional nu (`--session-id` suffit, cohérent avec Session/Run).

### Tests (TDD)

- cli.rs : `tui_command_accepts_resume_flag`, `tui_command_accepts_resume_with_session_id`, `tui_command_rejects_session_id_without_resume` (patrons :2608-2630).
- seed : `seed_chat_replays_persisted_messages_into_chat_lines` (1 user + 1 assistant → app.chat ordre/contenu), `seed_chat_replays_tool_request_and_response_pair` (ligne outil clôturée ✓, pending_tools vide), `seed_chat_closes_unmatched_tool_request_as_interrupted` (ToolRequest orphelin → état interrompu, pas de pending résiduel).
- Welcome : conversation non vide → pas de bandeau de bienvenue ; vide → bandeau présent.

Risques documentés acceptés (annexe) : durées d'outils rejouées ≈ 0 s (cosmétique) ; prompt cliclack de changement de cwd avant `ratatui::init()` (comportement hérité du chemin resume classique, OK) ; re-parsing markdown à chaque frame sur long historique (dette préexistante, hors scope — noter au ledger).

Vérification : `cargo test -p kaji-cli` ; clippy scoped ; fmt. E2E léger : `kaji tui --resume --session-id inexistant` doit produire une erreur propre (pas de panic) — vérifiable en non-TTY si le chemin d'erreur sort avant l'init TUI, sinon constater via test unitaire seulement.
Commit : `kaji: tui — --resume (dernière session ou --session-id), replay de l'historique dans le chat`

## Task 4 — Input : scroll horizontal à curseur suiveur

Annexe : `ANNEXE_C`. Fichiers : `crates/kaji-cli/src/tui/app.rs`, `crates/kaji-cli/src/tui/ui.rs`.

Décision (annexe, argumentée) : scroll horizontal single-line, PAS de wrap multi-ligne — `.wrap()` et `scroll.x` sont mutuellement exclusifs dans ratatui 0.30 (preuve lue dans ratatui-widgets 0.3.2 : la branche wrap ignore `scroll.x`), le layout reste `Length(3)`, et l'input n'a aucune édition mi-chaîne (push/pop fin de chaîne uniquement) donc le curseur est structurellement en fin.

1. `app.rs` (~234, après le bloc scroll chat) : `pub fn input_cursor_chars(&self) -> u16` (= `chars().count()`) et `pub fn input_scroll_x(&self, visible_width: u16) -> u16` (= `cursor.saturating_sub(visible_width.saturating_sub(1))`). Invariant : le curseur reste sur la dernière colonne visible dès dépassement.
2. `ui.rs` `draw_input` (:198-220) : `let scroll_x = app.input_scroll_x(inner.width.max(1));` + `.scroll((0, scroll_x))` sur le Paragraph (PAS de `.wrap()`) ; `cursor_x = inner.x + (app.input_cursor_chars() - scroll_x).min(inner.width.saturating_sub(1))`.
3. Un commentaire bref « why » au-dessus de `input_scroll_x` : wrap et scroll.x mutuellement exclusifs dans ratatui 0.30 — ne pas ajouter `.wrap()` ici.
4. Hors scope : édition mi-chaîne (Left/Right/Delete), unicode-width pour glyphes larges (dette préexistante — noter au ledger), limite de longueur d'input.

### Tests (TDD, app.rs mod tests ~811)

`input_scroll_x_is_zero_while_input_fits_the_visible_width` ; `input_scroll_x_follows_cursor_once_input_overflows_the_visible_width` (30 chars, width 10 → 21) ; `input_scroll_x_never_panics_on_zero_width_area` ; `input_cursor_chars_counts_unicode_scalars_not_bytes` (« héllo » → 5, len()==6) ; `typing_long_input_keeps_scroll_offset_zero_at_or_below_width` (chemin clavier réel via on_event).

Vérification : `cargo test -p kaji-cli` ; clippy scoped ; fmt.
Commit : `kaji: tui — scroll horizontal de l'input (curseur suiveur, plus de saisie invisible)`

## Task 5 — Input vivant pendant le setup de tour (option B)

Annexe : `ANNEXE_D` — la lire en entier, y compris risks. Fichiers : `crates/kaji-cli/src/tui/mod.rs`, `crates/kaji-cli/src/tui/app.rs`.

Cause racine (annexe) : les trois `send_turn(...).await` inline dans la boucle select (Submit :238, GateApprove :257, chaînage fin de tour :321-330) — pendant l'await du setup (`Agent::reply` : hooks sous-processus, add_message, tokenizer…), le select ne poll plus `input_rx`, le ticker est gaté par `turn_active` encore false, et le CancellationToken n'est stocké qu'APRÈS le setup → gel total, Esc inopérant.

Décision : **option B** de l'annexe — machine à états locale à la TUI, le futur de setup est poll PAR le select lui-même. Zéro changement de signature dans `crates/kaji` (l'option A « stream 'static / Arc<Agent> » est différée post-migration state-machine — décision actée, ne pas l'implémenter).

1. Scinder `send_turn` (:344-370) en `begin_setup` (construit le futur `Box::pin(agent.reply(...))` SANS await — le lifetime reste celui de `agent: &Agent` d'event_loop) et `install_turn` (consomme le résultat : stocke TurnStream + begin_turn). La scission est OBLIGATOIRE pour le borrow checker (risque documenté en annexe : `&mut turn` tenu pendant l'await).
2. `event_loop` : état `pending: Option<Pin<Box<dyn Future<Output = anyhow::Result<TurnStream<'_>>> + '_>>>` + 4e bras du select : `res = async { pending.as_mut().unwrap().await }, if pending.is_some() => { pending = None; install_turn(...) }`. Chaque `.await` interne au setup rend la main aux autres bras → input vivant pendant tout le setup asynchrone.
3. Annulation réelle pendant le setup : créer le CancellationToken AVANT `begin_setup`, le stocker immédiatement dans `cancel` ; sur `Action::CancelTurn` : `token.cancel()` + `pending = None` (drop du futur = interruption immédiate) + message système d'abort qui mentionne que le message utilisateur peut être resté persisté (annulation mi-setup = état partiel assumé, annexe risks).
4. `app.rs` : champ `turn_pending: bool` ; posé au lancement du setup, effacé dans `install_turn`/abort. Étendre les gardes : `Esc if turn_active || turn_pending => CancelTurn` (:327) et `Enter if turn_active || turn_pending` → refus de soumission (:328-331) — verrou anti-réentrance OBLIGATOIRE (deux reply() concurrents sur la même session = corruption de conversation, annexe risks).
5. Ticker : `_ = tick.tick(), if app.turn_active || app.turn_pending` (:223) — sinon pas de redraw pendant le setup. Le status « démarrage du tour… » reste posé par begin_setup.
6. Appliquer le même pattern aux TROIS sites d'await inline (Submit, GateApprove, chaînage fin de tour :321-330).
7. Hors scope (décisions actées) : option A (stream 'static) ; pré-chauffage `get_context_limit` (non mesuré — data-first) ; la section CPU synchrone du tokenizer (`check_if_compaction_needed`) reste bloquante — limite honnête de B, documentée au ledger.

### Tests (TDD, app.rs)

- `esc_during_pending_setup_returns_cancel_turn` (miroir de :589, avec turn_pending=true).
- `enter_during_pending_setup_does_not_submit` (miroir de :603).
- `turn_pending_lifecycle` : posé au début du setup, effacé à l'install et à l'abort (tester via les méthodes App si la transition est portée par App, sinon couvrir ce qui est testable sans event_loop — event_loop lui-même n'est pas testable (prend &mut DefaultTerminal), constat annexe ; ne pas tenter de harnais terminal).

Vérification : `cargo test -p kaji-cli` ; clippy scoped ; fmt. E2E réel obligatoire après install (fait par le contrôleur, pas l'implémenteur) : tmux + session TUI, taper pendant le « démarrage du tour… » et vérifier que les frappes s'affichent, Esc annule.
Commit : `kaji: tui — input vivant pendant le setup de tour (futur de setup poll par le select, Esc annule le setup)`
