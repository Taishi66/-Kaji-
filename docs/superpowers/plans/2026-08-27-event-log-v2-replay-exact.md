# Event Log v2 — Replay Exact — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Toute session kaji devient rejouable exactement : la vraie boucle agent re-tourne, ses entrées non-déterministes (LLM, outils, bloc mémoire, horloge du prompt, décision de compaction) sont servies depuis le log `session_events`.

**Architecture:** Extension du log v1 par de nouveaux kinds capturés aux sites partagés des deux boucles (enveloppe `Agent::reply()`, `stream_response_from_provider`, `ExtensionManager::dispatch_tool_call`) ; au replay, un `ReplayProvider` et des intercepteurs servent les valeurs par clé (jamais positionnel), en mode strict par défaut, dans une session dérivée hermétique.

**Tech Stack:** Rust, sqlx/SQLite (sessions.db), sha2, clap (kaji-cli).

**Spec:** `docs/superpowers/specs/2026-08-27-event-log-v2-replay-exact-design.md` — autorité de référence ; les conflits du plan se résolvent contre elle.

## Décisions d'implémentation (rulings pris à l'écriture du plan, amendant la lettre de la spec)

1. **Pas de writer batché** (spec S2) : le volume venait du 1-INSERT-par-chunk ; en agrégeant `llm_response` par appel (chunks dans un seul payload), un tour n'ajoute que ~3-6 rows. On garde le `append_event` immédiat du v1 — zéro buffer ⇒ zéro perte partielle (pre-mortem 5 affaibli à la source). Le « flush par tour » devient sans objet.
2. **`clock_reads` minimal** (spec S1) : la seule lecture d'horloge qui entre dans le payload LLM est la date du prompt système (`PromptManager.current_date_timestamp`). `Message.created` n'est jamais sérialisé vers le provider : il est **exclu du `request_hash` par normalisation** (comme le fait déjà `TestProvider::hash_input`) au lieu d'être rejoué. Les autres `Utc::now()` (diagnostics, rage, imports, ts_ms du log) sont hors chemin replay.
3. **La session dérivée du replay n'écrit aucun événement** : ni dans le log source (spec), ni dans le sien (inutile en v2 — un replay ne se rejoue pas).

## Global Constraints

- `source bin/activate-hermit` avant tout cargo ; cargo **toujours foreground**, un seul à la fois (verrou `target/`).
- Formatage : `rustfmt <fichier>` (le `rustfmt.toml` du repo pinne `style_edition = "2021"`) ; **jamais** `cargo fmt` tant que des fichiers non commités d'une autre session traînent dans l'arbre.
- Erreurs : `anyhow::Result` ; pas de `.context()` qui ne dit rien de plus. Zéro commentaire qui paraphrase le code.
- **Non-fatal par construction** : aucune écriture de log ne fait échouer un tour vivant — `warn!` + `replayable = false`, jamais `?` vers l'enveloppe.
- **Adressage par clé** : `llm_response` par `(turn_seq, call_idx)` + `request_hash` ; `tool_result` par `tool_call_id` ; jamais de matching positionnel aveugle.
- **Replay strict par défaut** : clé absente ou hash divergent ⇒ arrêt bruyant avec diagnostic ; `--lenient` continue en signalant.
- **Hermétisme replay** : ingest/curation/checkpoint/usage_ledger/append_event tous désactivés en ReplayMode ; la session source n'est jamais modifiée.
- Parité agent-loop : toute capture vit dans un site partagé par les deux boucles (enveloppe, `reply_parts.rs`, `extension_manager.rs`) — jamais dupliquée dans `agents/agent.rs` ET `agents/state_machine/` (exception tolérée : 1 ligne identique par site, comme le splice mémoire).
- Kinds v1 (`turn_start`, `turn_end`, `message`, `usage`, `message_usage`, `mcp_notification`, `history_replaced`, `approval`, `checkpoint`) : intouchés. Kinds permanents jamais purgés ; seuls `llm_request`, `llm_response`, `tool_result`, `memory_block`, `clock_reads` sont purgeables.
- Constantes verrouillées : `replay_retention_days` défaut **30** ; schéma **v17**.
- Tests touchant l'env (`KAJI_STATE_MACHINE`, `KAJI_MEMORY_DIR`, `KAJI_PATH_ROOT`) : toujours `env_lock::lock_env` (pattern `crates/kaji/tests/kaji_memory_test.rs`) ; l'isolation mémoire de test passe par `kaji::isolate_test_memory_dir()` là où le pattern existe.
- Commits fréquents, messages conventionnels français (`feat(replay): …`), un commit par tâche minimum, `git add` par chemins explicites uniquement.
- Les chemins d'import des extraits sont indicatifs : vérifier les vrais noms au premier échec de compilation (regarder comment les tests existants du même crate importent).
- Échecs préexistants tolérés (ne pas toucher) : `tests/compaction.rs` ×2, `tests/providers.rs` ×2, `tests/tetrate_streaming.rs` (hang réseau).

---

### Task 1: IdGen déterministe + clippy gate

**Files:**
- Create: `crates/kaji/src/replay/mod.rs`, `crates/kaji/src/replay/idgen.rs`
- Modify: `crates/kaji/src/lib.rs` (déclarer `pub mod replay;`), `crates/kaji/src/agents/reply_parts.rs:593`, `crates/kaji/src/agents/state_machine/operation.rs:240`, `clippy.toml` (create at repo root)
- Test: `crates/kaji/tests/replay_idgen_test.rs`

**Interfaces:**
- Produces: `kaji::replay::idgen::{IdGen, SessionIdGen}` — `trait IdGen: Send + Sync { fn next_message_id(&self) -> String; }` ; `SessionIdGen::new(seed: &str) -> SessionIdGen` (compteur atomique interne, ids dérivés `msg_<sha256(seed:counter)[..32]>`) ; `kaji::replay::idgen::default_idgen() -> Arc<dyn IdGen>` (UUID aléatoire, comportement actuel).
- Consumed by: Task 5 (ids du stream), Task 9 (replay avec la même graine ⇒ mêmes ids).

- [ ] **Step 1: failing test**

```rust
// crates/kaji/tests/replay_idgen_test.rs
use kaji::replay::idgen::{IdGen, SessionIdGen};

#[test]
fn same_seed_yields_same_id_sequence() {
    let a = SessionIdGen::new("sess-1");
    let b = SessionIdGen::new("sess-1");
    let ids_a: Vec<_> = (0..3).map(|_| a.next_message_id()).collect();
    let ids_b: Vec<_> = (0..3).map(|_| b.next_message_id()).collect();
    assert_eq!(ids_a, ids_b);
    assert_eq!(ids_a.len(), 3);
    assert!(ids_a[0].starts_with("msg_"));
    assert_ne!(ids_a[0], ids_a[1]);
}

#[test]
fn different_seeds_diverge() {
    let a = SessionIdGen::new("sess-1");
    let b = SessionIdGen::new("sess-2");
    assert_ne!(a.next_message_id(), b.next_message_id());
}
```

- [ ] **Step 2: run** `cargo test -p kaji --test replay_idgen_test` — FAIL (module inexistant)
- [ ] **Step 3: implement**

```rust
// crates/kaji/src/replay/idgen.rs
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

pub trait IdGen: Send + Sync {
    fn next_message_id(&self) -> String;
}

pub struct SessionIdGen {
    seed: String,
    counter: AtomicU64,
}

impl SessionIdGen {
    pub fn new(seed: &str) -> Self {
        Self { seed: seed.to_string(), counter: AtomicU64::new(0) }
    }
}

impl IdGen for SessionIdGen {
    fn next_message_id(&self) -> String {
        let n = self.counter.fetch_add(1, Ordering::SeqCst);
        let mut h = Sha256::new();
        h.update(self.seed.as_bytes());
        h.update(b":");
        h.update(n.to_le_bytes());
        let hex = format!("{:x}", h.finalize());
        format!("msg_{}", &hex[..32])
    }
}

pub struct RandomIdGen;

impl IdGen for RandomIdGen {
    #[allow(clippy::disallowed_methods)]
    fn next_message_id(&self) -> String {
        format!("msg_{}", uuid::Uuid::new_v4())
    }
}

pub fn default_idgen() -> Arc<dyn IdGen> {
    Arc::new(RandomIdGen)
}
```

`crates/kaji/src/replay/mod.rs` : `pub mod idgen;`. Dans `lib.rs` : `pub mod replay;` (respecter l'ordre alphabétique des `pub mod` existants).

- [ ] **Step 4: brancher les 2 sites de boucle** — l'`Agent` porte un champ `idgen: Arc<dyn IdGen>` initialisé à `default_idgen()` (setter `set_idgen` pour le replay). `reply_parts.rs:593` et `operation.rs:240` reçoivent l'`Arc<dyn IdGen>` par paramètre depuis leur appelant (suivre le chemin d'appel réel ; si le threading de paramètre traverse plus de 2 fonctions, stocker l'`Arc` dans la structure porteuse la plus proche — `Operations` côté state machine). Remplacements :

```rust
// reply_parts.rs:593 (avant)
.get_or_insert_with(|| format!("msg_{}", uuid::Uuid::new_v4()))
// (après)
.get_or_insert_with(|| idgen.next_message_id())

// operation.rs:240 (avant)
message.id = Some(format!("msg_{}", uuid::Uuid::new_v4()));
// (après)
message.id = Some(idgen.next_message_id());
```

`kaji-provider-types/.../message.rs:1012-1015` (`with_generated_id`) reste intouché — hors boucle.

- [ ] **Step 5: clippy gate** — créer `clippy.toml` à la racine :

```toml
disallowed-methods = [
    { path = "uuid::Uuid::new_v4", reason = "boucle agent : passer par kaji::replay::idgen::IdGen (replay exact)" },
]
```

Poser `#[allow(clippy::disallowed_methods)]` sur chaque usage légitime restant HORS boucle agent dans les crates du workspace que clippy signale (les recenser dans le rapport ; la boucle agent — `agents/`, `reply_parts` — ne doit avoir aucun allow).

- [ ] **Step 6: run** `cargo test -p kaji --test replay_idgen_test` PASS, puis `cargo test -p kaji --lib` (aucun nouveau failure), `cargo clippy -p kaji --all-targets -- -D warnings`
- [ ] **Step 7: commit** `feat(replay): IdGen déterministe seedé, sites de boucle branchés, garde clippy sur Uuid::new_v4`

---

### Task 2: Clock du prompt + kind clock_reads

**Files:**
- Create: `crates/kaji/src/replay/clock.rs`
- Modify: `crates/kaji/src/replay/mod.rs`, `crates/kaji/src/agents/prompt_manager.rs` (~:225-235)
- Test: `crates/kaji/tests/replay_clock_test.rs`

**Interfaces:**
- Produces: `kaji::replay::clock::{PromptClock, RealClock, FixedClock}` — `trait PromptClock: Send + Sync { fn prompt_timestamp(&self) -> String; }` ; `RealClock` (formate `Utc::now().format("%Y-%m-%d %H:00 %:z")` — copie du format exact de `prompt_manager.rs:231`) ; `FixedClock::new(ts: String)`.
- `PromptManager::new_with_clock(clock: &dyn PromptClock) -> Self` ; `PromptManager::new()` délègue avec `RealClock`.
- Consumed by: Task 7 (enregistrement de la valeur), Task 10 (service au replay via `FixedClock`).

- [ ] **Step 1: failing test**

```rust
// crates/kaji/tests/replay_clock_test.rs
use kaji::replay::clock::{FixedClock, PromptClock, RealClock};

#[test]
fn fixed_clock_returns_its_value() {
    let c = FixedClock::new("2026-08-27 10:00 +02:00".to_string());
    assert_eq!(c.prompt_timestamp(), "2026-08-27 10:00 +02:00");
}

#[test]
fn real_clock_matches_prompt_format() {
    let ts = RealClock.prompt_timestamp();
    // "YYYY-MM-DD HH:00 +TZ" — 4-2-2 date, heure pilée à :00
    assert!(ts.len() >= 16, "{ts}");
    assert_eq!(&ts[4..5], "-");
    assert!(ts.contains(":00 "), "{ts}");
}
```

- [ ] **Step 2: run** `cargo test -p kaji --test replay_clock_test` — FAIL
- [ ] **Step 3: implement**

```rust
// crates/kaji/src/replay/clock.rs
use chrono::Utc;

pub trait PromptClock: Send + Sync {
    fn prompt_timestamp(&self) -> String;
}

pub struct RealClock;

impl PromptClock for RealClock {
    #[allow(clippy::disallowed_methods)]
    fn prompt_timestamp(&self) -> String {
        Utc::now().format("%Y-%m-%d %H:00 %:z").to_string()
    }
}

pub struct FixedClock(String);

impl FixedClock {
    pub fn new(ts: String) -> Self {
        Self(ts)
    }
}

impl PromptClock for FixedClock {
    fn prompt_timestamp(&self) -> String {
        self.0.clone()
    }
}
```

Dans `prompt_manager.rs` : `PromptManager::new_with_clock(clock: &dyn PromptClock)` initialise `current_date_timestamp: clock.prompt_timestamp()` ; `new()` = `Self::new_with_clock(&RealClock)`. Le `#[cfg(test)] with_timestamp` existant reste.

- [ ] **Step 4: étendre clippy.toml** — ajouter `chrono::Utc::now` aux `disallowed-methods` scope kaji ; poser les `#[allow]` sur les sites légitimes hors chemin replay (`session_manager.rs:537,1748,2549`, `diagnostics.rs`, `rage.rs`, `schedule_tool.rs:403`, `import_formats/*`) avec la raison en un mot. Aucun allow dans `prompt_manager.rs`.
- [ ] **Step 5: run** tests + `cargo clippy -p kaji --all-targets -- -D warnings`
- [ ] **Step 6: commit** `feat(replay): PromptClock injecté dans PromptManager, garde clippy sur Utc::now`

---

### Task 3: Migration v17 — UNIQUE turn_seq, replayable, log_meta

**Files:**
- Modify: `crates/kaji/src/session/session_manager.rs` (`CURRENT_SCHEMA_VERSION` :27 → 17 ; `apply_migration` :1708+ ; `next_turn_seq` storage :2557-2564 ; `get_session` SELECT :1786-1795)
- Test: `crates/kaji/tests/replay_schema_test.rs`

**Interfaces:**
- Produces: colonne `sessions.replayable INTEGER NOT NULL DEFAULT 1` (+ champ `Session.replayable: bool`) ; `SessionManager::mark_not_replayable(&self, session_id: &str) -> Result<()>` ; index `UNIQUE(session_id, turn_seq)` sur l'allocation ; kind `log_meta` (payload `{"kaji_version":"...","schema_version":17,"idgen_seed":"<session_id>"}`) émis par l'enveloppe au tour 1 (Task 5 l'écrit ; cette tâche fournit le helper `SessionManager::append_log_meta_if_absent(&self, session_id: &str) -> Result<()>`).
- `next_turn_seq` devient transactionnel : `BEGIN IMMEDIATE` + `SELECT MAX` + retour, dans une seule transaction sqlx.

- [ ] **Step 1: failing tests**

```rust
// crates/kaji/tests/replay_schema_test.rs — s'appuyer sur le harness kaji-test-support
// (voir crates/kaji-test-support/src/session.rs pour créer un SessionManager temporaire)
#[tokio::test]
async fn log_meta_written_once() {
    let (mgr, session) = kaji_test_support::session::temp_session().await;
    mgr.append_log_meta_if_absent(&session.id).await.unwrap();
    mgr.append_log_meta_if_absent(&session.id).await.unwrap();
    let events = mgr.session_events(&session.id).await.unwrap();
    assert_eq!(events.iter().filter(|e| e.kind == "log_meta").count(), 1);
}

#[tokio::test]
async fn mark_not_replayable_flips_flag() {
    let (mgr, session) = kaji_test_support::session::temp_session().await;
    assert!(mgr.get_session(&session.id, false).await.unwrap().replayable);
    mgr.mark_not_replayable(&session.id).await.unwrap();
    assert!(!mgr.get_session(&session.id, false).await.unwrap().replayable);
}
```

Si `kaji_test_support::session::temp_session` n'existe pas sous ce nom, utiliser le helper réel du crate (le lire) ; s'il n'y a pas de lecteur d'événements public, ajouter `SessionManager::session_events(&self, session_id: &str) -> Result<Vec<SessionEvent>>` (SELECT ordonné par `turn_seq, id`) — Task 9 en a besoin de toute façon.

- [ ] **Step 2: run** — FAIL (colonne/méthodes absentes)
- [ ] **Step 3: implement** — migration `17 =>` sur le pattern verbatim du bloc `16 =>` (`apply_migration`) :

```rust
17 => {
    sqlx::query("ALTER TABLE sessions ADD COLUMN replayable INTEGER NOT NULL DEFAULT 1")
        .execute(&mut **tx).await?;
    sqlx::query("CREATE UNIQUE INDEX IF NOT EXISTS idx_session_events_turn_alloc ON session_events(session_id, turn_seq, kind, id)")
        .execute(&mut **tx).await?;
}
```

⚠ L'unicité stricte `(session_id, turn_seq)` seule est FAUSSE (plusieurs events par tour) : l'invariant à garantir est l'allocation de tour, pas les rows. Implémenter l'allocation transactionnelle dans `next_turn_seq` (une transaction `BEGIN IMMEDIATE` autour du `SELECT MAX(turn_seq)+1` + l'insertion du `turn_start` par l'appelant reste hors transaction — documenter dans le rapport que la course résiduelle inter-process est fermée par le lock d'écriture SQLite pris par `BEGIN IMMEDIATE`). L'index ci-dessus reste non-unique dans ce cas : le remplacer par un index simple si déjà couvert par `idx_session_events_session` — trancher à l'implémentation et le justifier au rapport.

`mark_not_replayable` : `UPDATE sessions SET replayable = 0 WHERE id = ?`. `get_session` : ajouter `replayable` au SELECT + au struct.

- [ ] **Step 4: run** tests PASS + `cargo test -p kaji --lib` (migrations rejouées sur bases de test)
- [ ] **Step 5: commit** `feat(replay): schéma v17 — replayable, allocation de tour transactionnelle, log_meta`

---

### Task 4: RecordSink — écriture non-fatale des kinds v2

**Files:**
- Create: `crates/kaji/src/replay/record.rs`
- Modify: `crates/kaji/src/replay/mod.rs`
- Test: `crates/kaji/tests/replay_record_test.rs`

**Interfaces:**
- Produces: `kaji::replay::record::RecordSink` — `RecordSink::new(session_manager: Arc<SessionManager>, session_id: String) -> Self` ; méthodes **toutes `async` et non-fatales** (échec ⇒ `tracing::warn!` + `mark_not_replayable`, retour `()`) :
  - `record_llm_request(&self, turn_seq: i64, call_idx: u32, request_hash: &str, model: &str, provider: &str)`
  - `record_llm_response(&self, turn_seq: i64, call_idx: u32, chunks_json: &str, finish: &str)`
  - `record_tool_result(&self, turn_seq: i64, tool_call_id: &str, result_json: &str)`
  - `record_memory_block(&self, turn_seq: i64, block: &str)`
  - `record_clock_reads(&self, turn_seq: i64, reads: &[String])`
  - `record_condense_triggered(&self, turn_seq: i64, reason: &str)`
- Payloads JSON : `{"turn_seq":N,"call_idx":N,"request_hash":"...","model":"...","provider":"..."}` etc. — clés d'adressage TOUJOURS dans le payload (le `turn_seq` colonne sert l'ordre, le payload sert le matching).
- Consumed by: Tasks 5, 6, 7.

- [ ] **Step 1: failing test** — un `RecordSink` sur session temp : `record_tool_result` écrit un row `tool_result` lisible via `session_events()` avec le bon payload ; après fermeture du pool (simuler l'échec en droppant la DB file en lecture seule ou en fermant le pool), un record supplémentaire ne panique pas et la session est marquée `replayable = false`.

```rust
#[tokio::test]
async fn tool_result_roundtrip_and_nonfatal_failure() {
    let (mgr, session) = kaji_test_support::session::temp_session().await;
    let sink = RecordSink::new(mgr.clone(), session.id.clone());
    sink.record_tool_result(1, "call_42", r#"{"ok":true}"#).await;
    let events = mgr.session_events(&session.id).await.unwrap();
    let ev = events.iter().find(|e| e.kind == "tool_result").unwrap();
    assert!(ev.payload_json.contains("call_42"));
}
```

- [ ] **Step 2: run** — FAIL
- [ ] **Step 3: implement** — chaque méthode construit le payload `serde_json::json!`, appelle `session_manager.append_event(...)` ; sur `Err` : `warn!(error = %e, kind, "event log v2: écriture échouée — session marquée non rejouable")` + `mark_not_replayable` (dont l'échec est lui-même warn-only).
- [ ] **Step 4: run** PASS + `cargo test -p kaji --lib`
- [ ] **Step 5: commit** `feat(replay): RecordSink non-fatal pour les kinds v2`

---

### Task 5: Capture LLM — llm_request / llm_response + log_meta au tour 1

**Files:**
- Create: `crates/kaji/src/replay/hashing.rs` (normalisation + hash de requête)
- Modify: `crates/kaji/src/agents/reply_parts.rs` (`stream_response_from_provider` :426-434 et son corps :477-606), `crates/kaji/src/agents/agent.rs` (enveloppe :2268-2381 — émettre `log_meta` via `append_log_meta_if_absent` juste avant `turn_start`)
- Test: `crates/kaji/tests/replay_capture_test.rs`

**Interfaces:**
- Produces: `kaji::replay::hashing::request_hash(system: &str, messages: &[Message], tools: &[Tool]) -> String` — sérialise une forme **normalisée** (ids et `created` des messages remplacés par `""`/`0` ; même esprit que `TestProvider::hash_input`, `testprovider.rs:78-118` — lire et réutiliser sa logique de strip si extractible, sinon la dupliquer dans `hashing.rs` avec un test de non-régression croisé) ; SHA-256 hex.
- `stream_response_from_provider` gagne deux paramètres : `record: Option<&RecordSink>` et `call_ctx: Option<(i64 /*turn_seq*/, u32 /*call_idx*/)>` — `None` = comportement actuel intact (tous les appelants hors boucle passent `None`).
- Compteur `call_idx` : possédé par l'appelant de boucle (un `AtomicU32` remis à zéro par tour, porté par la structure de tour de chaque boucle — 1 ligne par boucle, identique, pattern splice).
- Consumed by: Task 9 (vérification du hash, service des chunks).

- [ ] **Step 1: failing test** — via `TestProvider` en mode recording branché dans un `Agent` de test (réutiliser le harness des tests d'agent existants, `agent.rs:6305+` montre comment) : après un `reply()` complet, le log contient exactement 1 `log_meta`, ≥1 `llm_request` avec un `request_hash` de 64 hex, et 1 `llm_response` par `llm_request` dont le payload contient les chunks ordonnés ; deux appels dans le même tour portent `call_idx` 0 puis 1.
- [ ] **Step 2: run** — FAIL
- [ ] **Step 3: implement** — dans le corps de `stream_response_from_provider` : calcul du hash + `record_llm_request` avant `provider.stream(...)` ; accumulation des chunks `(Option<Message>, Option<ProviderUsage>)` sérialisés dans un `Vec` pendant le re-yield du `try_stream!` ; `record_llm_response` à l'épuisement du stream (et sur erreur : payload `{"error":"..."}` — le replay strict s'arrêtera dessus). Enveloppe : `append_log_meta_if_absent` avant le `turn_start` (payload avec `idgen_seed = session_id`, version kaji via `env!("CARGO_PKG_VERSION")`).
- [ ] **Step 4: run** PASS + `cargo test -p kaji --lib` + parité : le test s'exécute sous les deux boucles (`env_lock` + `KAJI_STATE_MACHINE=1`) avec les mêmes asserts.
- [ ] **Step 5: commit** `feat(replay): capture llm_request/llm_response au site partagé, log_meta en tête de session`

---

### Task 6: Capture tool_result au site partagé

**Files:**
- Modify: `crates/kaji/src/agents/extension_manager.rs` (`dispatch_tool_call` :1861+)
- Test: étendre `crates/kaji/tests/replay_capture_test.rs`

**Interfaces:**
- `ExtensionManager::dispatch_tool_call` reçoit le contexte existant `ToolCallContext` (`tool_execution.rs:36-41`, champ `tool_call_request_id: Option<String>`) — ajouter au struct un champ `record: Option<Arc<RecordSink>>` + `turn_seq: Option<i64>` (ou les porter par un nouveau paramètre si le struct est construit à trop d'endroits — trancher à l'implémentation, justifier au rapport). Après l'exécution, si `record` et `tool_call_request_id` sont présents : `record_tool_result(turn_seq, id, result_json)` où `result_json` sérialise le `ToolCallResult` complet (Ok comme Err — les erreurs d'outil font partie du replay).
- Consumed by: Task 10.

- [ ] **Step 1: failing test** — session de test avec un outil fixture (harness MCP de `kaji-test-support`) : après `reply()`, le log contient 1 `tool_result` par appel d'outil, payload avec le `tool_call_id` exact du `ToolRequest` correspondant (croiser avec les rows `message` du log v1).
- [ ] **Step 2: run** — FAIL
- [ ] **Step 3: implement** (capture au retour du dispatch, avant les post-hooks pour enregistrer le résultat brut de l'outil)
- [ ] **Step 4: run** PASS sous les deux boucles + `cargo test -p kaji --lib`
- [ ] **Step 5: commit** `feat(replay): capture tool_result dans ExtensionManager::dispatch_tool_call`

---

### Task 7: Capture memory_block, clock_reads, condense_triggered

**Files:**
- Modify: `crates/kaji/src/kaji.rs` (autour de `splice_memory_block`), `crates/kaji/src/agents/agent.rs:1018-1032`, `crates/kaji/src/agents/state_machine/ops_llm.rs:455-473`, `crates/kaji/src/context_mgmt/mod.rs` (site de déclenchement de la compaction — le localiser : grep `condense`/seuil), `crates/kaji/src/agents/prompt_manager.rs` (exposer la valeur servie pour l'enregistrement)
- Test: étendre `crates/kaji/tests/replay_capture_test.rs`

**Interfaces:**
- `splice_memory_block` retourne désormais `(String /*prompt*/, Option<String> /*bloc splicé verbatim, None si vide*/)` — les 2 sites de boucle passent le bloc au `RecordSink` (`record_memory_block`), 1 ligne identique par site (pattern établi).
- `record_clock_reads(turn_seq, &[prompt_timestamp])` — émis par l'enveloppe (elle a accès au timestamp via le `PromptManager` du tour ; si l'accès n'existe pas, l'enregistrer depuis le site qui construit le system prompt).
- `record_condense_triggered(turn_seq, reason)` au site où la compaction se décide (avant l'appel LLM de résumé).
- Consumed by: Task 10.

- [ ] **Step 1: failing test** — session avec faits mémoire pré-écrits (pattern `kaji_memory_test.rs`) : le log contient un `memory_block` dont le payload est verbatim le bloc splicé ; un `clock_reads` par tour ; test compaction : forcer le seuil bas (env/config du condense) ⇒ `condense_triggered` présent.
- [ ] **Step 2: run** — FAIL
- [ ] **Step 3: implement**
- [ ] **Step 4: run** PASS sous les deux boucles + suite `kaji_memory_test` intacte
- [ ] **Step 5: commit** `feat(replay): capture memory_block, clock_reads et condense_triggered`

---

### Task 8: ReplayMode — hermétisme de l'enveloppe

**Files:**
- Create: `crates/kaji/src/replay/mode.rs` (`pub struct ReplayMode { pub source_session_id: String, pub lenient: bool, pub until_turn: Option<i64> }`)
- Modify: `crates/kaji/src/agents/agent.rs` (champ `replay_mode: Option<ReplayMode>` + setter ; gates dans l'enveloppe :2268-2381 et `snapshot_checkpoint` :2210-2252), `crates/kaji/src/agents/agent.rs:1018-1032` + `ops_llm.rs:455-473` (gate ingest/curation), `reply_parts.rs:790-796` + `state_machine/usage.rs:71` (gate `record_usage_metrics`)
- Test: `crates/kaji/tests/replay_hermetic_test.rs`

**Interfaces:**
- Produces: `Agent::set_replay_mode(ReplayMode)` ; helper interne `Agent::is_replay(&self) -> bool`.
- En ReplayMode : `append_event`/`append_log_meta_if_absent` non appelés, `snapshot_checkpoint` non appelé, `ingest_turn`/`maybe_spawn_curation` non appelés, `record_usage_metrics` non appelé, `RecordSink` absent (pas de capture).
- Le gate ingest/curation vit dans `crate::kaji` (une garde `if replay { return }` dans `ingest_turn` et `maybe_spawn_curation` via un paramètre ou un flag thread-local ? NON — paramètre explicite) : les 2 sites de boucle testent `self.is_replay()` avant d'appeler — 1 condition identique par site, pattern splice.
- Consumed by: Tasks 9-11.

- [ ] **Step 1: failing test (hermétisme)**

```rust
// replay_hermetic_test.rs — l'Agent en ReplayMode fait un reply() complet (provider fixture) ;
// asserts : zéro nouvel event dans session_events de la session, zéro row usage_ledger,
// zéro entrée nouvelle dans le journal mémoire (Memory::uncurated inchangé),
// zéro checkpoint. Snapshot des 4 états avant/après, comparaison stricte.
```

- [ ] **Step 2: run** — FAIL
- [ ] **Step 3: implement** — chaque gate est un `if self.replay_mode.is_none()` autour de l'appel existant, jamais une suppression.
- [ ] **Step 4: run** PASS sous les deux boucles + `cargo test -p kaji --lib`
- [ ] **Step 5: commit** `feat(replay): ReplayMode — enveloppe hermétique (log, checkpoints, mémoire, usage)`

---

### Task 9: EventCursor + ReplayProvider

**Files:**
- Create: `crates/kaji/src/replay/cursor.rs`, `crates/kaji/src/replay/provider.rs`
- Modify: `crates/kaji/src/replay/mod.rs`
- Test: `crates/kaji/tests/replay_provider_test.rs`

**Interfaces:**
- `EventCursor::load(session_manager: &SessionManager, session_id: &str) -> anyhow::Result<EventCursor>` — charge et indexe les events v2 : `llm_responses: HashMap<(i64, u32), LlmExchange>` (exchange = request_hash + chunks + finish), `tool_results: HashMap<String, String>`, `memory_blocks: HashMap<i64, String>`, `clock_reads: HashMap<i64, Vec<String>>`, `condense_turns: HashSet<i64>`, `log_meta: Option<LogMeta>`. Erreurs typées : `ReplayUnavailable::PreV2` (pas de `log_meta`), `ReplayUnavailable::Purged` (`replayable == false`), `ReplayUnavailable::TruncatedAt(turn)` (dernier tour sans `turn_end` — réutiliser la logique `last_turn_is_interrupted`).
- `ReplayProvider::new(cursor: Arc<EventCursor>, lenient: bool) -> ReplayProvider` — implémente `Provider` (comme `TestProvider` : `get_name` + `stream` seulement, `testprovider.rs:168-219`). `stream()` : recalcule `request_hash` sur ses arguments, retrouve l'échange courant par le compteur `(turn_seq, call_idx)` partagé avec l'appelant (le provider reçoit turn/idx via le même mécanisme que la capture Task 5 — champ interne alimenté par `set_position` ou lecture d'un `Arc<AtomicI64>` partagé ; trancher à l'implémentation pour rester compatible avec la signature `stream(&self, ...)`), compare les hashes : mismatch en strict ⇒ `ProviderError` explicite portant les deux hashes + le tour ; en lenient ⇒ `warn!` + servir quand même. Sert les chunks enregistrés dans l'ordre via un `MessageStream` reconstruit.
- Consumed by: Task 11.

- [ ] **Step 1: failing test** — enregistrer une session fixture (2 tours, 1 outil) avec le vrai pipeline des Tasks 5-7, puis : `EventCursor::load` retrouve tous les kinds ; `ReplayProvider::stream` avec les bons arguments re-produit exactement les chunks enregistrés ; avec un system prompt altéré ⇒ `Err` en strict, `Ok` + warn en lenient ; log tronqué (DELETE du dernier `turn_end` à la main dans le test) ⇒ `TruncatedAt`.
- [ ] **Step 2: run** — FAIL
- [ ] **Step 3: implement**
- [ ] **Step 4: run** PASS + `cargo test -p kaji --lib`
- [ ] **Step 5: commit** `feat(replay): EventCursor adressé par clé + ReplayProvider strict/lenient`

---

### Task 10: Intercepteurs replay — outils, mémoire, horloge, compaction

**Files:**
- Modify: `crates/kaji/src/agents/extension_manager.rs` (`dispatch_tool_call` — branche replay), `crates/kaji/src/agents/agent.rs` + `ops_llm.rs` (splice servi depuis le cursor en ReplayMode), `crates/kaji/src/agents/prompt_manager.rs` (FixedClock branchée), site condense (suivre `condense_triggered`)
- Test: `crates/kaji/tests/replay_intercept_test.rs`

**Interfaces:**
- `ToolCallContext` gagne `replay_cursor: Option<Arc<EventCursor>>` : présent ⇒ `dispatch_tool_call` retourne `tool_results[tool_call_id]` désérialisé **sans exécuter l'outil** ; clé absente ⇒ `Err(ErrorData)` explicite « replay: tool_result absent pour <id> — log tronqué ou divergent » (strict) / warn + exécution refusée quand même en lenient (jamais d'exécution réelle au replay — décision spec).
- Bloc mémoire : en ReplayMode les sites de boucle utilisent `cursor.memory_blocks[turn_seq]` au lieu de `splice_memory_block` (absent ⇒ pas de bloc, comme à l'enregistrement).
- Horloge : `PromptManager::new_with_clock(&FixedClock::new(cursor.clock_reads[turn]...))`.
- Compaction : en ReplayMode le déclencheur suit `cursor.condense_turns.contains(turn)` au lieu du calcul de seuil.
- Approbations (spec S4) : en ReplayMode, aucune confirmation n'est jamais demandée à l'utilisateur — le chemin de confirmation d'outil (`ToolConfirmationRouter`) est court-circuité en suivant les rows `approval` v1 du log : outil approuvé à l'enregistrement ⇒ le replay sert son `tool_result` ; outil refusé ⇒ pas de `tool_result` dans le log ET une row `approval` deny — le replay rejoue le refus (le message de refus est déjà dans les rows `message` v1). Implémentation minimale défendable : en ReplayMode le routeur répond automatiquement selon la row `approval` du tour (deny par défaut si absente) ; ajouter `approvals: HashMap<(i64, String), bool>` à l'`EventCursor` (Task 9, clé = (turn_seq, tool id ou nom selon le payload réel des rows approval — lire `log_approval_event`, `agent.rs:1817-1858`, pour la forme exacte).
- Consumed by: Task 11.

- [ ] **Step 1: failing test** — session fixture enregistrée avec 1 outil : replay ⇒ le résultat d'outil est celui du log (l'outil fixture est instrumenté pour paniquer s'il est réellement appelé pendant le replay) ; suppression du `tool_result` du log ⇒ erreur explicite ; le prompt du replay contient le `memory_block` et le timestamp enregistrés.
- [ ] **Step 2: run** — FAIL
- [ ] **Step 3: implement**
- [ ] **Step 4: run** PASS sous les deux boucles + `cargo test -p kaji --lib`
- [ ] **Step 5: commit** `feat(replay): intercepteurs outils/mémoire/horloge/compaction servis depuis le log`

---

### Task 11: CLI kaji replay

**Files:**
- Modify: `crates/kaji-cli/src/cli.rs` (nouvelle variante de commande sur le pattern `Memory` :1070-1075, dispatch :2440, handler)
- Create: `crates/kaji-cli/src/commands/replay.rs`
- Test: `crates/kaji-cli/src/commands/replay.rs` (module `#[cfg(test)]` pour les parties pures) + test d'intégration léger si le harness CLI existant le permet

**Interfaces:**

```rust
/// Rejoue exactement une session enregistrée (event log v2)
#[command(about = "Replay a recorded session exactly from its event log")]
Replay {
    /// Session à rejouer
    session_id: String,
    /// S'arrêter après le tour N
    #[arg(long)]
    until: Option<i64>,
    /// Continuer sur divergence (signalée) au lieu d'échouer
    #[arg(long)]
    lenient: bool,
},
```

- `handle_replay_subcommand` : charge `EventCursor` (les erreurs `ReplayUnavailable` deviennent des messages humains : « session enregistrée avant le replay v2 », « payloads purgés (rétention N j) », « log tronqué au tour N — replay jusqu'au tour N-1 possible avec --until ») ; crée la session dérivée `SessionManager::create_session(working_dir_source, format!("replay-of-{session_id}"), SessionType::Hidden, mode)` + `parent_session_id` via le builder ; construit l'Agent : `ReplayProvider`, `set_replay_mode`, `SessionIdGen::new(&log_meta.idgen_seed)`, `FixedClock` ; rejoue tour par tour en réinjectant les messages user du log v1 (rows `message` de rôle user, par `turn_seq`) ; imprime la transcription (tour, rôle, contenu tronqué à 200 chars, outils, divergences lenient) ; codes de sortie : 0 ok, 2 divergence (strict), 3 tronqué, 4 non disponible.
- Le message user de chaque tour vient du log (kind `message`, payload v1 — lire `agent_event_payload` `agent.rs:330-359` pour le format exact).

- [ ] **Step 1: failing test** — sur les parties pures : mapping `ReplayUnavailable` → message + code de sortie ; extraction des messages user par tour depuis une liste de `SessionEvent` fixture.
- [ ] **Step 2: run** — FAIL
- [ ] **Step 3: implement**
- [ ] **Step 4: run** `cargo test -p kaji-cli` + `cargo build -p kaji-cli`
- [ ] **Step 5: commit** `feat(replay): commande kaji replay — strict par défaut, codes de sortie dédiés`

---

### Task 12: Rétention par kind

**Files:**
- Modify: `crates/kaji/src/session/session_manager.rs` (purge au même point d'entrée que `run_migrations` — au boot du manager)
- Test: `crates/kaji/tests/replay_retention_test.rs`

**Interfaces:**
- `SessionManager::purge_replay_payloads(&self, retention_days: i64) -> Result<u64>` :

```sql
DELETE FROM session_events
WHERE kind IN ('llm_request','llm_response','tool_result','memory_block','clock_reads')
  AND ts_ms < :cutoff_ms
```

puis `UPDATE sessions SET replayable = 0 WHERE id IN (SELECT DISTINCT session_id …)` pour les sessions touchées. Appelée au boot avec `config.get_param::<i64>("KAJI_REPLAY_RETENTION_DAYS").unwrap_or(30)` (pattern `get_param`, `config/base.rs:770-787`). `0` = purge immédiate de tout ; valeur négative = jamais purger (documenter dans le help).
- Les kinds permanents (`turn_*`, `message`, `usage`, `message_usage`, `approval`, `checkpoint`, `mcp_notification`, `history_replaced`, `log_meta`, `condense_triggered`) ne sont **jamais** dans la clause IN — `condense_triggered` est petit et structurel : permanent.

- [ ] **Step 1: failing test** — events v2 antidatés (UPDATE ts_ms à la main) : après purge, les kinds purgeables anciens ont disparu, les kinds permanents du même tour restent, la session est `replayable = false` ; une session récente est intacte et `replayable = true` ; `kaji replay` sur la purgée répond « payloads purgés ».
- [ ] **Step 2: run** — FAIL
- [ ] **Step 3: implement**
- [ ] **Step 4: run** PASS + `cargo test -p kaji --lib`
- [ ] **Step 5: commit** `feat(replay): rétention par kind, replayable=false sur purge`

---

### Task 13: Tests dorés end-to-end + gate final

**Files:**
- Create: `crates/kaji/tests/replay_golden_test.rs`
- Modify: `kaji-self-test.yaml` (règle AGENTS.md — nouvelle phase)
- Test: c'est la tâche de test.

**Steps:**

- [ ] **Step 1: test doré** — avec provider fixture et outil fixture : enregistrer une session synthétique de 3 tours (dont 1 appel d'outil et des faits mémoire), puis `replay` ×2 par l'API (pas le binaire) ; asserts : les deux transcriptions structurées (séquence de `(turn, kind, clé, contenu)`) sont **identiques entre elles** ; les ids de messages des deux replays sont identiques (IdGen seedé) ; test exécuté sous les deux boucles (`env_lock` + `KAJI_STATE_MACHINE`).
- [ ] **Step 2: test parité d'enregistrement** — même scénario enregistré sous legacy puis sous state machine : même séquence de kinds v2, mêmes clés (`call_idx`, `tool_call_id` à normalisation d'id près — comparer les formes).
- [ ] **Step 3: test strict-mismatch de bout en bout** — replay avec un fait mémoire ajouté après l'enregistrement : sans intercepteur ce serait une divergence ; vérifier que le `memory_block` servi depuis le log rend le replay identique (le test PROUVE la mitigation du pre-mortem 3).
- [ ] **Step 3bis: AGENTS.md — règle d'extension (spec S1)** — ajouter sous la section Rules une ligne : « Replay v2 : toute nouvelle source d'état externe entrant dans le prompt (hints, extensions, splices futurs) exige son kind d'événement dans session_events + son service au replay — sinon le replay diverge silencieusement (voir spec 2026-08-27). »
- [ ] **Step 4: kaji-self-test.yaml** — ajouter une phase shell minimale : `kaji replay <session-inexistante>` doit échouer avec le message « introuvable » (exit ≠ 0), et `kaji replay --help` sort 0 — le self-test valide la surface CLI, pas le replay complet (qui exige une session enregistrée par le même binaire — noté comme vérification manuelle).
- [ ] **Step 5: gate final** — `cargo build` ; `cargo test -p kaji-core -p kaji -p kaji-cli -p kaji-mcp` (échecs préexistants listés en Global Constraints tolérés, zéro NOUVEAU failure) ; `cargo clippy --all-targets -- -D warnings` (si les fichiers foreign non commités cassent all-targets : clippy scoped par crate du plan + le signaler).
- [ ] **Step 6: commit** `test(replay): dorés record→replay×2, parité, hermétisme P1×P2 ; self-test CLI`
