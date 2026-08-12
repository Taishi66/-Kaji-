# Checkpoints — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Rendre chaque tour annulable — snapshot git bare de l'arbre avant chaque tour, `/restore <id>` couplé qui remet fichiers **et** conversation à l'état d'un tour choisi — spec `docs/superpowers/specs/2026-08-12-checkpoints-design.md`.

**Architecture:** Store git bare par projet (module `checkpoint` dans crate `kaji`), snapshot non-fatal au `turn_start` de `Agent::reply()` (réutilise l'enveloppe event log), event `checkpoint` dans `session_events` (pas de table nouvelle), restore couplé atomique (pre-restore snapshot → git restore → truncate conversation), surface TUI `/checkpoints`+`/restore`.

**Tech Stack:** `subprocess::git_command()` (sync std::process → `tokio::task::spawn_blocking`), sqlx (session_events existant), ratatui (modal + COMMANDS).

## Global Constraints

- `source bin/activate-hermit` ; cargo TOUJOURS foreground avec `timeout: 600000` EXPLICITE, JAMAIS run_in_background/Monitor/`&`, un seul à la fois. Si Bash bascule en background à 600 s : boucler `ps aux | grep -cE "[c]argo|[r]ustc"` jusqu'à 0 puis reprendre. ⚠ grep/rg de session peuvent corrompre le contenu affiché → `/usr/bin/grep` ou Read.
- TDD strict. Baselines : `cargo test -p kaji --lib` = 8 échecs PRÉEXISTANTS (compaction ×2, gcpauth ×3, chatgpt_codex JWT, prompt_manager snapshot, context_mgmt cutoff) — jamais des régressions, aucun NOUVEL échec ; `cargo test -p kaji-cli` = 459.
- Chaque choix contre-intuitif porte un doc-comment nommant le post-mortem qu'il prévient (`09 - Meta/premortems/kaji-checkpoints-2026-08-12.md`). Les **tests-barrières** (marqués ⛔) échouent si un futur dev prend le raccourci — ne jamais les affaiblir.
- `session_events.kind` n'a pas de contrainte CHECK → ajouter le kind `checkpoint` est libre.
- Un commit par tâche, français `kaji: …` + trailer `Claude-Session: https://claude.ai/code/session_014ngoE4sNSgzrZPdgb7qC2r`. `git add` fichiers explicites (jamais -A). Ne pas commiter les docs spec/plan (déjà commités). Ne pas pusher (contrôleur groupe).
- Numéros de ligne = repères, se fier aux symboles.

---

### Task 1 : `CheckpointStore` — store git bare, snapshot/restore/diff, mutex

**Files:**
- Create: `crates/kaji/src/checkpoint.rs`
- Modify: `crates/kaji/src/lib.rs` (ajouter `pub mod checkpoint;`)
- Test: `mod tests` dans checkpoint.rs (dépôt + projet temp réels via `tempfile::TempDir`)

**Interfaces:**
- Consumes : `subprocess::git_command()` (sync `std::process::Command`), `config::paths::Paths::in_data_dir`.
- Produces :
  - `pub struct CheckpointId(pub String);` (le sha court, ex. `"a1b2c3d4e5f6"`)
  - `pub struct CheckpointStore { git_dir: PathBuf, lock: std::sync::Mutex<()> }`
  - `CheckpointStore::for_project(project: &Path) -> Result<Self>` (calcule `project_key`, `git_dir = Paths::in_data_dir("kaji/checkpoints").join(format!("{project_key}.git"))`, `git init --bare` si absent)
  - `CheckpointStore::snapshot(&self, project: &Path, label: &str) -> Result<(CheckpointId, String)>` (retourne `(id, tree_sha)`)
  - `CheckpointStore::files_created_since(&self, project: &Path, target_tree: &str) -> Result<Vec<PathBuf>>`
  - `CheckpointStore::restore(&self, project: &Path, target: &CheckpointId) -> Result<()>`
  - `fn project_key(project: &Path) -> String` (privé mais testé)
  - Tasks 2/3 consomment `for_project`, `snapshot`, `restore`.

- [ ] **Step 1 : tests RED** (mod tests, un run). Helper : créer un projet temp + un fichier, `CheckpointStore::for_project(temp)`.

```rust
use tempfile::TempDir;
use std::fs;

// override du data dir pour ne pas polluer le vrai store (idiome KAJI_MEMORY_DIR — voir s'il existe un override data_dir ; sinon KAJI_PATH_ROOT sur un TempDir)
fn store_for(project: &std::path::Path) -> CheckpointStore {
    CheckpointStore::for_project(project).expect("store")
}

#[test]
fn snapshot_then_modify_then_restore_returns_the_tree() {
    let root = TempDir::new().unwrap();
    // KAJI_PATH_ROOT ou équivalent pointé sur un autre TempDir pour le git_dir — copier l'idiome des tests session
    let proj = root.path();
    fs::write(proj.join("a.txt"), "v1").unwrap();
    let store = store_for(proj);
    let (id, _tree) = store.snapshot(proj, "t1").unwrap();
    fs::write(proj.join("a.txt"), "v2").unwrap();
    fs::write(proj.join("b.txt"), "new").unwrap();
    store.restore(proj, &id).unwrap();
    assert_eq!(fs::read_to_string(proj.join("a.txt")).unwrap(), "v1", "fichier suivi restauré");
    assert!(!proj.join("b.txt").exists(), "fichier créé depuis le snapshot supprimé");
}

/// ⛔ BARRIÈRE premortem PM5 — restore ne doit JAMAIS toucher les fichiers
/// non-suivis étrangers au snapshot (pas de `git clean`). Ce test échoue si
/// un futur dev remplace le reverse-diff par `git clean -fd`.
#[test]
fn restore_preserves_untracked_files_outside_the_snapshot() {
    let root = TempDir::new().unwrap();
    let proj = root.path();
    fs::write(proj.join("a.txt"), "v1").unwrap();
    let store = store_for(proj);
    let (id, _) = store.snapshot(proj, "t1").unwrap();
    fs::write(proj.join("secret.env"), "TOKEN=xyz").unwrap(); // étranger, jamais snapshoté
    fs::write(proj.join("a.txt"), "v2").unwrap();
    store.restore(proj, &id).unwrap();
    assert_eq!(fs::read_to_string(proj.join("a.txt")).unwrap(), "v1");
    assert!(proj.join("secret.env").exists(), "un non-suivi étranger doit survivre au restore");
}

#[test]
fn files_created_since_lists_only_additions() {
    let root = TempDir::new().unwrap();
    let proj = root.path();
    fs::write(proj.join("a.txt"), "v1").unwrap();
    let store = store_for(proj);
    let (_, tree) = store.snapshot(proj, "t1").unwrap();
    fs::write(proj.join("a.txt"), "v2").unwrap(); // modif, pas ajout
    fs::write(proj.join("c.txt"), "new").unwrap(); // ajout
    let created = store.files_created_since(proj, &tree).unwrap();
    assert_eq!(created, vec![std::path::PathBuf::from("c.txt")]);
}

/// ⛔ BARRIÈRE premortem PM7 — la fonction project_key est un contrat de
/// compat : la changer orphelinerait tous les stores existants. Ce test fige
/// la sortie pour un chemin connu.
#[test]
fn project_key_is_stable_for_a_known_path() {
    // valeur d'or figée au premier run réel (remplacer par la vraie)
    let k = project_key(std::path::Path::new("/tmp/kaji-fixture-proj"));
    assert_eq!(k.len(), 16, "sha256 tronqué 16 hex");
    assert_eq!(k, "<REMPLIR au 1er run>", "project_key ne doit jamais changer sans migration");
}
```

- [ ] **Step 2 : vérifier le RED** — `cargo test -p kaji --lib checkpoint::` FAIL (module inexistant).
- [ ] **Step 3 : implémentation** — checkpoint.rs. Toutes les ops git via un helper qui prend le lock et lance `git_command()` avec `--git-dir`/`--work-tree` :

```rust
use crate::subprocess::git_command;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub struct CheckpointId(pub String);

pub struct CheckpointStore {
    git_dir: PathBuf,
    // premortem PM6 : l'index unique du bare interdit les ops git concurrentes.
    // La sérialisation vit DANS le store (mutex), pas héritée de la boucle TUI.
    lock: Mutex<()>,
}

fn project_key(project: &Path) -> String {
    // repo git → toplevel .git ; sinon cwd canonicalisé. Fonction FIGÉE (PM7).
    let base = git_toplevel(project).unwrap_or_else(|| project.to_path_buf());
    let canon = std::fs::canonicalize(&base).unwrap_or(base);
    let digest = sha256_hex(canon.to_string_lossy().as_bytes());
    digest[..16].to_string()
}

impl CheckpointStore {
    pub fn for_project(project: &Path) -> Result<Self> {
        let git_dir = crate::config::paths::Paths::in_data_dir("kaji/checkpoints")
            .join(format!("{}.git", project_key(project)));
        if !git_dir.exists() {
            std::fs::create_dir_all(&git_dir)?;
            run_git(&git_dir, project, &["init", "--bare", "--quiet"])?; // ou init --bare sur git_dir
        }
        Ok(Self { git_dir, lock: Mutex::new(()) })
    }

    pub fn snapshot(&self, project: &Path, label: &str) -> Result<(CheckpointId, String)> {
        let _g = self.lock.lock().unwrap();
        run_git(&self.git_dir, project, &["add", "-A"])?;
        let tree = run_git(&self.git_dir, project, &["write-tree"])?.trim().to_string();
        let mut args = vec!["commit-tree".to_string(), tree.clone(), "-m".to_string(), label.to_string()];
        if let Some(parent) = self.current_ref_commit() { args.extend(["-p".into(), parent]); }
        let commit = run_git_owned(&self.git_dir, project, &args)?.trim().to_string();
        let id = commit[..12].to_string();
        run_git(&self.git_dir, project, &["update-ref", &format!("refs/kaji/{id}"), &commit])?;
        Ok((CheckpointId(id), tree))
    }

    pub fn files_created_since(&self, project: &Path, target_tree: &str) -> Result<Vec<PathBuf>> {
        let _g = self.lock.lock().unwrap();
        run_git(&self.git_dir, project, &["add", "-A"])?;
        let current = run_git(&self.git_dir, project, &["write-tree"])?.trim().to_string();
        let out = run_git_owned(&self.git_dir, project,
            &["diff".into(), "--name-only".into(), "--diff-filter=A".into(), target_tree.into(), current])?;
        Ok(out.lines().map(PathBuf::from).collect())
    }

    pub fn restore(&self, project: &Path, target: &CheckpointId) -> Result<()> {
        let tree = self.tree_of(&target.0)?;
        let created = self.files_created_since(project, &tree)?;
        let _g = self.lock.lock().unwrap();
        run_git(&self.git_dir, project, &["read-tree", &target.0])?;
        run_git(&self.git_dir, project, &["checkout-index", "-f", "-a"])?;
        // premortem PM5 : supprimer UNIQUEMENT les ajouts diffés — JAMAIS `git clean`
        // (le work-tree est le repo RÉEL de l'utilisateur avec des non-suivis légitimes).
        for f in created {
            let _ = std::fs::remove_file(project.join(f));
        }
        Ok(())
    }
}

fn run_git(git_dir: &Path, work_tree: &Path, args: &[&str]) -> Result<String> { /* git_command() + --git-dir + --work-tree + args, capture stdout, bail sur !status */ }
```

(`sha256_hex` : réutiliser le hasher déjà présent dans le repo — `grep -rn "Sha256\|sha2" crates/kaji/src` ; sinon la dep `sha2` est probablement déjà dans l'arbre. `git_toplevel` : `git -C project rev-parse --show-toplevel`, `None` si pas un repo. `tree_of(id)` : `git --git-dir cat-file -p <id>^{tree}` ou `rev-parse <id>^{tree}`. Adapter les helpers `run_git`/`run_git_owned` à un seul helper générique.)

- [ ] **Step 4 : GREEN** — remplir la valeur d'or de `project_key_is_stable_for_a_known_path` avec la vraie sortie ; `cargo test -p kaji --lib checkpoint::` vert ; `cargo test -p kaji --lib` = 8 préexistants inchangés.
- [ ] **Step 5 : fmt + clippy `-p kaji` + commit** — `kaji: checkpoint — store git bare par projet (snapshot/restore/diff, reverse-diff sûr, mutex)`.

### Task 2 : snapshot non-fatal au turn_start + event checkpoint + frontière message_id

**Files:**
- Modify: `crates/kaji/src/agents/agent.rs` (enveloppe reply() : snapshot + event checkpoint ; champ store optionnel)
- Modify: `crates/kaji/src/session/session_manager.rs` (ajouter `last_message_id`)
- Test: `mod tests` de agent.rs + session_manager.rs

**Interfaces:**
- Consumes : `CheckpointStore::{for_project, snapshot}` (T1), `SessionManager::append_event` (event log), `resolve_turn_seq` pattern (event log fix).
- Produces : event `checkpoint` dans session_events, payload `{ checkpoint_id, tree_sha, captured:"pre_turn", boundary_message_id }` ; `SessionManager::last_message_id(session_id) -> Result<Option<String>>` (consommé par T3).

- [ ] **Step 1 : tests RED** :

```rust
// session_manager.rs
#[tokio::test]
async fn last_message_id_returns_the_most_recent_persisted_message() {
    let mgr = /* SessionManager temp */; let sid = /* session */;
    assert!(mgr.last_message_id(&sid).await.unwrap().is_none());
    /* add_message m1, m2 */;
    assert_eq!(mgr.last_message_id(&sid).await.unwrap().as_deref(), Some("m2"));
}

// agent.rs (harness reply/condense existant)
#[tokio::test]
async fn a_completed_turn_logs_a_checkpoint_event_with_pre_turn_boundary() {
    let (agent, sid) = /* agent + provider mock + store temp */;
    let mut s = agent.reply(/* … */).await.unwrap();
    while let Some(_) = s.next().await {}
    let evs = agent.session_manager.events_for_session(&sid).await.unwrap();
    let cp = evs.iter().find(|e| e.kind == "checkpoint").expect("un event checkpoint");
    let v: serde_json::Value = serde_json::from_str(&cp.payload_json).unwrap();
    assert_eq!(v["captured"], "pre_turn");
    assert!(v["checkpoint_id"].is_string());
}

/// ⛔ BARRIÈRE premortem PM4 — un snapshot en échec ne doit JAMAIS avorter le
/// tour (répétition du Major next_turn_seq). Calqué sur
/// `a_failed_next_turn_seq_does_not_abort_the_turn`.
#[tokio::test]
async fn a_failed_snapshot_does_not_abort_the_turn() {
    // store pointé sur un git_dir non-inscriptible (ou snapshot stubé Err) →
    // le tour se termine normalement, le message user est persisté.
}
```

- [ ] **Step 2 : RED** — FAIL.
- [ ] **Step 3 : impl** :
  - `SessionManager::last_message_id` : `SELECT message_id FROM messages WHERE session_id=? ORDER BY created_timestamp DESC, id DESC LIMIT 1`.
  - `Agent` : champ `checkpoint_store: Option<Arc<CheckpointStore>>` (Option → les ~call sites de test sans store passent None ; construit dans le vrai chemin depuis `current_dir`). L'enveloppe reply() au turn_start, APRÈS le turn_start event : si `Some(store)`, calculer `boundary = session_manager.last_message_id(session_id).await.ok().flatten()`, puis `spawn_blocking` le `store.snapshot(project, &turn_label)` — **non-fatal** : sur `Ok((id, tree))` append event `checkpoint` ; sur `Err`, `warn!` et continuer (JAMAIS `?`). Doc-comment pointant PM4. `project` = `std::env::current_dir()` (best-effort, `warn!`+skip si Err).
  - ⚠ `spawn_blocking` car `git_command()` est sync et bloquerait l'executor (PM4 latence). Le snapshot ne doit pas retarder le premier octet plus que nécessaire — il tourne pendant que le tour démarre ; si l'ordonnancement l'exige, lancer le snapshot sans attendre son résultat pour yielder (mais alors l'event checkpoint est écrit quand le blocking finit — acceptable).
- [ ] **Step 4 : GREEN** — tests verts ; `cargo test -p kaji --lib` 8 préexistants inchangés.
- [ ] **Step 5 : fmt + clippy `-p kaji` + commit** — `kaji: agent — snapshot checkpoint non-fatal au turn_start (frontière message_id, event checkpoint)`.

### Task 3 : restore couplé atomique

**Files:**
- Create: `crates/kaji/src/checkpoint_restore.rs` (ou fn dans checkpoint.rs) — l'orchestration store × session
- Modify: `crates/kaji/src/lib.rs` si nouveau module
- Test: `mod tests` du module

**Interfaces:**
- Consumes : `CheckpointStore::{snapshot, restore}` (T1), `SessionManager::{truncate_conversation_from_message, events_for_session}` (frontière lue de l'event checkpoint).
- Produces : `async fn restore_checkpoint(store: &CheckpointStore, sm: &SessionManager, project: &Path, session_id: &str, target: &CheckpointId) -> Result<RestoreOutcome>` où `RestoreOutcome { restored_turn: i64 }`. Consommé par T4.

- [ ] **Step 1 : tests RED** :

```rust
#[tokio::test]
async fn restore_takes_a_pre_restore_snapshot_then_restores_tree_then_truncates() {
    // séquence heureuse : store + session avec un checkpoint(turn N, boundary=mX) ;
    // restore_checkpoint → un NOUVEAU event checkpoint label "pre-restore" existe,
    // l'arbre est au tree N, truncate_conversation_from_message(mX) a été appelé.
}

/// ⛔ BARRIÈRE premortem PM2 — restore couplé est tout-ou-rien. Si le git
/// restore réussit mais le truncate échoue, la fonction retourne Err et ne
/// prétend PAS "restauré". Ne JAMAIS rendre le truncate non-fatal.
#[tokio::test]
async fn restore_errors_when_truncation_fails_after_tree_restore() {
    // forcer truncate_conversation_from_message à Err (busy/table drop) →
    // restore_checkpoint retourne Err (pas Ok).
}

#[tokio::test]
async fn restore_refuses_coupling_when_boundary_message_id_is_null() {
    // checkpoint dont le payload a boundary_message_id = null → restore_checkpoint
    // retourne une erreur claire "frontière conversation absente", pas un truncate au hasard.
}
```

- [ ] **Step 2 : RED** — FAIL.
- [ ] **Step 3 : impl** — séquence de la spec, dans cet ordre exact :

```rust
pub async fn restore_checkpoint(store, sm, project, session_id, target) -> Result<RestoreOutcome> {
    // 1. undo-the-undo : snapshot AVANT toute mutation (PM2/PM5 récupérables)
    let _ = store.snapshot(project, "pre-restore"); // best-effort, warn si Err

    // frontière lue de l'event checkpoint ciblé
    let boundary = boundary_of(sm, session_id, target).await?; // Option<String> + turn_seq
    let Some(msg_id) = boundary.message_id else {
        bail!("restore couplé impossible : frontière conversation absente pour ce checkpoint");
    };

    // 2. git restore — échec = abandon, aucune mutation conversation
    store.restore(project, target).context("restore de l'arbre a échoué — conversation intacte")?;

    // 3. truncate — FATAL et bruyant (PM2). git a déjà écrit ; un échec ici = incohérent.
    sm.truncate_conversation_from_message(session_id, &msg_id).await
        .context("restore: troncature conversation échouée APRÈS restore de l'arbre — état incohérent, re-tenter /restore")?;

    Ok(RestoreOutcome { restored_turn: boundary.turn_seq })
}
```

(Doc-comment : « transaction logique — l'ordre 1→2→3 et le caractère fatal de l'étape 3 sont des invariants de sûreté, voir premortem PM2. »)

- [ ] **Step 4 : GREEN** — tests verts ; suite kaji --lib 8 préexistants.
- [ ] **Step 5 : fmt + clippy `-p kaji` + commit** — `kaji: checkpoint — restore couplé atomique (pre-restore snapshot, git puis truncate fatal)`.

### Task 4 : surface TUI — `/checkpoints`, `/restore <id>`, modal, garde

**Files:**
- Modify: `crates/kaji-cli/src/tui/app.rs` (COMMANDS, dispatch de `/restore <id>`, Action, garde turn_active)
- Modify: `crates/kaji-cli/src/tui/mod.rs` (handlers Action::Checkpoints / Action::Restore → appel des fns kaji)
- Test: `mod tests` de app.rs

**Interfaces:**
- Consumes : `restore_checkpoint` (T3), `SessionManager::events_for_session` (liste), `CheckpointStore` (T1).
- Produces : rien de nouveau.

- [ ] **Step 1 : tests RED** :

```rust
#[test]
fn slash_checkpoints_returns_the_list_action() {
    let mut app = App::new(None);
    for c in "/checkpoints".chars() { app.on_event(&key(KeyCode::Char(c))); }
    assert_eq!(app.on_event(&key(KeyCode::Enter)), Action::Checkpoints);
}

#[test]
fn slash_restore_with_id_parses_the_argument() {
    let mut app = App::new(None);
    for c in "/restore a1b2c3".chars() { app.on_event(&key(KeyCode::Char(c))); }
    assert_eq!(app.on_event(&key(KeyCode::Enter)), Action::Restore("a1b2c3".to_string()));
}

/// ⛔ BARRIÈRE premortem PM6 — /restore refusé pendant un tour actif (l'index
/// du store et l'arbre ne doivent pas bouger sous un tour).
#[test]
fn restore_is_refused_while_a_turn_is_active() {
    let mut app = App::new(None);
    app.turn_active = true;
    for c in "/restore a1b2c3".chars() { app.on_event(&key(KeyCode::Char(c))); }
    let action = app.on_event(&key(KeyCode::Enter));
    assert_eq!(action, Action::None);
    assert!(app.chat.iter().any(|l| l.text.contains("termine ou annule le tour")));
}
```

- [ ] **Step 2 : RED** — FAIL.
- [ ] **Step 3 : impl** :
  - `Action::Checkpoints`, `Action::Restore(String)` ajoutés à l'enum.
  - `/checkpoints` dans COMMANDS (arg-less) → `Action::Checkpoints`. `/restore` : PAS dans COMMANDS (a un argument) ; dans le handler Enter, AVANT le lookup COMMANDS : `if let Some(arg) = text.strip_prefix("/restore ") { if self.turn_active || self.turn_pending { self.push_system("termine ou annule le tour avant de restaurer"); return Action::None } return Action::Restore(arg.trim().to_string()) }`. (`/restore` seul sans arg → message d'aide « usage : /restore <id> ».)
  - mod.rs : `Action::Checkpoints` → lister via `events_for_session` filtré kind=checkpoint, `push_system_lines` (prompt-preview via `sanitize_for_display`). `Action::Restore(id)` → **ouvrir le modal y/n** (réutiliser le pattern tool_approval/gate) ; sur `y` → `restore_checkpoint(...)`, message « ⚠ restauré au tour N » ; sur `n` → annulé.
- [ ] **Step 4 : GREEN** — `cargo test -p kaji-cli` verts + pas de régression.
- [ ] **Step 5 : fmt + clippy `-p kaji-cli` + commit** — `kaji: tui — /checkpoints et /restore <id> (modal de confirmation, garde tour actif)`.

### Task 5 : E2E + review

- [ ] **Step 1** : `cargo test -p kaji --lib` (8 préexistants) + `cargo test -p kaji-cli` + clippy scoped kaji & kaji-cli, tous foreground.
- [ ] **Step 2** : E2E tmux — `kaji`, un tour qui crée un fichier, `/checkpoints` (le snapshot apparaît), un 2e tour qui modifie, `/restore <id du 1er>` → confirmer → vérifier en shell que le fichier est revenu ET que la conversation est tronquée. Documenter la sortie.
- [ ] **Step 3** : (contrôleur) review adversariale whole-diff (dimensions : sûreté git/restore, atomicité couplée, non-fatal snapshot, mapping frontière, tests-barrières intacts) → fix round → `just install` → push → roadmap.

---

## Self-review (à la rédaction)

Spec↔tasks : store bare + reverse-diff sûr + mutex + project_key figé → T1 (+ barrières PM5/PM7) ✓ ; snapshot non-fatal pre-turn + frontière message_id → T2 (+ barrière PM4) ✓ ; restore couplé atomique (pre-restore→git→truncate fatal) → T3 (+ barrières PM2, boundary null) ✓ ; surface TUI + modal + garde turn_active → T4 (+ barrière PM6) ✓ ; sémantique pre_turn testée (T1 séquence a/b/c + T2 captured:pre_turn) ✓ ; non-objectifs (dev+ino, gc, branches) absents ✓. Types cohérents T1→T4 (`CheckpointStore`, `CheckpointId`, `snapshot`/`restore`/`for_project`, `restore_checkpoint`, `last_message_id`, `Action::Restore(String)`). 6 tests-barrières mappés 1:1 sur les 7 post-mortems (PM1 sémantique, PM2 atomicité, PM3 boundary null, PM4 non-fatal, PM5 untracked, PM6 turn-active, PM7 project_key). Question ouverte résolue : `last_message_id` ajouté à SessionManager (T2). Placeholder restant assumé : valeur d'or `project_key` à remplir au 1er run (marqué explicitement).
