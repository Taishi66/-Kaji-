//! KAJI integration bridge : links the kaji-core memory store to the goose
//! kernel without touching the two agent-loop hot paths.
//!
//! Scope (ADR-03 wiring milestone):
//! - **Inter-session persistence**: one store file per session, kept under the
//!   goose data dir (cross-session recall survives process restarts).
//! - **Ingestion**: snapshot current session messages into memory via
//!   `ingest_session` (callers pass the text they want recalled later).
//! - **Recall**: ranked, zero-token retrieval of prior facts for a prompt.
//!
//! Deliberately NOT yet injected into `prepare_reply_context` / the
//! state-machine path — that doc handles parity concerns across both agent-loop
//! implementations.

use std::time::Duration;

use kaji_core::memory::{Memory, RecallResult};

use crate::config::paths::Paths;

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

    /// Persist a fact with its entities; optional TTL for volatile facts.
    pub fn remember(&mut self, text: &str, entities: &[&str], ttl: Option<Duration>) {
        self.store.remember(text, entities, ttl);
    }

    /// True when the caller's usage ratio crossed the AIAD budget band.
    pub fn should_compact(&self, usage_ratio: f64) -> bool {
        self.store.should_compact(usage_ratio)
    }
}

impl Default for SessionMemory {
    fn default() -> Self {
        Self::load("default")
    }
}
