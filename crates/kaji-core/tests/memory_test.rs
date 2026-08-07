use std::time::Duration;

use kaji_core::memory::Memory;

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

#[test]
fn recall_ranks_entity_hits_first() {
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

    let hits = m.recall("aiad toggle session", 2);
    assert_eq!(hits.hits.len(), 2);
    assert!(
        hits.hits[0].body.contains("AIAD"),
        "AIAD entry should rank top"
    );
    assert!(hits.hits[0].score > hits.hits[1].score);
}

#[test]
fn recall_returns_top_k_and_default_0_tokens() {
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

    // Force age past the TTL.
    m.forget_where(|_| false); // no-op, keep structure explicit
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
        "goose legacy threshold now inside scope"
    );
}

#[test]
fn persists_and_loads_roundtrip() {
    let mut m = Memory::new();
    m.remember("remember me", &["note"], None);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mem.json");
    std::fs::write(&path, serde_json::to_string(&m).unwrap()).unwrap();

    let raw = std::fs::read_to_string(&path).unwrap();
    let loaded: kaji_core::memory::Memory = serde_json::from_str(&raw).unwrap();
    assert_eq!(loaded.len(), 1);
    let hits = loaded.recall("remember", 1);
    assert_eq!(hits.hits.len(), 1);
    assert_eq!(hits.hits[0].body, "remember me");
}
