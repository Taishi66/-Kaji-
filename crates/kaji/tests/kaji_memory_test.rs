use std::sync::Once;

use kaji::conversation::message::Message;
use kaji::kaji::{ingest_turn, latest_user_instruction, splice_memory_block, SessionMemory};

#[test]
fn splice_memory_block_appends_recalled_facts() {
    init();
    {
        let mut mem = SessionMemory::load("splice-session");
        mem.remember("Onboarding lives on the PO dashboard", &["po"], None);
    }
    let prompt = "You are KAJI. You have standard PI.";

    let spliced = splice_memory_block(prompt, "some-session", "po dashboard");
    assert_eq!(spliced, prompt, "unknown session yields unchanged prompt");

    let spliced = splice_memory_block(prompt, "splice-session", "po dashboard");
    assert!(spliced.contains("KAJI memory"));
    assert!(spliced.contains("Onboarding lives"));
}

fn latest_user_instruction_extracts_recent_text() {
    let messages = vec![
        Message::user().with_text("setup the workspace"),
        Message::assistant().with_text("I'll set that up."),
        Message::user().with_text("now list the todos"),
    ];
    assert_eq!(
        latest_user_instruction(&messages).as_deref(),
        Some("now list the todos")
    );
    assert_eq!(
        latest_user_instruction(&[]),
        None,
        "no user message yields None"
    );
}

#[test]
fn ingest_is_idempotent_and_extracts_entities() {
    let mut mem = SessionMemory::load("ingest-session");
    mem.ingest("Network the raspberry pi and deploy the gateway");
    assert_eq!(mem.recall("raspberry gateway", 5).hits.len(), 1);

    mem.ingest("Network the raspberry pi and deploy the gateway");
    let hits = mem.recall("raspberry gateway", 5);
    assert_eq!(hits.hits.len(), 1, "same body must be deduplicated");
}

#[test]
fn ingest_turn_records_user_instructions_without_duplicating() {
    let messages = vec![
        Message::user().with_text("Kick off the onboarding pipeline"),
        Message::assistant().with_text("I've set the pipeline up."),
        Message::user().with_text("Kick off the onboarding pipeline"),
    ];
    ingest_turn("ingest-turn-session", &messages);

    let mem = SessionMemory::load("ingest-turn-session");
    let hits = mem.recall("onboarding pipeline", 5);
    assert_eq!(hits.hits.len(), 1, "duplicate instruction stored once");
    assert!(hits.hits[0].body.contains("onboarding"));
}

fn init() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let root = std::env::temp_dir().join("kaji-kaji-test");
        std::env::set_var("KAJI_PATH_ROOT", root);
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
fn recall_prompt_renders_block_with_anchored_context() {
    init();
    let mem = SessionMemory::load("prompt-session");
    let empty = mem.recall_prompt("nothing relevant stored", 3);
    assert!(empty.is_none(), "empty store yields no block");

    let mut mem = SessionMemory::load("prompt-block-session");
    mem.remember("goal: ship the toggle", &["goal"], None);
    mem.remember("the toggle fact lives in config.rs", &["toggle"], None);
    mem.remember("outcome: toggle shipped", &["outcome"], None);

    let block = mem.recall_prompt("toggle config", 1).expect("block");
    assert!(block.contains("KAJI memory"));
    assert!(block.contains("toggle fact"));
    assert!(block.contains("opening"));
    assert!(block.contains("resolution"));
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
