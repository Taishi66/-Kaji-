use std::path::Path;

use anyhow::Result;
use kaji::config::Config;
use kaji::kaji::{project_facts_dir, user_facts_dir, MemoryEntry, SessionMemory};
use kaji::memory_curator::CurationOutcome;
use kaji::model_config::model_config_from_user_config;
use kaji_core::facts::{Fact, FactStore};

use crate::cli::MemoryCommand;

/// Default display width for the memory body column.
const MAX_BODY_WIDTH: usize = 80;

/// Session stamped on facts written by an explicit `kaji memory curate`, which
/// runs outside any agent session.
const CURATE_SESSION_ID: &str = "cli";

pub async fn handle_memory_subcommand(command: MemoryCommand) -> Result<()> {
    match command {
        MemoryCommand::List {
            session,
            limit,
            format,
            curated,
            raw: _,
        } => {
            if curated {
                list_curated(session.as_deref(), limit, &format)
            } else {
                list(session.as_deref(), limit, &format)
            }
        }
        MemoryCommand::Curate => curate().await,
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

/// Curated facts of both scopes, project first, sorted by type then slug within
/// a scope. `session` matches the session that recorded the fact.
fn curated_facts(
    working_dir: &Path,
    session: Option<&str>,
    limit: Option<usize>,
) -> Vec<(&'static str, Fact)> {
    let scopes = [
        ("project", FactStore::new(project_facts_dir(working_dir))),
        ("user", FactStore::new(user_facts_dir())),
    ];
    let mut rows = Vec::new();
    for (scope, store) in scopes {
        let mut facts = store.list();
        if let Some(sid) = session {
            facts.retain(|fact| fact.session == sid);
        }
        facts.sort_by(|a, b| {
            a.fact_type
                .as_str()
                .cmp(b.fact_type.as_str())
                .then_with(|| a.slug.cmp(&b.slug))
        });
        rows.extend(facts.into_iter().map(|fact| (scope, fact)));
    }
    if let Some(limit) = limit {
        rows.truncate(limit);
    }
    rows
}

fn render_curated_table(rows: &[(&'static str, Fact)]) -> String {
    if rows.is_empty() {
        return "(none)\n".to_string();
    }
    let mut scope_w = "SCOPE".len();
    let mut type_w = "TYPE".len();
    let mut slug_w = "SLUG".len();
    for (scope, fact) in rows {
        scope_w = scope_w.max(scope.chars().count());
        type_w = type_w.max(fact.fact_type.as_str().chars().count());
        slug_w = slug_w.max(fact.slug.chars().count());
    }
    let mut out = format!(
        "{:<scope_w$}  {:<type_w$}  {:<slug_w$}  DESCRIPTION\n",
        "SCOPE", "TYPE", "SLUG"
    );
    for (scope, fact) in rows {
        out.push_str(&format!(
            "{:<scope_w$}  {:<type_w$}  {:<slug_w$}  {}\n",
            scope,
            fact.fact_type.as_str(),
            fact.slug,
            preview(&fact.description)
        ));
    }
    out
}

fn list_curated(session: Option<&str>, limit: Option<usize>, format: &str) -> Result<()> {
    let working_dir = std::env::current_dir()?;
    let rows = curated_facts(&working_dir, session, limit);
    match format {
        "json" => {
            let out: Vec<serde_json::Value> = rows
                .iter()
                .map(|(scope, fact)| {
                    serde_json::json!({
                        "scope": scope,
                        "type": fact.fact_type.as_str(),
                        "slug": fact.slug,
                        "description": fact.description,
                    })
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&out)?);
        }
        _ => print!("{}", render_curated_table(&rows)),
    }
    Ok(())
}

/// A run that wrote some facts and dropped others leaves its journal batch
/// pending, so the failure count is surfaced rather than folded into a success
/// line.
fn curation_summary(outcome: &CurationOutcome) -> String {
    let memorized = outcome.created + outcome.updated;
    if outcome.failed > 0 {
        format!(
            "記 {memorized} faits mémorisés — {} échoués, le lot sera rejoué",
            outcome.failed
        )
    } else if memorized == 0 {
        "記 0 faits mémorisés — rien à curer".to_string()
    } else {
        format!("記 {memorized} faits mémorisés")
    }
}

/// Run one curation now, on the provider and model the CLI is configured with.
async fn curate() -> Result<()> {
    let config = Config::global();
    let Ok(provider_name) = config.get_kaji_provider() else {
        anyhow::bail!("aucun provider configuré — lancez `kaji configure`");
    };
    let Ok(model_name) = config.get_kaji_model() else {
        anyhow::bail!("aucun modèle configuré — lancez `kaji configure`");
    };
    let model_config = model_config_from_user_config(&provider_name, &model_name)?;
    let provider = kaji::providers::create(&provider_name, Vec::new()).await?;
    let working_dir = std::env::current_dir()?;

    let outcome = kaji::memory_curator::curate(
        provider.as_ref(),
        provider.get_name(),
        &model_config,
        CURATE_SESSION_ID,
        &working_dir,
    )
    .await?;

    println!("{}", curation_summary(&outcome));
    Ok(())
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
    use kaji_core::facts::{CreatedBy, FactType};

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

    /// Isolate the data dir and drop any `KAJI_MEMORY_DIR` override, so both
    /// fact scopes resolve under the temp root.
    fn with_scope_root(f: impl FnOnce(&std::path::Path)) {
        let tmp = tempfile::tempdir().unwrap();
        let guard = env_lock::lock_env([
            ("KAJI_PATH_ROOT", Some(tmp.path().to_str().unwrap())),
            ("KAJI_MEMORY_DIR", None),
        ]);
        f(tmp.path());
        drop(guard);
    }

    fn fact(fact_type: FactType, slug: &str, description: &str) -> Fact {
        Fact {
            fact_type,
            slug: slug.to_string(),
            description: description.to_string(),
            date: "2026-08-22".to_string(),
            session: "s1".to_string(),
            created_by: CreatedBy::Curator,
            body: "corps du fait".to_string(),
        }
    }

    #[test]
    fn curated_listing_merges_both_scopes() {
        with_scope_root(|root| {
            let working_dir = root.join("workspace");
            std::fs::create_dir_all(&working_dir).unwrap();

            FactStore::new(project_facts_dir(&working_dir))
                .write(&fact(
                    FactType::Decision,
                    "cache-ttl",
                    "le cache expire au bout de 60s",
                ))
                .unwrap();
            FactStore::new(user_facts_dir())
                .write(&fact(
                    FactType::Preference,
                    "cache-warmup",
                    "préchauffer le cache au démarrage",
                ))
                .unwrap();

            let rows = curated_facts(&working_dir, None, None);
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0].0, "project");
            assert_eq!(rows[0].1.slug, "cache-ttl");
            assert_eq!(rows[1].0, "user");
            assert_eq!(rows[1].1.slug, "cache-warmup");

            let lines: Vec<String> = render_curated_table(&rows)
                .lines()
                .map(str::to_string)
                .collect();
            assert_eq!(lines.len(), 3);
            for header in ["SCOPE", "TYPE", "SLUG", "DESCRIPTION"] {
                assert!(lines[0].contains(header), "header holds {header}");
            }
            assert!(lines[1].starts_with("project"));
            assert!(lines[1].contains("decision"));
            assert!(lines[1].contains("cache-ttl"));
            assert!(lines[1].contains("le cache expire au bout de 60s"));
            assert!(lines[2].starts_with("user"));
            assert!(lines[2].contains("preference"));
            assert!(lines[2].contains("cache-warmup"));
        })
    }

    #[test]
    fn curated_listing_filters_by_session_and_limit() {
        with_scope_root(|root| {
            let working_dir = root.join("workspace");
            std::fs::create_dir_all(&working_dir).unwrap();
            let store = FactStore::new(project_facts_dir(&working_dir));
            store
                .write(&fact(FactType::Decision, "alpha", "premier"))
                .unwrap();
            let mut other = fact(FactType::Gotcha, "beta", "second");
            other.session = "s2".to_string();
            store.write(&other).unwrap();

            let filtered = curated_facts(&working_dir, Some("s2"), None);
            assert_eq!(filtered.len(), 1);
            assert_eq!(filtered[0].1.slug, "beta");

            assert_eq!(curated_facts(&working_dir, None, Some(1)).len(), 1);
        })
    }

    #[test]
    fn curation_summary_surfaces_failures_and_the_empty_run() {
        assert_eq!(
            curation_summary(&CurationOutcome::default()),
            "記 0 faits mémorisés — rien à curer"
        );
        assert_eq!(
            curation_summary(&CurationOutcome {
                created: 2,
                updated: 1,
                failed: 0
            }),
            "記 3 faits mémorisés"
        );
        assert_eq!(
            curation_summary(&CurationOutcome {
                created: 1,
                updated: 0,
                failed: 2
            }),
            "記 1 faits mémorisés — 2 échoués, le lot sera rejoué"
        );
    }

    #[test]
    fn curated_listing_renders_none_when_empty() {
        with_scope_root(|root| {
            let working_dir = root.join("vide");
            std::fs::create_dir_all(&working_dir).unwrap();
            let rows = curated_facts(&working_dir, None, None);
            assert!(rows.is_empty());
            assert_eq!(render_curated_table(&rows), "(none)\n");
        })
    }
}
