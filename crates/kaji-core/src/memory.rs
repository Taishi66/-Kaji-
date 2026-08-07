//! Memory layer for KAJI : a persistent store + zero-token recall.
//!
//! Design goals (ADR-03):
//! - **Zero-token recall**: `recall` ranks existing entries by BM25 directly on
//!   indexed text. No LLM inference is spent finding relevant memory — the
//!   whole point vs. naive "summarize everything back in context".
//! - **Budget**: the compaction threshold is bounded to the AIAD band [0.6, 0.7]
//!   instead of goose's default 0.8, so compaction triggers *before* the model
//!   degrades (proactive context engineering), not after.
//! - **Temporal invalidation**: every entry carries a timestamp and an optional
//!   ttl; expired entries are visible as stale on recall and swept on
//!   `remember`, plugging the ADR-03 "temporal invalidation" gap without
//!   requiring external tooling.
//!
//! BM25 is intentionally dependency-free here (stdlib + serde).

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// AIAD-prescribed compaction budget band. Compaction is triggered when the
/// context fill ratio reaches the low end, not goose's reactive 0.8.
pub const BUDGET_MIN: f64 = 0.6;
pub const BUDGET_MAX: f64 = 0.7;
/// Borrowed from the retained goose context_mgmt constant for parity.
pub const GOOSE_LEGACY_THRESHOLD: f64 = 0.8;

const K1: f64 = 1.2;
const B: f64 = 0.75;

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub id: u64,
    pub text: String,
    /// Entities/keywords extracted at ingest; give an IDF boost on recall.
    pub entities: Vec<String>,
    /// Full body (same as text today; kept so a future summarizer can shrink it).
    pub body: String,
    pub ts: u64,
    /// None = never expires.
    pub ttl: Option<Duration>,
}

impl Entry {
    pub fn is_expired(&self, as_of: u64) -> bool {
        self.ttl
            .is_some_and(|ttl| self.ts.saturating_add(ttl.as_secs()) < as_of)
    }
}

/// Long-lived ordered ids; BTreeMap so serialization is stable and the
/// persisted file is diff-friendly.
#[derive(Serialize, Deserialize)]
pub struct Memory {
    next_id: u64,
    entries: BTreeMap<u64, Entry>,
}

impl Default for Memory {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecallHit {
    pub id: u64,
    pub body: String,
    pub score: u32,
}

#[derive(Debug, Clone)]
pub struct RecallResult {
    pub hits: Vec<RecallHit>,
    /// Entries dropped because their TTL expired relative to the requested
    /// frame. Traceability for the invalidation decision.
    pub stale_dropped: usize,
}

impl Memory {
    pub fn new() -> Self {
        Memory {
            next_id: 1,
            entries: BTreeMap::new(),
        }
    }

    /// Persist a fact + its extracted entities. Sweeps entries past their TTL
    /// first.
    pub fn remember(&mut self, text: &str, entities: &[&str], ttl: Option<Duration>) -> u64 {
        self.sweep_stale(now_secs());
        let id = self.next_id;
        self.next_id += 1;
        self.entries.insert(
            id,
            Entry {
                id,
                text: text.to_string(),
                entities: entities.iter().map(|e| e.to_ascii_lowercase()).collect(),
                body: text.to_string(),
                ts: now_secs(),
                ttl,
            },
        );
        id
    }

    /// Zero-token recall : score entries with BM25 against tokenized query.
    /// Returns the top-k ranked by score (ties broken by id).
    pub fn recall(&self, query: &str, k: usize) -> RecallResult {
        self.recall_at(query, k, now_secs())
    }

    fn recall_at(&self, query: &str, k: usize, as_of: u64) -> RecallResult {
        let mut stale_dropped = 0;
        let mut scored: Vec<RecallHit> = Vec::new();
        let avgdl = avg_doc_len(self.entries.values());
        let query_terms = terms(query);

        for e in self.entries.values() {
            if e.is_expired(as_of) {
                stale_dropped += 1;
                continue;
            }
            scored.push(RecallHit {
                id: e.id,
                body: e.body.clone(),
                score: bm25_score(&query_terms, e, avgdl),
            });
        }

        scored.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| b.id.cmp(&a.id)));
        let hits = scored.into_iter().take(k).collect();
        RecallResult {
            hits,
            stale_dropped,
        }
    }

    /// True when the aggregate historical context fill has crossed the AIAD
    /// low bound. Mirrors `GOOSE_AUTO_COMPACT_THRESHOLD` semantics but inside
    /// the bounded band. Callers pass the ratio of used tokens to context limit
    /// (as goose's `check_if_compaction_needed` computes).
    pub fn should_compact(&self, usage_ratio: f64) -> bool {
        usage_ratio >= BUDGET_MIN
    }

    /// Drop entries matching a predicate; returns how many were removed.
    pub fn forget_where<F>(&mut self, mut pred: F) -> usize
    where
        F: FnMut(&Entry) -> bool,
    {
        let ids: Vec<u64> = self
            .entries
            .iter()
            .filter(|(_, e)| pred(e))
            .map(|(id, _)| *id)
            .collect();
        let n = ids.len();
        for id in ids {
            self.entries.remove(&id);
        }
        n
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn get(&self, id: u64) -> Option<&Entry> {
        self.entries.get(&id)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Entry> {
        self.entries.values()
    }

    fn sweep_stale(&mut self, as_of: u64) {
        let expired: Vec<u64> = self
            .entries
            .iter()
            .filter(|(_, e)| e.is_expired(as_of))
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            self.entries.remove(&id);
        }
    }
}

fn avg_doc_len<'a>(entries: impl Iterator<Item = &'a Entry>) -> f64 {
    let mut total = 0usize;
    let mut count = 0usize;
    for e in entries {
        total += terms(&e.text).len();
        count += 1;
    }
    if count == 0 {
        0.0
    } else {
        total as f64 / count as f64
    }
}

fn bm25_score(query_terms: &[String], doc: &Entry, avgdl: f64) -> u32 {
    let dl = terms(&doc.text).len() as f64;
    if dl == 0.0 || avgdl == 0.0 {
        return 0;
    }
    let tf_map: HashMap<String, usize> = {
        let mut m = HashMap::new();
        for t in terms(&doc.text) {
            *m.entry(t).or_insert(0) += 1;
        }
        m
    };
    let mut total: f64 = 0.0;
    for qt in query_terms {
        let tf = *tf_map.get(qt).unwrap_or(&0) as f64;
        if tf == 0.0 {
            continue;
        }
        // Entity boost: an extracted entity matching the query outranks raw text.
        let idf = if doc.entities.iter().any(|e| e == qt) {
            2.0
        } else {
            1.0
        };
        let score = idf * (tf * (K1 + 1.0)) / (tf + K1 * (1.0 - B + B * dl / avgdl));
        total += score;
    }
    (total * 1000.0) as u32
}

fn terms(text: &str) -> Vec<String> {
    text.split_whitespace()
        .flat_map(|w| {
            w.split(|c: char| !c.is_alphanumeric())
                .filter(|s| !s.is_empty())
        })
        .filter(|s| s.len() > 1)
        .map(|s| s.to_ascii_lowercase())
        .collect()
}
