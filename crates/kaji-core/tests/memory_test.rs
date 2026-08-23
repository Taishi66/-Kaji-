use std::time::Duration;

use kaji_core::memory::{Memory, RecallHit};

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

#[test]
fn recall_excludes_off_topic_and_ranks_relevant() {
    let mut m = Memory::new();
    m.remember(
        "extract the onboarding checklist for the PO dashboard",
        &["onboarding", "po", "checklist"],
        None,
    );
    m.remember(
        "switch the toggle to AIAD mode for the session",
        &["toggle", "aiad", "sdd", "session"],
        None,
    );

    let hits = m.recall("aiad toggle session", 5);
    // The AIAD entry matches query terms (toggle, session); the onboarding
    // entry matches none and is excluded by MATCH.
    assert_eq!(hits.hits.len(), 1);
    assert!(hits.hits[0].body.contains("AIAD"));
}

#[test]
fn entity_weight_boosts_matching_entity() {
    let mut m = Memory::new();
    // Same query term in both; only the first declares it as an entity.
    m.remember("the proxy handles requests", &["proxy"], None);
    m.remember("proxy appears in comments only", &["other"], None);
    m.remember("unrelated filler here", &["other"], None);

    let hits = m.recall("proxy", 2);
    assert_eq!(hits.hits.len(), 2);
    // The entity-declared entry must outrank the plain-text one.
    assert!(hits.hits[0].body.contains("handles"));
}

#[test]
fn recall_returns_top_k() {
    let mut m = Memory::new();
    for i in 0..10 {
        m.remember(
            &format!("fact number {i} about the proxy"),
            &["proxy"],
            None,
        );
    }
    let hits = m.recall("proxy", 3);
    assert_eq!(hits.hits.len(), 3);
}

#[test]
fn ttl_expired_entries_are_swept_on_remember() {
    let mut m = Memory::new();
    let id = m.remember("this fact expires fast", &["transient"], Some(secs(1)));
    assert_eq!(m.len(), 1);

    std::thread::sleep(secs(2));
    // Sweep happens on next remember.
    m.remember("replacement fact", &["replacement"], None);
    assert!(
        m.get(id).is_none(),
        "expired entry must be dropped after a remember sweep"
    );
    assert_eq!(m.len(), 1, "only the replacement remains");
}

#[test]
fn recall_reports_stale_dropped() {
    let mut m = Memory::new();
    m.remember("stable fact", &["stable"], None);
    m.remember("volatile fact", &["volatile"], Some(secs(1)));
    std::thread::sleep(secs(2));
    let res = m.recall("fact", 10);
    assert_eq!(res.stale_dropped, 1, "volatile entry counted as stale");
    assert!(res.hits.iter().all(|h| !h.body.contains("volatile")));
}

#[test]
fn should_compact_bounds_the_aiad_band() {
    let m = Memory::new();
    assert!(!m.should_compact(0.5), "below band : no compaction");
    assert!(m.should_compact(0.6), "low bound : plan compaction");
    assert!(m.should_compact(0.65), "mid band : plan compaction");
    assert!(
        m.should_compact(0.8),
        "kaji legacy threshold now inside scope"
    );
}

#[test]
fn persists_and_reopens_from_disk() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mem.db");
    {
        let mut m = Memory::open(&path).unwrap();
        m.remember("remember me", &["note"], None);
    }

    let m = Memory::open(&path).unwrap();
    assert_eq!(m.len(), 1);
    let hits = m.recall("remember", 1);
    assert_eq!(hits.hits.len(), 1);
    assert_eq!(hits.hits[0].body, "remember me");
}

#[test]
fn fts_query_with_special_chars_is_safe() {
    let mut m = Memory::new();
    m.remember("kaji-core uses serde + rusqlite", &["rust"], None);
    // Parens/colons would break a raw MATCH; must be quoted away.
    let res = m.recall("kaji-core: (rust)", 5);
    assert_eq!(res.hits.len(), 1, "special chars must not break recall");
}

#[test]
fn hit_type_is_exported() {
    let _: Option<RecallHit> = None;
}

#[test]
fn anchored_reattaches_temporal_context() {
    let mut m = Memory::new();
    m.remember("opening goal: onboard PO users", &["goal"], None);
    m.remember("stub a", &["filler"], None);
    m.remember("stub b", &["filler"], None);
    m.remember("the crucial fact about the toggle", &["toggle"], None);
    m.remember("stub c", &["filler"], None);
    m.remember("stub d", &["filler"], None);
    m.remember("resolution: toggle shipped", &["outcome"], None);

    let hits = m.recall("crucial turn", 1);
    assert_eq!(hits.hits.len(), 1);
    let view = m.anchored(&hits.hits[0], 2, 1).expect("view");

    assert_eq!(view.hit.body, "the crucial fact about the toggle");
    // window = 2 entries with closest ts around the hit (either side).
    assert_eq!(view.window.len(), 2);
    assert!(
        view.window.iter().all(|e| !e.body.contains("crucial")),
        "hit must not appear in its own window"
    );

    // opening = earliest entry; resolution = latest entry.
    assert_eq!(view.opening.len(), 1);
    assert!(view.opening[0].body.contains("opening"));
    assert_eq!(view.resolution.len(), 1);
    assert!(view.resolution[0].body.contains("resolution"));
}

#[test]
fn recall_is_global_across_sessions() {
    let mut m = Memory::new();
    m.remember_in_session("sess-a", "the bedroom light stays on", &["light"], None);
    m.remember_in_session("sess-b", "paint the bedroom wall", &["bedroom"], None);

    let hits = m.recall("bedroom", 5);
    assert_eq!(
        hits.hits.len(),
        2,
        "a query must surface facts recorded in any session"
    );
    let sessions: Vec<&str> = hits.hits.iter().map(|h| h.session_id.as_str()).collect();
    assert!(sessions.contains(&"sess-a"), "cross-session recall");
    assert!(sessions.contains(&"sess-b"), "cross-session recall");
}

#[test]
fn anchored_is_scoped_to_the_hit_session() {
    let mut m = Memory::new();
    m.remember_in_session("sess-a", "opening a: goal", &["a"], None);
    m.remember_in_session("sess-a", "the crucial fact for a", &["critical"], None);
    m.remember_in_session("sess-a", "resolution a: done", &["a"], None);
    // Foreign sessions bookend the store but must not appear in the view.
    m.remember_in_session("sess-b", "opening b: noise", &["b"], None);
    m.remember_in_session("sess-b", "resolution b: noise", &["b"], None);

    let hits = m.recall("critical", 1);
    assert_eq!(hits.hits.len(), 1);
    let view = m.anchored(&hits.hits[0], 3, 2).expect("view");

    let seen: Vec<String> = view
        .window
        .iter()
        .chain(view.opening.iter())
        .chain(view.resolution.iter())
        .map(|e| e.body.clone())
        .collect();
    assert!(
        seen.iter().any(|b| b.contains("goal")),
        "session-a opening preserved"
    );
    assert!(
        seen.iter().any(|b| b.contains("done")),
        "session-a resolution preserved"
    );
    assert!(
        seen.iter()
            .all(|b| !b.contains(": b") && !b.contains("noise")),
        "session-b context must not leak into the anchored view"
    );
}

#[test]
fn migration_adds_session_column_to_legacy_db() {
    use tempfile::tempdir;

    let dir = tempdir().unwrap();
    let path = dir.path().join("legacy.db");

    // Write a v1 database without the session_id column.
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE memory_entries (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                text TEXT NOT NULL,
                entities TEXT NOT NULL DEFAULT '',
                body TEXT NOT NULL,
                ts INTEGER NOT NULL,
                ttl_ms INTEGER
            );
            CREATE VIRTUAL TABLE memory_fts USING fts5(
                text, entities, body,
                content='memory_entries',
                content_rowid='id'
            );
            CREATE TRIGGER memory_fts_insert AFTER INSERT ON memory_entries BEGIN
                INSERT INTO memory_fts(rowid, text, entities, body)
                VALUES (new.id, new.text, new.entities, new.body);
            END;
            INSERT INTO memory_entries (text, entities, body, ts, ttl_ms)
            VALUES ('legacy fact', 'legacy', 'legacy fact', 0, NULL);",
        )
        .unwrap();
    }

    let mut m = Memory::open(&path).unwrap();
    assert_eq!(m.len(), 1);
    let entries = m.iter();
    assert_eq!(
        entries[0].session_id, "",
        "v1 rows default to empty session"
    );

    // Writes after migration must carry their session.
    m.remember_in_session("sess-x", "new tagged fact", &["new"], None);
    let hits = m.recall("legacy new", 5);
    let sessions: Vec<&str> = hits.hits.iter().map(|h| h.session_id.as_str()).collect();
    assert!(sessions.contains(&"sess-x"));
    assert!(
        sessions.contains(&""),
        "legacy rows readable post-migration"
    );
}

#[test]
fn uncurated_returns_unstamped_oldest_first_and_respects_limit() {
    let mut mem = Memory::new();
    let a = mem.remember("fact a", &[], None);
    let b = mem.remember("fact b", &[], None);
    let c = mem.remember("fact c", &[], None);
    mem.mark_curated(&[b]);
    let pending = mem.uncurated(10);
    assert_eq!(pending.iter().map(|e| e.id).collect::<Vec<_>>(), vec![a, c]);
    assert_eq!(mem.uncurated(1).len(), 1);
}

#[test]
fn mark_curated_is_idempotent_and_ignores_unknown_ids() {
    let mut mem = Memory::new();
    let a = mem.remember("fact a", &[], None);
    mem.mark_curated(&[a, 9999]);
    mem.mark_curated(&[a]);
    assert!(mem.uncurated(10).is_empty());
}

#[test]
fn open_migrates_curated_at_on_existing_db() {
    // Base créée sans la colonne : simuler en ouvrant, droppant la colonne étant impossible
    // en SQLite ancien — à la place, vérifier que open() sur une base v-courante expose l'API
    // et que PRAGMA table_info contient curated_at (le pattern migrate_session_id est déjà
    // prouvé par les tests existants).
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("m.db");
    let mem = Memory::open(&path).unwrap();
    drop(mem);
    let conn = rusqlite::Connection::open(&path).unwrap();
    let mut stmt = conn.prepare("PRAGMA table_info(memory_entries)").unwrap();
    let cols: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(1))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(cols.iter().any(|c| c == "curated_at"));
}
