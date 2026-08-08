use anyhow::Result;
use kaji::kaji::{MemoryEntry, SessionMemory};

use crate::cli::MemoryCommand;

/// Default display width for the memory body column.
const MAX_BODY_WIDTH: usize = 80;

pub async fn handle_memory_subcommand(command: MemoryCommand) -> Result<()> {
    match command {
        MemoryCommand::List {
            session,
            limit,
            format,
        } => list(session.as_deref(), limit, &format),
        MemoryCommand::Clear { session, all } => clear(session.as_deref(), all),
    }
}

/// Snapshot the shared store, filtering to `session` when provided ("" matches
/// legacy/global entries), capped at `limit`, newest first.
fn entries(session: Option<&str>, limit: Option<usize>) -> Vec<MemoryEntry> {
    let mem = SessionMemory::load("");
    let mut all = mem.list();
    if let Some(sid) = session {
        all.retain(|e| e.session_id == sid);
    }
    all.sort_by_key(|e| std::cmp::Reverse((e.ts, e.id)));
    if let Some(limit) = limit {
        all.truncate(limit);
    }
    all
}

fn list(session: Option<&str>, limit: Option<usize>, format: &str) -> Result<()> {
    let rows = entries(session, limit);
    match format {
        "json" => {
            let out: Vec<serde_json::Value> = rows
                .iter()
                .map(|e| {
                    serde_json::json!({
                        "id": e.id,
                        "ts": e.ts,
                        "session_id": e.session_id,
                        "body": e.body,
                    })
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&out)?);
        }
        _ => print_table(&rows, session),
    }
    Ok(())
}

fn print_table(rows: &[MemoryEntry], session: Option<&str>) {
    let summary = match session {
        Some(sid) => format!("memory ({sid})"),
        None => "memory (all sessions)".to_string(),
    };
    println!("{summary}: {} entries", rows.len());
    println!();
    if rows.is_empty() {
        println!("(none)");
        return;
    }
    // Relative timestamp bucket (just now / today / recent / older).
    let rendered = rows
        .iter()
        .map(|e| (render_ts(e.ts), preview(&e.body)))
        .collect::<Vec<_>>();
    let ts_w = rendered
        .iter()
        .map(|(t, _)| t.chars().count())
        .max()
        .unwrap_or(0);
    let body_w = rendered
        .iter()
        .map(|(_, b)| b.chars().count())
        .max()
        .unwrap_or(0);
    for ((ts, body), e) in rendered.iter().zip(rows.iter()) {
        let sid = if e.session_id.is_empty() {
            "(legacy)".to_string()
        } else {
            e.session_id.clone()
        };
        println!(
            "  #{:<6} {:<ts_w$}  {:<body_w$}  [{}]",
            e.id,
            ts,
            body,
            sid,
            ts_w = ts_w,
            body_w = body_w
        );
    }
}

fn preview(text: &str) -> String {
    let single = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if single.chars().count() <= MAX_BODY_WIDTH {
        return single;
    }
    if MAX_BODY_WIDTH <= 3 {
        return ".".repeat(MAX_BODY_WIDTH);
    }
    let mut out: String = single.chars().take(MAX_BODY_WIDTH - 3).collect();
    out.push_str("...");
    out
}

fn render_ts(secs: u64) -> String {
    const DAY: u64 = 86400;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if secs + 60 * 60 > now {
        "just now".to_string()
    } else if secs + DAY > now {
        "today".to_string()
    } else if secs + 7 * DAY > now {
        "recent".to_string()
    } else {
        "older".to_string()
    }
}

fn clear(session: Option<&str>, all: bool) -> Result<()> {
    let mut mem = SessionMemory::load("");
    let removed = if all {
        mem.clear(None)
    } else {
        match session {
            Some(sid) => mem.clear(Some(sid)),
            None => {
                anyhow::bail!(
                    "nothing to remove: pass --all to clear the whole store or a session ID to \
                     target one session"
                );
            }
        }
    };
    println!("removed {removed} entries");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Seed the shared store under an isolated `KAJI_MEMORY_DIR`.
    fn seed(tagged: &[(&'static str, &'static str)]) {
        for &(session, body) in tagged {
            let mut mem = SessionMemory::load(session);
            mem.remember(body, &["fact"], None);
        }
    }

    fn with_root(f: impl FnOnce()) {
        let guard = env_lock::lock_env([(
            "KAJI_MEMORY_DIR",
            Some(tempfile::tempdir().unwrap().path().to_str().unwrap()),
        )]);
        f();
        drop(guard);
    }

    #[test]
    fn entries_filter_and_order() {
        with_root(|| {
            seed(&[("s1", "alpha"), ("s2", "beta"), ("", "legacy-item")]);
            let all = entries(None, None);
            assert_eq!(all.len(), 3);

            let s2 = entries(Some("s2"), None);
            assert_eq!(s2.len(), 1);
            assert_eq!(s2[0].body, "beta");

            let legacy = entries(Some(""), None);
            assert_eq!(legacy.len(), 1);
            assert_eq!(legacy[0].body, "legacy-item");

            let limited = entries(None, Some(2));
            assert_eq!(limited.len(), 2);
        })
    }

    #[test]
    fn clear_targets_one_session() {
        with_root(|| {
            seed(&[("s-a", "alpha"), ("s-b", "beta")]);
            clear(Some("s-a"), false).unwrap();
            assert_eq!(entries(None, None).len(), 1);
            assert_eq!(entries(Some("s-a"), None).len(), 0);
        })
    }

    #[test]
    fn clear_requires_session_or_all() {
        with_root(|| {
            seed(&[("s-a", "alpha")]);
            assert!(clear(None, false).is_err());
            clear(None, true).unwrap();
            assert_eq!(entries(None, None).len(), 0);
        })
    }

    #[test]
    fn render_ts_buckets() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert_eq!(render_ts(now), "just now");
        assert_eq!(render_ts(now - 3 * 86400), "recent");
        assert_eq!(render_ts(now - 10 * 86400), "older");
    }

    #[test]
    fn preview_truncates_long_bodies() {
        let short = "short fact";
        assert_eq!(preview(short), short);
        let long = "x".repeat(200);
        let out = preview(&long);
        assert!(out.chars().count() == MAX_BODY_WIDTH);
        assert!(out.ends_with("..."));
    }
}
