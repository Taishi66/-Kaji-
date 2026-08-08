//! KAJI integration bridge : links the kaji-core memory store to the kaji
//! kernel without touching the two agent-loop hot paths.
//!
//! Scope (ADR-03 wiring milestone):
//! - **Inter-session persistence**: one store file per session, kept under the
//!   kaji data dir (cross-session recall survives process restarts).
//! - **Ingestion**: snapshot current session messages into memory via
//!   `ingest_session` (callers pass the text they want recalled later).
//! - **Recall**: ranked, zero-token retrieval of prior facts for a prompt.
//! - **Prompt block**: `recall_prompt` renders the recalled facts + their
//!   anchored context into a plain-text block ready to splice into a system
//!   prompt. Still only rendered on demand — callers decide when to inject.

use std::time::Duration;

use kaji_core::memory::{Anchored, Memory, RecallHit, RecallResult};

use crate::config::paths::Paths;

/// Rendered prefix for the memory block in a system prompt.
const BLOCK_HEADER: &str = "## KAJI memory — recalled from prior sessions";

/// A session-scoped, file-backed memory handle.
///
/// Persistence is delegated to the SQLite + FTS5 store in kaji-core: every
/// read/write hits the file directly, so there is no serialization step and no
/// explicit save. One DB file per session, kept under the kaji data dir.
pub struct SessionMemory {
    store: Memory,
}

impl SessionMemory {
    /// Load (or create) the store file for `session_id`.
    pub fn load(session_id: &str) -> Self {
        let dir = Paths::in_data_dir("kaji/memory");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!("{session_id}.db"));
        let store = Memory::open(&path).unwrap_or_default();
        SessionMemory { store }
    }

    /// Retrieve the top-k facts relevant to `query`. Zero LLM tokens spent.
    pub fn recall(&self, query: &str, k: usize) -> RecallResult {
        self.store.recall(query, k)
    }

    /// Attach temporal context (window + bookends) around a recall hit.
    pub fn anchored(&self, hit: &RecallHit, window: usize, bookend: usize) -> Option<Anchored> {
        self.store.anchored(hit, window, bookend)
    }

    /// Persist a fact with its entities; optional TTL for volatile facts.
    pub fn remember(&mut self, text: &str, entities: &[&str], ttl: Option<Duration>) {
        self.store.remember(text, entities, ttl);
    }

    /// True when the caller's usage ratio crossed the AIAD budget band.
    pub fn should_compact(&self, usage_ratio: f64) -> bool {
        self.store.should_compact(usage_ratio)
    }

    /// Record a fact about the session, skipping it if already stored. Entity
    /// extraction is zero-token: content words (length > 3, stopword-filtered)
    /// become the FTS5-weighted entities column.
    pub fn ingest(&mut self, text: &str) {
        if text.trim().is_empty() || self.store.contains_body(text) {
            return;
        }
        let entities = extract_entities(text);
        let entities: Vec<&str> = entities.iter().map(String::as_str).collect();
        self.store.remember(text, &entities, None);
    }

    /// Zero-token prompt block : the top-k recalled facts for `query`, each
    /// followed by its temporal context (anchored window + bookends).
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
            parts.push(format!("{i}. {}", hit.body));
            if let Some(view) = self.anchored(hit, 2, 1) {
                let context = render_anchored(&view);
                if !context.is_empty() {
                    parts.push(context);
                }
            }
        }
        Some(parts.join("\n"))
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
