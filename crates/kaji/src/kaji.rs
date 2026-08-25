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
use std::time::Duration;

use kaji_core::facts::slugify;
use kaji_core::memory::{Anchored, Entry, Memory, RecallHit, RecallResult};

pub use kaji_core::memory::Entry as MemoryEntry;

use crate::config::paths::{find_git_root, Paths};

/// Rendered prefix for the memory block in a system prompt.
const BLOCK_HEADER: &str = "## KAJI memory — recalled across sessions";

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

/// Splice the KAJI memory block into a system prompt for `session_id`, using
/// `query` (usually the latest user instruction) as the recall query.
///
/// Shared by both agent-loop paths (legacy `prepare_reply_context` and the
/// state machine's inference op) so the behavior stays in parity. Returns the
/// prompt unchanged when nothing relevant is stored.
pub fn splice_memory_block(system_prompt: &str, session_id: &str, query: &str) -> String {
    let mem = SessionMemory::load(session_id);
    match mem.recall_prompt(query, 3) {
        Some(block) => format!("{system_prompt}\n\n{block}"),
        None => system_prompt.to_string(),
    }
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
