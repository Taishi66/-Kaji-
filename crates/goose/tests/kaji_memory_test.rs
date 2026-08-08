use std::sync::Once;

use goose::kaji::SessionMemory;

fn init() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let root = std::env::temp_dir().join("kaji-goose-test");
        std::env::set_var("GOOSE_PATH_ROOT", root);
    });
}

#[test]
fn session_memory_recall_and_persist() {
    init();
    let mut mem = SessionMemory::load("integration-session");
    mem.remember(
        "The onboarding checklist lives on the PO dashboard",
        &["po", "onboarding"],
        None,
    );
    mem.remember(
        "Toggle switched to AIAD mode this session",
        &["aiad", "toggle"],
        None,
    );
    mem.remember(
        "volatile config knob (expired)",
        &["volatile"],
        Some(std::time::Duration::from_secs(1)),
    );

    assert!(mem.should_compact(0.60), "AIAD budget low bound triggers");
    assert!(!mem.should_compact(0.4), "below band no compaction");

    let hits = mem.recall("aiad toggle", 2);
    assert!(hits.hits.iter().any(|h| h.body.contains("AIAD")));
}

#[test]
fn session_memory_reloads_from_disk() {
    init();
    {
        let mut mem = SessionMemory::load("persist-session");
        mem.remember("persisted across process restarts", &["persist"], None);
    }
    let mem = SessionMemory::load("persist-session");
    let hits = mem.recall("persist", 1);
    assert_eq!(hits.hits.len(), 1);
    assert!(hits.hits[0].body.contains("persisted"));
}

#[test]
fn session_memory_anchored_view() {
    init();
    let mut mem = SessionMemory::load("anchored-session");
    mem.remember("goal: ship the toggle", &["goal"], None);
    mem.remember("filler before", &["filler"], None);
    mem.remember("the toggle fact", &["toggle"], None);
    mem.remember("filler after", &["filler"], None);
    mem.remember("outcome: toggle live", &["outcome"], None);

    let hits = mem.recall("toggle", 1);
    assert_eq!(hits.hits.len(), 1);
    let view = mem.anchored(&hits.hits[0], 2, 1).expect("anchored");
    assert!(view.opening.iter().any(|e| e.body.contains("goal")));
    assert!(view.resolution.iter().any(|e| e.body.contains("outcome")));
    assert_eq!(view.window.len(), 2);
}
