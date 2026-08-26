use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension};

use super::{Fact, FactStore};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE VIRTUAL TABLE IF NOT EXISTS facts_fts USING fts5(scope UNINDEXED, file_name UNINDEXED, description, body);
"#;

pub struct FactHit {
    pub scope: String,
    pub file_name: String,
    pub description: String,
    pub body: String,
}

pub struct FactIndex {
    conn: Connection,
}

impl FactIndex {
    pub fn open(db_path: &Path) -> rusqlite::Result<FactIndex> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|err| rusqlite::Error::ToSqlConversionFailure(Box::new(err)))?;
        }
        let conn = Connection::open(db_path)?;
        // A concurrent session must not block a recall: WAL lets readers through
        // during a rebuild, and the wait stays short because this open sits in
        // the prompt hot path.
        conn.busy_timeout(Duration::from_millis(250))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(SCHEMA)?;
        Ok(FactIndex { conn })
    }

    pub fn rebuild_if_stale(&mut self, stores: &[(&str, &FactStore)]) -> anyhow::Result<()> {
        let mut fingerprint_lines = Vec::new();
        let mut entries: Vec<(&str, Fact)> = Vec::new();
        for &(scope, store) in stores {
            for fact in store.list() {
                let path = store.dir().join(fact.file_name());
                let metadata = std::fs::metadata(&path)?;
                let mtime_secs = metadata
                    .modified()?
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                fingerprint_lines.push(format!(
                    "{scope}/{}:{mtime_secs}:{}",
                    fact.file_name(),
                    metadata.len()
                ));
                entries.push((scope, fact));
            }
        }
        fingerprint_lines.sort();
        let fingerprint = fingerprint_lines.join(";");

        let current: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'fingerprint'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        if current.as_deref() == Some(fingerprint.as_str()) {
            return Ok(());
        }

        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM facts_fts", [])?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO facts_fts (scope, file_name, description, body) VALUES (?1, ?2, ?3, ?4)",
            )?;
            for (scope, fact) in &entries {
                stmt.execute(rusqlite::params![
                    scope,
                    fact.file_name(),
                    fact.description,
                    fact.body
                ])?;
            }
        }
        tx.execute(
            "INSERT INTO meta (key, value) VALUES ('fingerprint', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            rusqlite::params![fingerprint],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn search(&self, query: &str, k: usize) -> Vec<FactHit> {
        let match_expr = fts_query(query);
        if match_expr.is_empty() {
            return Vec::new();
        }

        let sql = "SELECT scope, file_name, description, body
                   FROM facts_fts
                   WHERE facts_fts MATCH ?1
                   ORDER BY bm25(facts_fts)
                   LIMIT ?2";
        let mut stmt = match self.conn.prepare(sql) {
            Ok(stmt) => stmt,
            Err(_) => return Vec::new(),
        };
        stmt.query_map(rusqlite::params![match_expr, k as i64], |row| {
            Ok(FactHit {
                scope: row.get(0)?,
                file_name: row.get(1)?,
                description: row.get(2)?,
                body: row.get(3)?,
            })
        })
        .map(|iter| iter.filter_map(Result::ok).collect::<Vec<_>>())
        .unwrap_or_default()
    }
}

fn fts_query(query: &str) -> String {
    let tokens: Vec<String> = query
        .split_whitespace()
        .map(|t| t.replace('"', "\"\""))
        .map(|t| format!("\"{t}\""))
        .collect();
    tokens.join(" OR ")
}
