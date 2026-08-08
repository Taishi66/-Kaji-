//! Memory layer for KAJI : SQLite FTS5 store + zero-token recall.
//!
//! Design goals (ADR-03, ADR-004):
//! - **Zero-token recall**: `recall` ranks persisted entries with SQLite's
//!   native FTS5 BM25 (`MATCH … ORDER BY bm25(...)`). No LLM inference is spent
//!   finding relevant memory — the whole point vs. naive "summarize everything
//!   back in context".
//! - **External-content index**: the FTS5 table only holds the inverted index;
//!   `memory_entries` keeps the text (no duplication), kept in sync by
//!   triggers — incremental writes, crash-safe in WAL mode (ADR-004).
//! - **Budget**: compaction triggers at the AIAD band low bound [0.6, 0.7]
//!   instead of goose's reactive 0.8 (proactive context engineering).
//! - **Temporal invalidation**: every entry carries a timestamp and an optional
//!   ttl; expired entries are swept on `remember` and excluded from `recall`,
//!   plugging the ADR-03 "temporal invalidation" gap.
//!
//! `rusqlite` links the system SQLite shared by `sqlx-sqlite` (goose session),
//! so no vendored C is compiled and both crates share one library.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::Connection;

/// AIAD-prescribed compaction budget band. Compaction is triggered when the
/// context fill ratio reaches the low end, not goose's reactive 0.8.
pub const BUDGET_MIN: f64 = 0.6;
pub const BUDGET_MAX: f64 = 0.7;
/// Borrowed from the retained goose context_mgmt constant for parity.
pub const GOOSE_LEGACY_THRESHOLD: f64 = 0.8;

/// FTS5 column weights passed to `bm25()`; the entities column outranks plain
/// text (mirrors the POC's entity IDF boost, now at query time).
const WEIGHTS: [f64; 3] = [1.0, 5.0, 1.0];

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// How many ms a `remember` ttl means (sqlite stores ms as INTEGER).
fn ttl_ms(ttl: Option<Duration>) -> Option<i64> {
    ttl.map(|t| t.as_millis().min(i64::MAX as u128) as i64)
}

/// `as_of` is in seconds in the API but the DB compares in milliseconds.
fn as_of_ms(as_of_secs: u64) -> i64 {
    (as_of_secs as i64).saturating_mul(1000)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub id: u64,
    pub text: String,
    /// Entities/keywords extracted at ingest; weighted 5x in FTS5 BM25.
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

/// Context surrounding one recall hit, mirroring Hermes-Agent's
/// `get_anchored_view(hit, window, bookend)` (`hermes_state_search.py`).
///
/// A bare hit loses the narrative: the agent reads facts out of order. The
/// anchored view re-attaches temporally adjacent entries (the window) plus the
/// session's opening and resolution (the bookends), so the prompt gets the arc
/// around a fact — opening = goal, end = outcome — at zero LLM cost.
#[derive(Debug, Clone)]
pub struct Anchored {
    pub hit: RecallHit,
    /// `window` entries with the closest timestamps around the hit (excluded).
    pub window: Vec<Entry>,
    /// First `bookend` entries (session opening / goal).
    pub opening: Vec<Entry>,
    /// Last `bookend` entries (session resolution / outcome).
    pub resolution: Vec<Entry>,
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS memory_entries (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    text TEXT NOT NULL,
    entities TEXT NOT NULL DEFAULT '',
    body TEXT NOT NULL,
    ts INTEGER NOT NULL,
    ttl_ms INTEGER
);
CREATE VIRTUAL TABLE IF NOT EXISTS memory_fts USING fts5(
    text, entities, body,
    content='memory_entries',
    content_rowid='id'
);
CREATE TRIGGER IF NOT EXISTS memory_fts_insert AFTER INSERT ON memory_entries BEGIN
    INSERT INTO memory_fts(rowid, text, entities, body)
    VALUES (new.id, new.text, new.entities, new.body);
END;
CREATE TRIGGER IF NOT EXISTS memory_fts_delete AFTER DELETE ON memory_entries BEGIN
    INSERT INTO memory_fts(memory_fts, rowid, text, entities, body)
    VALUES ('delete', old.id, old.text, old.entities, old.body);
END;
CREATE TRIGGER IF NOT EXISTS memory_fts_update AFTER UPDATE OF text, entities, body ON memory_entries BEGIN
    INSERT INTO memory_fts(memory_fts, rowid, text, entities, body)
    VALUES ('delete', old.id, old.text, old.entities, old.body);
    INSERT INTO memory_fts(rowid, text, entities, body)
    VALUES (new.id, new.text, new.entities, new.body);
END;
"#;

pub struct Memory {
    conn: Connection,
}

impl Default for Memory {
    fn default() -> Self {
        Self::new()
    }
}

impl Memory {
    /// In-memory store (tests, ephemeral).
    pub fn new() -> Self {
        let conn = Connection::open_in_memory().expect("sqlite in-memory open");
        conn.execute_batch(SCHEMA).expect("memory schema");
        Memory { conn }
    }

    /// Persistent store at `path` (creates schema idempotently, WAL mode).
    pub fn open<P: AsRef<Path>>(path: P) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Memory { conn })
    }

    /// Persist a fact + its extracted entities. Sweeps entries past their TTL
    /// first. Returns the new entry id.
    pub fn remember(&mut self, text: &str, entities: &[&str], ttl: Option<Duration>) -> u64 {
        self.sweep_stale(now_secs());
        let ts_ms = now_secs() as i64 * 1000;
        let entities_joined = entities
            .iter()
            .map(|e| e.to_ascii_lowercase())
            .collect::<Vec<_>>()
            .join(" ");
        let body = text.to_string();
        let ttl = ttl_ms(ttl);
        self.conn
            .execute(
                "INSERT INTO memory_entries (text, entities, body, ts, ttl_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![text, entities_joined, body, ts_ms, ttl],
            )
            .expect("memory_entries insert");
        self.conn.last_insert_rowid() as u64
    }

    /// Zero-token recall : FTS5 BM25 (entities weighted 5x). Top-k ranked by
    /// relevance, ties broken by id.
    pub fn recall(&self, query: &str, k: usize) -> RecallResult {
        self.recall_at(query, k, now_secs())
    }

    /// Enrich one hit with its temporal neighborhood : `window` entries closest
    /// in time around it, plus the store's opening and resolution.
    ///
    /// Mirrors Hermes' `get_anchored_view(hit, window, bookend)` : a recalled
    /// fact alone loses the narrative arc, so the surrounding facts (before,
    /// after) + bookends (goal / outcome) are attached for the prompt. All pure
    /// SQL (no LLM tokens). Returns `None` when the entry is gone (swept).
    pub fn anchored(&self, hit: &RecallHit, window: usize, bookend: usize) -> Option<Anchored> {
        let ts = self.get(hit.id)?.ts;
        let sql_window = "SELECT id, text, entities, body, ts, ttl_ms
                          FROM memory_entries
                          WHERE id != ?1
                            AND (ttl_ms IS NULL OR ts + ttl_ms >= ?2)
                          ORDER BY ABS(ts - ?3) ASC, id ASC
                          LIMIT ?4";
        let w = self
            .conn
            .prepare(sql_window)
            .ok()?
            .query_map(
                rusqlite::params![
                    hit.id as i64,
                    as_of_ms(now_secs()),
                    (ts as i64) * 1000,
                    window as i64
                ],
                row_to_entry,
            )
            .ok()?
            .filter_map(Result::ok)
            .collect::<Vec<_>>();

        let sql_side = "SELECT id, text, entities, body, ts, ttl_ms
                        FROM memory_entries
                        WHERE ttl_ms IS NULL OR ts + ttl_ms >= ?1
                        ORDER BY {dir} LIMIT ?2";
        let opening = self
            .conn
            .prepare(&sql_side.replace("{dir}", "ts ASC, id ASC"))
            .ok()?
            .query_map(
                rusqlite::params![as_of_ms(now_secs()), bookend as i64],
                row_to_entry,
            )
            .ok()?
            .filter_map(Result::ok)
            .collect::<Vec<_>>();
        let mut stmt = self
            .conn
            .prepare(&sql_side.replace("{dir}", "ts DESC, id DESC"))
            .ok()?;
        let mut resolution_raw = stmt
            .query_map(
                rusqlite::params![as_of_ms(now_secs()), bookend as i64],
                row_to_entry,
            )
            .ok()?
            .filter_map(Result::ok)
            .collect::<Vec<_>>();
        resolution_raw.reverse();
        let resolution = resolution_raw;

        Some(Anchored {
            hit: hit.clone(),
            window: w,
            opening,
            resolution,
        })
    }

    fn recall_at(&self, query: &str, k: usize, as_of: u64) -> RecallResult {
        let stale_dropped = self.count_stale(as_of);
        let match_expr = fts_query(query);
        if match_expr.is_empty() {
            return RecallResult {
                hits: Vec::new(),
                stale_dropped,
            };
        }

        let sql = "SELECT e.id, e.body,
                          bm25(memory_fts, ?1, ?2, ?3) AS score
                   FROM memory_fts f
                   JOIN memory_entries e ON e.id = f.rowid
                   WHERE memory_fts MATCH ?4
                     AND (e.ttl_ms IS NULL OR e.ts + e.ttl_ms >= ?5)
                   ORDER BY score ASC, e.id ASC
                   LIMIT ?6";

        let mut stmt = match self.conn.prepare(sql) {
            Ok(s) => s,
            Err(_) => {
                return RecallResult {
                    hits: Vec::new(),
                    stale_dropped,
                }
            }
        };

        let as_of = as_of_ms(as_of);
        let hits = stmt
            .query_map(
                rusqlite::params![WEIGHTS[0], WEIGHTS[1], WEIGHTS[2], match_expr, as_of, k],
                |row| {
                    Ok(RecallHit {
                        id: row.get::<_, i64>(0)? as u64,
                        body: row.get(1)?,
                        // bm25() returns negative (better = more negative);
                        // flip to a positive score where higher = more relevant.
                        score: (-row.get::<_, f64>(2)? * 1000.0) as u32,
                    })
                },
            )
            .map(|iter| iter.filter_map(Result::ok).collect::<Vec<_>>())
            .unwrap_or_default();

        RecallResult {
            hits,
            stale_dropped,
        }
    }

    fn count_stale(&self, as_of: u64) -> usize {
        let as_of = as_of_ms(as_of);
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM memory_entries
                 WHERE ttl_ms IS NOT NULL AND ts + ttl_ms < ?1",
                [as_of],
                |row| row.get::<_, i64>(0),
            )
            .map(|n| n as usize)
            .unwrap_or(0)
    }

    /// True when the aggregate historical context fill has crossed the AIAD
    /// low bound. Callers pass the ratio of used tokens to context limit
    /// (as goose's `check_if_compaction_needed` computes).
    pub fn should_compact(&self, usage_ratio: f64) -> bool {
        usage_ratio >= BUDGET_MIN
    }

    /// Drop entries matching a predicate; returns how many were removed.
    pub fn forget_where<F>(&mut self, mut pred: F) -> usize
    where
        F: FnMut(&Entry) -> bool,
    {
        let to_drop: Vec<u64> = self
            .iter()
            .into_iter()
            .filter(|e| pred(e))
            .map(|e| e.id)
            .collect();
        let n = to_drop.len();
        for id in to_drop {
            let _ = self
                .conn
                .execute("DELETE FROM memory_entries WHERE id = ?1", [id]);
        }
        n
    }

    pub fn len(&self) -> usize {
        self.conn
            .query_row("SELECT COUNT(*) FROM memory_entries", [], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap_or(0) as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn get(&self, id: u64) -> Option<Entry> {
        self.conn
            .query_row(
                "SELECT id, text, entities, body, ts, ttl_ms
                 FROM memory_entries WHERE id = ?1",
                [id],
                row_to_entry,
            )
            .ok()
    }

    /// Whether an entry with this exact body already exists. Cheap dedup for
    /// ingestion loops that would otherwise rewrite the same fact every turn.
    pub fn contains_body(&self, body: &str) -> bool {
        self.conn
            .query_row(
                "SELECT 1 FROM memory_entries WHERE body = ?1 LIMIT 1",
                [body],
                |_| Ok(()),
            )
            .is_ok()
    }

    /// Owned snapshot of all entries (DB-backed, so owned rather than refs).
    pub fn iter(&self) -> Vec<Entry> {
        self.iter_entries()
    }

    fn iter_entries(&self) -> Vec<Entry> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, text, entities, body, ts, ttl_ms FROM memory_entries")
            .expect("prepare memory_entries select");
        stmt.query_map([], row_to_entry)
            .map(|iter| iter.filter_map(Result::ok).collect())
            .unwrap_or_default()
    }

    fn sweep_stale(&mut self, as_of: u64) {
        let as_of = as_of_ms(as_of);
        let _ = self.conn.execute(
            "DELETE FROM memory_entries WHERE ttl_ms IS NOT NULL AND ts + ttl_ms < ?1",
            [as_of],
        );
    }
}

fn row_to_entry(row: &rusqlite::Row) -> rusqlite::Result<Entry> {
    let id: i64 = row.get(0)?;
    let text: String = row.get(1)?;
    let entities_raw: String = row.get(2)?;
    let body: String = row.get(3)?;
    let ts: i64 = row.get(4)?;
    let ttl_ms: Option<i64> = row.get(5)?;
    let entities = entities_raw
        .split_whitespace()
        .map(str::to_string)
        .collect();
    let ttl = ttl_ms.map(|ms| Duration::from_millis(ms.max(0) as u64));
    Ok(Entry {
        id: id as u64,
        text,
        entities,
        body,
        ts: (ts / 1000) as u64,
        ttl,
    })
}

/// Build a safe FTS5 MATCH expression: quote every token, OR-join them.
///
/// OR maximizes *recall* (return every candidate that could be relevant),
/// then FTS5 BM25 ranks them — exactly what a memory-retrieval path wants
/// (like the POC's vector-free scoring). A stray special char in user input
/// is quoted away so it can't break the query grammar.
fn fts_query(query: &str) -> String {
    let tokens: Vec<String> = query
        .split_whitespace()
        .map(|t| t.replace('"', "\"\""))
        .map(|t| format!("\"{t}\""))
        .collect();
    tokens.join(" OR ")
}
