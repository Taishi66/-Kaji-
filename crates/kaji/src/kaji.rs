//! KAJI integration bridge : links the kaji-core memory store to the kaji
//! kernel without touching the two agent-loop hot paths.
//!
//! Scope (ADR-03 wiring milestone):
//! - **Cross-session persistence**: one shared store under the kaji data dir,
//!   entries tagged with their source `session_id` (cross-session recall
//!   survives process restarts and spans sessions).
//! - **Ingestion**: snapshot current session messages into memory via
//!   `ingest_turn` (callers pass the text they want recalled later).
//! - **Recall**: ranked, zero-token retrieval of prior facts for a prompt —
//!   global across sessions, with per-session temporal anchoring.
//! - **Prompt block**: `recall_prompt` renders the recalled facts + their
//!   anchored context into a plain-text block ready to splice into a system
//!   prompt. Still only rendered on demand — callers decide when to inject.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Once};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kaji_core::facts::{slugify, CreatedBy, Fact, FactIndex, FactStore, FactType};
use kaji_core::memory::{Anchored, Entry, Memory, RecallHit, RecallResult};
use kaji_providers::model::ModelConfig;

pub use kaji_core::memory::Entry as MemoryEntry;

use crate::config::paths::{find_git_root, Paths};
use crate::providers::base::Provider;
use crate::session::redact_text;

/// Rendered prefix for the memory block in a system prompt.
const BLOCK_HEADER: &str = "## KAJI memory — recalled across sessions";

/// Rendered prefix for the curated facts, spliced above the raw journal.
const FACTS_HEADER: &str = "## Faits mémorisés";

/// Curated facts injected per turn. Kept small on purpose: a fact is dense and
/// competes with the raw journal for the same prompt budget.
const FACTS_TOP_K: usize = 3;

/// Longest fact body rendered in the block. Facts are meant to be read in full;
/// the cap only guards against a curator writing an essay.
const FACT_BODY_MAX_CHARS: usize = 300;

/// Largest body stored as a memory fact. Payloads above this are kept in the
/// conversation only, so recalling a giant message can't balloon every future
/// system prompt (or exceed the model's context limit).
const MAX_FACT_LEN: usize = 4_000;

/// File (relative to the memory dir) holding the shared cross-session store.
const SHARED_FILE: &str = "shared.db";

/// Dir holding the shared store. Tests override `KAJI_MEMORY_DIR` for
/// isolation (the state-machine and bridge tests must not touch the real
/// user store); otherwise the standard data dir is used.
fn memory_dir() -> std::path::PathBuf {
    if let Some(dir) = std::env::var_os("KAJI_MEMORY_DIR") {
        return dir.into();
    }
    Paths::in_data_dir("kaji/memory")
}

/// Points `KAJI_MEMORY_DIR` at a throwaway root, once for the whole test
/// binary, so no in-crate test ever reads or writes the real user store.
/// Shared by every module that needs this isolation (`context_report`,
/// the state-machine `pipeline` test harness, ...) so there is exactly one
/// writer of the variable — `Once` serializes the racing callers itself, so
/// this doesn't need (and must not take) `env_lock`: state-machine tests
/// routinely build several independent pipelines per test via `let`
/// shadowing, or already hold their own `env_lock` guard before reaching
/// this call, and `env_lock`'s mutex isn't reentrant — holding it for a
/// whole pipeline's lifetime self-deadlocks those callers.
#[cfg(test)]
pub(crate) fn isolate_test_memory_dir() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let dir = tempfile::tempdir().expect("tempdir for memory isolation");
        std::env::set_var("KAJI_MEMORY_DIR", dir.path());
        std::mem::forget(dir);
    });
}

/// Directory holding the project-scoped facts for `working_dir`. Inside a git
/// worktree the facts live in the repo (`.kaji/memory`) so they travel with it;
/// outside one they fall back to a per-path dir under the memory dir.
pub fn project_facts_dir(working_dir: &Path) -> PathBuf {
    match find_git_root(working_dir) {
        Some(root) => root.join(".kaji").join("memory"),
        None => memory_dir().join("projects").join(path_slug(working_dir)),
    }
}

/// Directory holding the user-scoped facts, shared across every project.
pub fn user_facts_dir() -> PathBuf {
    memory_dir().join("user")
}

/// Path of the recall index for `working_dir`. Always derived data under the
/// memory dir, never inside the repo, even when the facts themselves are.
pub fn fact_index_path(working_dir: &Path) -> PathBuf {
    memory_dir()
        .join("index")
        .join(format!("{}.db", path_slug(working_dir)))
}

fn path_slug(path: &Path) -> String {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    slugify(absolute.to_string_lossy().as_ref())
}

/// A file-backed handle over the shared cross-session store, scoped to one
/// session for writes. Recall is global by design; anchoring is restricted to
/// each hit's own session by `Memory::anchored`.
pub struct SessionMemory {
    store: Memory,
    session_id: String,
}

impl SessionMemory {
    /// Load (or create) the shared store and return a handle for `session_id`.
    /// One-time, idempotent migration folds legacy per-session databases
    /// (`{session_id}.db`) into the shared store, then renames them to
    /// `{session_id}.db.legacy` so the import never runs twice.
    ///
    /// The store dir resolves through `KAJI_MEMORY_DIR` (test isolation)
    /// before falling back to the standard data dir.
    pub fn load(session_id: &str) -> Self {
        let dir = memory_dir();
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(SHARED_FILE);
        let mut store = Memory::open(&path).unwrap_or_default();
        migrate_legacy_sessions(&dir, &mut store);
        SessionMemory {
            store,
            session_id: session_id.to_string(),
        }
    }

    /// Retrieve the top-k facts relevant to `query`, across all sessions.
    /// Zero LLM tokens spent.
    pub fn recall(&self, query: &str, k: usize) -> RecallResult {
        self.store.recall(query, k)
    }

    /// Attach temporal context (window + bookends) around a recall hit, scoped
    /// to the hit's own session so foreign sessions never leak their arc.
    pub fn anchored(&self, hit: &RecallHit, window: usize, bookend: usize) -> Option<Anchored> {
        self.store.anchored(hit, window, bookend)
    }

    /// Persist a fact under this handle's session; optional TTL.
    pub fn remember(&mut self, text: &str, entities: &[&str], ttl: Option<Duration>) {
        self.store
            .remember_in_session(&self.session_id, text, entities, ttl);
    }

    /// True when the caller's usage ratio crossed the AIAD budget band.
    pub fn should_compact(&self, usage_ratio: f64) -> bool {
        self.store.should_compact(usage_ratio)
    }

    /// Record a fact about the session, skipping it if already stored or too
    /// large to be a concise fact (big payloads live in the conversation, not
    /// in memory, where recall re-injects them into every future system prompt).
    /// Entity extraction is zero-token: content words (length > 3,
    /// stopword-filtered) become the FTS5-weighted entities column.
    pub fn ingest(&mut self, text: &str) {
        if text.trim().is_empty() || text.len() > MAX_FACT_LEN || self.store.contains_body(text) {
            return;
        }
        let entities = extract_entities(text);
        let entities: Vec<&str> = entities.iter().map(String::as_str).collect();
        self.remember(text, &entities, None);
    }

    /// Zero-token prompt block : the top-k recalled facts for `query`, each
    /// tagged with its source session and followed by its temporal context
    /// (anchored window + bookends) scoped to that session.
    ///
    /// Rendered as plain markdown so it can be appended to a system prompt by
    /// either agent-loop path. Returns `None` when nothing relevant is stored.
    pub fn recall_prompt(&self, query: &str, k: usize) -> Option<String> {
        let result = self.recall(query, k);
        if result.hits.is_empty() {
            return None;
        }
        let mut parts = vec![BLOCK_HEADER.to_string()];
        for (i, hit) in result.hits.iter().enumerate() {
            let source = if hit.session_id.is_empty() {
                String::new()
            } else {
                format!("[{}] ", hit.session_id)
            };
            parts.push(format!("{i}. {source}{}", hit.body));
            if let Some(view) = self.anchored(hit, 2, 1) {
                let context = render_anchored(&view);
                if !context.is_empty() {
                    parts.push(context);
                }
            }
        }
        Some(parts.join("\n"))
    }

    /// Owned snapshot of all entries in the shared store, newest first. Backed
    /// by SQLite; used by the `kaji memory` inspection CLI.
    pub fn list(&self) -> Vec<Entry> {
        self.store.iter()
    }

    /// Entries the curator hasn't processed yet, oldest first. Global like
    /// recall: a curation run drains whatever the store still owes, whichever
    /// session recorded it.
    pub fn uncurated(&self, limit: usize) -> Vec<Entry> {
        self.store.uncurated(limit)
    }

    /// Stamp `ids` as curated. Called only once a curation run has fully
    /// succeeded, so a failed run replays its batch.
    pub fn mark_curated(&mut self, ids: &[u64]) {
        self.store.mark_curated(ids);
    }

    /// Drop entries from the shared store. When `session_id` is `Some`, only
    /// that session's entries are removed ("" targets legacy/global entries);
    /// `None` clears the whole store. Returns how many entries were removed.
    pub fn clear(&mut self, session_id: Option<&str>) -> usize {
        match session_id {
            Some(sid) => self.store.forget_where(|e| e.session_id == sid),
            None => self.store.forget_where(|_| true),
        }
    }
}

/// Import legacy per-session database files (`{session_id}.db`) into the shared
/// store, tagged with the source session, then rename them to
/// `{session_id}.db.legacy`. Idempotent by construction: once renamed, a file
/// no longer matches the `*.db` scan, so a restart cannot double-import.
fn migrate_legacy_sessions(dir: &Path, shared: &mut Memory) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(name) => name,
            None => continue,
        };
        if name == SHARED_FILE || !name.ends_with(".db") || name.ends_with(".legacy") {
            continue;
        }
        let Some(session_id) = name.strip_suffix(".db") else {
            continue;
        };
        let Ok(legacy) = Memory::open(&path) else {
            continue;
        };
        for entry in legacy.iter() {
            if shared.contains_body(&entry.body) {
                continue;
            }
            let entities: Vec<&str> = entry.entities.iter().map(String::as_str).collect();
            shared.remember_in_session(session_id, &entry.text, &entities, entry.ttl);
        }
        let _ = std::fs::rename(&path, path.with_extension("db.legacy"));
    }
}

/// Guards the legacy `.txt` import: one attempt per process, on first access.
static LEGACY_TXT_MIGRATION: Once = Once::new();

/// Dir the legacy memory extension used for its user-wide categories: the
/// config dir, not the data dir the facts live in.
fn legacy_global_txt_dir() -> PathBuf {
    Paths::in_config_dir("memory")
}

/// Import the categories left by the legacy MCP memory extension. Each `*.txt`
/// file becomes one `reference` fact — project scope for the repo-local dir
/// (the very dir the fact store now owns), user scope for the global one — and
/// the original is renamed `*.txt.legacy`, never deleted.
///
/// Idempotent by construction: a renamed file no longer matches the `*.txt`
/// scan, so a second boot imports nothing. Facts (`*.md`) sharing the dir are
/// out of the scan by the same filter.
pub fn migrate_legacy_txt_memory(working_dir: &Path) {
    let project_dir = project_facts_dir(working_dir);
    let project = FactStore::new(project_dir.clone());
    let working_dir_local = working_dir.join(".kaji").join("memory");

    import_legacy_txt_dir(&project_dir, &project, true);
    if working_dir_local != project_dir {
        import_legacy_txt_dir(&working_dir_local, &project, true);
    }
    import_legacy_txt_dir(
        &legacy_global_txt_dir(),
        &FactStore::new(user_facts_dir()),
        false,
    );
}

/// Import every legacy category of `dir` into `store`. Errors are per-file:
/// a category that can't be read or written is logged and left in place, so
/// the next boot retries it.
fn import_legacy_txt_dir(dir: &Path, store: &FactStore, redact: bool) {
    let Ok(dir_entries) = std::fs::read_dir(dir) else {
        return;
    };
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    for dir_entry in dir_entries.flatten() {
        let path = dir_entry.path();
        let Some(category) = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_suffix(".txt"))
        else {
            continue;
        };
        let Ok(content) = std::fs::read_to_string(&path) else {
            tracing::warn!(file = %path.display(), "legacy memory category unreadable");
            continue;
        };

        let records = parse_legacy_txt(&content);
        if !records.is_empty() {
            let body = records.join("\n");
            let fact = Fact {
                fact_type: FactType::Reference,
                slug: slugify(category),
                description: format!(
                    "Importé de l'extension memory héritée ({} entrées)",
                    records.len()
                ),
                date: today.clone(),
                session: String::new(),
                created_by: CreatedBy::Curator,
                body: if redact { redact_text(&body).0 } else { body },
            };
            let held_by_user = store
                .get(&FactType::Reference, &fact.slug)
                .is_some_and(|existing| existing.created_by == CreatedBy::User);
            if held_by_user {
                tracing::warn!(
                    file = %path.display(),
                    "legacy memory import skipped: the slug already holds a user fact"
                );
            } else if let Err(err) = store.write(&fact) {
                tracing::warn!(file = %path.display(), error = %err, "legacy memory import failed");
                continue;
            }
        }
        let _ = std::fs::rename(&path, path.with_extension("txt.legacy"));
    }
}

/// Entries of a legacy category file: blocks separated by a blank line, each
/// optionally opened by a `# tag1 tag2` line. Tags become a line prefix so they
/// stay searchable in the fact body.
fn parse_legacy_txt(content: &str) -> Vec<String> {
    content
        .split("\n\n")
        .filter_map(|block| {
            let mut lines = block.lines();
            let first = lines.next()?;
            let (tags, data) = match first.strip_prefix('#') {
                Some(tags) => (
                    tags.split_whitespace().collect::<Vec<_>>().join(" "),
                    lines.collect::<Vec<_>>(),
                ),
                None => (
                    String::new(),
                    std::iter::once(first).chain(lines).collect::<Vec<_>>(),
                ),
            };
            let text = data.join("\n");
            let text = text.trim();
            if text.is_empty() {
                return None;
            }
            Some(if tags.is_empty() {
                text.to_string()
            } else {
                format!("[{tags}] {text}")
            })
        })
        .collect()
}

/// Splice the KAJI memory block into a system prompt for `session_id`, using
/// `query` (usually the latest user instruction) as the recall query.
///
/// Shared by both agent-loop paths (legacy `prepare_reply_context` and the
/// state machine's inference op) so the behavior stays in parity. Returns the
/// prompt unchanged when nothing relevant is stored.
///
/// The second element is the block appended to the prompt, verbatim — the
/// event log records it so a replay serves the same recall instead of running
/// it again against a store that has moved on. `None` when nothing was spliced.
///
/// `working_dir` is the session's own dir, the same one the curator writes
/// through — never `std::env::current_dir()`, which a resumed session may no
/// longer be running from: reading a scope the writer never used would hide
/// every project fact.
pub fn splice_memory_block(
    system_prompt: &str,
    session_id: &str,
    query: &str,
    working_dir: &Path,
) -> (String, Option<String>) {
    LEGACY_TXT_MIGRATION.call_once(|| migrate_legacy_txt_memory(working_dir));
    let mem = SessionMemory::load(session_id);
    let mut parts = Vec::new();
    parts.extend(curated_facts_block(working_dir, query, FACTS_TOP_K));
    parts.extend(mem.recall_prompt(query, 3));
    if parts.is_empty() {
        return (system_prompt.to_string(), None);
    }
    let block = parts.join("\n\n");
    (format!("{system_prompt}\n\n{block}"), Some(block))
}

/// Le bloc mémoire du tour, appliqué au prompt système : recalculé et
/// journalisé à l'enregistrement, resservi depuis le journal au rejeu.
///
/// Point unique partagé par les deux boucles — chacune n'en porte qu'un appel,
/// donc ni le splice, ni la capture, ni la bascule de rejeu ne peuvent diverger
/// entre elles (spec
/// `docs/superpowers/specs/2026-08-27-event-log-v2-replay-exact-design.md`, S3).
pub async fn apply_memory_block(
    system_prompt: String,
    messages: &[crate::conversation::message::Message],
    session_id: &str,
    working_dir: &Path,
    recorder: Option<&Arc<crate::replay::record::TurnRecorder>>,
    replay: Option<&crate::replay::source::ReplaySource>,
) -> String {
    if let Some(replay) = replay {
        return match replay.memory_block() {
            Some(block) => format!("{system_prompt}\n\n{block}"),
            None => system_prompt,
        };
    }

    let Some(query) = latest_user_instruction(messages) else {
        return system_prompt;
    };
    let (spliced, block) = splice_memory_block(&system_prompt, session_id, &query, working_dir);
    crate::replay::record::record_memory_block(recorder, block.as_deref()).await;
    spliced
}

/// Top-k curated facts for `query`, both scopes merged into the same bm25
/// ranking, rendered as a markdown block. `None` when nothing matches.
///
/// Recall must never break a turn: an unreadable index or a failed rebuild
/// drops the block and lets the raw journal splice go through.
fn curated_facts_block(working_dir: &Path, query: &str, k: usize) -> Option<String> {
    let project = FactStore::new(project_facts_dir(working_dir));
    let user = FactStore::new(user_facts_dir());
    let mut index = FactIndex::open(&fact_index_path(working_dir)).ok()?;
    index
        .rebuild_if_stale(&[("project", &project), ("user", &user)])
        .ok()?;

    let hits = index.search(query, k);
    if hits.is_empty() {
        return None;
    }
    let mut parts = vec![FACTS_HEADER.to_string()];
    for (i, hit) in hits.iter().enumerate() {
        parts.push(format!(
            "{}. [{}] {} — {}",
            i + 1,
            fact_type_label(&hit.file_name),
            hit.description,
            truncate_chars(&hit.body, FACT_BODY_MAX_CHARS)
        ));
    }
    Some(parts.join("\n"))
}

/// Fact type carried by a fact file name (`{type}-{slug}.md`).
fn fact_type_label(file_name: &str) -> &str {
    file_name
        .split_once('-')
        .map(|(fact_type, _)| fact_type)
        .unwrap_or(file_name)
}

fn truncate_chars(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

/// Uncurated journal entries a turn must have accumulated before a curation
/// run is worth an LLM call.
pub const CURATE_MIN_PENDING: usize = 5;

/// Quiet period between two curation runs. A busy session keeps ingesting; the
/// debounce keeps the background curator from firing on every turn.
pub const CURATE_DEBOUNCE_SECS: u64 = 300;

/// Wall-clock seconds of the last armed curation, process-wide. Both agent
/// loops share it, so a single debounce governs whichever path runs the turn.
static LAST_CURATION_SECS: AtomicU64 = AtomicU64::new(0);

/// True when the journal owes enough entries and the debounce has elapsed.
///
/// Arming is exclusive: the caller that observes the window open stamps it via
/// compare-exchange, so concurrent turns produce a single curation run.
pub fn curation_due(session_id: &str, now_secs: u64) -> bool {
    let pending = SessionMemory::load(session_id)
        .uncurated(CURATE_MIN_PENDING)
        .len();
    if pending < CURATE_MIN_PENDING {
        return false;
    }
    let last = LAST_CURATION_SECS.load(Ordering::Acquire);
    if now_secs.saturating_sub(last) < CURATE_DEBOUNCE_SECS {
        return false;
    }
    LAST_CURATION_SECS
        .compare_exchange(last, now_secs, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
}

/// Run one curation and report its outcome to the logs. Detached from the turn:
/// a failure is reported, never propagated, and never stamps the journal.
///
/// [`CurationOutcome::failed`](crate::memory_curator::CurationOutcome::failed)
/// is surfaced on its own — a run that wrote some facts and dropped others is a
/// warning, not a success, because the batch stays pending and will be replayed.
pub async fn run_curation(
    provider: Arc<dyn Provider>,
    provider_name: String,
    model_config: ModelConfig,
    session_id: String,
    working_dir: PathBuf,
) {
    match crate::memory_curator::curate(
        provider.as_ref(),
        &provider_name,
        &model_config,
        &session_id,
        &working_dir,
    )
    .await
    {
        Ok(outcome) if outcome.failed > 0 => tracing::warn!(
            created = outcome.created,
            updated = outcome.updated,
            failed = outcome.failed,
            "記 curation incomplete; the batch replays on the next trigger"
        ),
        Ok(outcome) if outcome.created + outcome.updated > 0 => tracing::info!(
            created = outcome.created,
            updated = outcome.updated,
            failed = outcome.failed,
            "記 {} faits mémorisés",
            outcome.created + outcome.updated
        ),
        Ok(_) => {}
        Err(err) => tracing::warn!("記 curation failed: {err}"),
    }
}

/// End-of-turn curation trigger, called by both agent loops once the memory
/// block has been spliced. Spawns [`run_curation`] when the journal is due;
/// otherwise it costs one indexed read and returns.
///
/// Never blocks the turn and never touches the reply: the run is detached and
/// its result only reaches the logs.
pub fn maybe_spawn_curation(
    provider: Arc<dyn Provider>,
    provider_name: String,
    model_config: ModelConfig,
    session_id: String,
    working_dir: PathBuf,
) {
    if !curation_due(&session_id, now_secs()) {
        return;
    }
    tokio::spawn(run_curation(
        provider,
        provider_name,
        model_config,
        session_id,
        working_dir,
    ));
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
}

/// Extract the latest user instruction from a conversation — the natural query
/// for a memory recall. Skips tool responses and flagged transcripts.
pub fn latest_user_instruction(
    messages: &[crate::conversation::message::Message],
) -> Option<String> {
    messages.iter().rev().find_map(|message| {
        let role = crate::conversation::effective_role(message);
        if role == crate::conversation::EffectiveRole::User {
            let text = message.as_concat_text();
            if !text.trim().is_empty() {
                return Some(text);
            }
        }
        None
    })
}

fn extract_entities(text: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() > 3)
        .map(|w| w.to_ascii_lowercase())
        .filter(|w| !STOPWORDS.contains(&w.as_str()))
        .filter(|w| seen.insert(w.clone()))
        .collect()
}

const STOPWORDS: &[&str] = &[
    "that", "this", "with", "from", "have", "will", "your", "what", "when", "then", "them", "they",
    "were", "been", "would", "which", "there", "vous", "avec", "pour", "dans", "mais", "être",
    "fait", "tout", "comme", "plus", "pour", "alors",
];

/// Ingest the latest user instruction(s) of a conversation into session
/// memory — the write side of the recall loop. Idempotent: already-stored
/// bodies are skipped.
pub fn ingest_turn(session_id: &str, messages: &[crate::conversation::message::Message]) {
    let mut mem = SessionMemory::load(session_id);
    let instructions = messages
        .iter()
        .filter(|message| {
            crate::conversation::effective_role(message) == crate::conversation::EffectiveRole::User
        })
        .filter_map(|message| {
            let text = message.as_concat_text();
            (!text.trim().is_empty()).then_some(text)
        })
        .collect::<Vec<_>>();
    for instruction in instructions.iter().rev().take(3) {
        mem.ingest(instruction);
    }
}

fn render_anchored(view: &Anchored) -> String {
    let mut lines = Vec::new();
    if !view.opening.is_empty() {
        lines.push("   — opening:".to_string());
        for entry in &view.opening {
            lines.push(format!("     * {}", entry.body));
        }
    }
    if !view.window.is_empty() {
        lines.push("   — nearby:".to_string());
        for entry in &view.window {
            lines.push(format!("     * {}", entry.body));
        }
    }
    if !view.resolution.is_empty() {
        lines.push("   — resolution:".to_string());
        for entry in &view.resolution {
            lines.push(format!("     * {}", entry.body));
        }
    }
    lines.join("\n")
}

impl Default for SessionMemory {
    fn default() -> Self {
        Self::load("default")
    }
}
