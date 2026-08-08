//! KAJI integration bridge : links the kaji-core memory store to the goose
//! kernel without touching the two agent-loop hot paths.
//!
//! Scope (ADR-03 wiring milestone):
//! - **Inter-session persistence**: one store file per session, kept under the
//!   goose data dir (cross-session recall survives process restarts).
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
/// explicit save. One DB file per session, kept under the goose data dir.
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
