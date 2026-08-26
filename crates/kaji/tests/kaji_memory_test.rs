use async_trait::async_trait;
use kaji::conversation::message::Message;
use kaji::kaji::{
    curation_due, fact_index_path, ingest_turn, latest_user_instruction, project_facts_dir,
    run_curation, splice_memory_block, user_facts_dir, SessionMemory, CURATE_DEBOUNCE_SECS,
    CURATE_MIN_PENDING,
};
use kaji::providers::base::{stream_from_single_message, MessageStream, Provider};
use kaji_core::facts::{FactStore, FactType};
use kaji_providers::conversation::token_usage::{ProviderUsage, Usage};
use kaji_providers::errors::ProviderError;
use kaji_providers::model::ModelConfig;
use rmcp::model::Tool;
use std::sync::Arc;

/// Temporarily point the memory data dir at a fresh temp root, serialized
/// against the rest of the suite (env_lock keeps one process-wide mutex for
/// env access). A per-test root isolates the shared store between tests.
fn with_root(f: impl FnOnce()) {
    let guard = env_lock::lock_env([(
        "KAJI_PATH_ROOT",
        Some(tempfile::tempdir().unwrap().path().to_str().unwrap()),
    )]);
    f();
    drop(guard);
}

#[test]
fn splice_memory_block_appends_recalled_facts() {
    with_root(|| {
        {
            let mut mem = SessionMemory::load("splice-session");
            mem.remember("Onboarding lives on the PO dashboard", &["po"], None);
        }
        let prompt = "You are KAJI. You have standard PI.";

        let spliced = splice_memory_block(prompt, "some-session", "po dashboard");
        assert!(
            spliced.contains("KAJI memory"),
            "any session recalls facts recorded by another session"
        );
        assert!(spliced.contains("Onboarding lives"));
        assert!(
            spliced.contains("splice-session"),
            "hit tagged with its source"
        );

        let spliced = splice_memory_block(prompt, "splice-session", "po dashboard");
        assert!(spliced.contains("KAJI memory"));
        assert!(spliced.contains("Onboarding lives"));
    })
}

#[test]
fn no_relevant_facts_yields_unchanged_prompt() {
    with_root(|| {
        let prompt = "You are KAJI.";
        let spliced = splice_memory_block(prompt, "empty-session", "nothing about this");
        assert_eq!(spliced, prompt);
    })
}

#[test]
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
    with_root(|| {
        let mut mem = SessionMemory::load("ingest-session");
        mem.ingest("Network the raspberry pi and deploy the gateway");
        assert_eq!(mem.recall("raspberry gateway", 5).hits.len(), 1);

        mem.ingest("Network the raspberry pi and deploy the gateway");
        let hits = mem.recall("raspberry gateway", 5);
        assert_eq!(hits.hits.len(), 1, "same body must be deduplicated");
    })
}

#[test]
fn ingest_turn_records_user_instructions_without_duplicating() {
    with_root(|| {
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
    })
}

#[test]
fn session_memory_recall_and_persist() {
    with_root(|| {
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
    })
}

#[test]
fn session_memory_reloads_from_disk() {
    with_root(|| {
        {
            let mut mem = SessionMemory::load("persist-session");
            mem.remember("persisted across process restarts", &["persist"], None);
        }
        let mem = SessionMemory::load("persist-session");
        let hits = mem.recall("persist", 1);
        assert_eq!(hits.hits.len(), 1);
        assert!(hits.hits[0].body.contains("persisted"));
    })
}

#[test]
fn recall_prompt_renders_block_with_anchored_context() {
    with_root(|| {
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
    })
}

#[test]
fn session_memory_anchored_view() {
    with_root(|| {
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
    })
}

/// Migration folds a legacy v1 per-session DB into the shared store, tagged
/// with the source session, and renames the file so it isn't imported twice.
#[test]
fn migration_folds_legacy_session_files_into_shared() {
    with_root(|| {
        let memory_dir = kaji::config::paths::Paths::in_data_dir("kaji/memory");
        std::fs::create_dir_all(&memory_dir).unwrap();
        let legacy_path = memory_dir.join("old-session.db");
        {
            // Create a v1 per-session database (legacy schema without session_id
            // column) using the raw Memory API at the legacy path.
            let mut legacy = kaji_core::memory::Memory::open(&legacy_path).unwrap();
            legacy.remember("legacy fact from an old session", &["legacy"], None);
        }

        // Loading any brand-new session triggers the one-time migration.
        let mem = SessionMemory::load("new-session");
        let hits = mem.recall("legacy fact", 5);
        assert!(hits.hits.iter().any(|h| h.body.contains("legacy")));
        assert!(
            hits.hits.iter().any(|h| h.session_id == "old-session"),
            "imported entries carry their source session"
        );

        // The legacy file was renamed away so re-opening cannot double-import.
        assert!(!legacy_path.exists(), "legacy DB renamed after import");
    })
}

/// Isolate both the data dir and the memory dir override, keeping the temp root
/// alive for the duration of the test so path assertions can reference it.
fn with_scope_root(f: impl FnOnce(&std::path::Path)) {
    let tmp = tempfile::tempdir().unwrap();
    let guard = env_lock::lock_env([
        ("KAJI_PATH_ROOT", Some(tmp.path().to_str().unwrap())),
        ("KAJI_MEMORY_DIR", None),
    ]);
    f(tmp.path());
    drop(guard);
}

#[test]
fn project_facts_dir_uses_git_root_or_data_dir() {
    with_scope_root(|root| {
        let repo = root.join("repo/nested");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(root.join("repo/.git")).unwrap();
        assert_eq!(project_facts_dir(&repo), root.join("repo/.kaji/memory"));

        let outside = root.join("nogit");
        std::fs::create_dir_all(&outside).unwrap();
        let dir = project_facts_dir(&outside);
        assert!(
            dir.starts_with(root.join("data")),
            "outside a repo the project scope falls back to the data dir: {}",
            dir.display()
        );
        assert!(dir.to_string_lossy().contains("projects"));
    })
}

#[test]
fn user_facts_dir_lives_under_the_memory_dir() {
    with_scope_root(|root| {
        assert_eq!(user_facts_dir(), root.join("data/kaji/memory/user"));
    })
}

#[test]
fn curation_due_needs_threshold_and_debounce() {
    with_root(|| {
        let session = "curation-due-session";
        assert!(
            !curation_due(session, 1_000),
            "an empty journal never fires"
        );

        let mut mem = SessionMemory::load(session);
        for i in 0..CURATE_MIN_PENDING {
            mem.ingest(&format!("uncurated decision number {i} about the gateway"));
        }
        drop(mem);

        assert!(
            curation_due(session, 1_000),
            "threshold reached with a cold debounce arms the trigger"
        );
        assert!(
            !curation_due(session, 1_000),
            "the winning caller stamped the debounce; the next turn is a no-op"
        );
        assert!(
            !curation_due(session, 1_000 + CURATE_DEBOUNCE_SECS - 1),
            "still inside the debounce window"
        );
        assert!(
            curation_due(session, 1_000 + CURATE_DEBOUNCE_SECS),
            "past the window an unchanged backlog arms the trigger again"
        );
    })
}

/// Replies with a canned assistant message, standing in for the curator model.
struct ScriptedProvider {
    reply: String,
}

#[async_trait]
impl Provider for ScriptedProvider {
    fn get_name(&self) -> &str {
        "scripted-curator"
    }

    async fn stream(
        &self,
        _model_config: &ModelConfig,
        _system: &str,
        _messages: &[Message],
        _tools: &[Tool],
    ) -> Result<MessageStream, ProviderError> {
        Ok(stream_from_single_message(
            Message::assistant().with_text(self.reply.clone()),
            ProviderUsage::new("scripted".to_string(), Usage::default()),
        ))
    }
}

/// End-to-end through the spawned body of the trigger: a well-formed curator
/// reply writes facts and stamps the batch, a malformed one leaves the journal
/// untouched so the entries replay on the next trigger.
#[tokio::test]
async fn run_curation_writes_facts_and_stamps_only_on_success() {
    let tmp = tempfile::tempdir().unwrap();
    let guard = env_lock::lock_env([
        ("KAJI_PATH_ROOT", Some(tmp.path().to_str().unwrap())),
        ("KAJI_MEMORY_DIR", None),
        ("KAJI_MEMORY_CURATOR_MODEL", Some("main-model")),
    ]);

    let session = "run-curation-session";
    let working_dir = tmp.path().join("workspace");
    std::fs::create_dir_all(&working_dir).unwrap();
    let model_config = ModelConfig::new("main-model");

    let mut mem = SessionMemory::load(session);
    for i in 0..CURATE_MIN_PENDING {
        mem.ingest(&format!("journal entry number {i} about the gateway"));
    }
    drop(mem);

    let prose = Arc::new(ScriptedProvider {
        reply: "désolé, voici les faits en prose".to_string(),
    }) as Arc<dyn Provider>;
    run_curation(
        prose,
        "scripted-curator".to_string(),
        model_config.clone(),
        session.to_string(),
        working_dir.clone(),
    )
    .await;

    let facts = FactStore::new(project_facts_dir(&working_dir));
    assert!(facts.list().is_empty(), "a failed run writes no fact");
    assert_eq!(
        SessionMemory::load(session).uncurated(50).len(),
        CURATE_MIN_PENDING,
        "a failed run leaves the whole batch pending for the next trigger"
    );

    let json = Arc::new(ScriptedProvider {
        reply: r#"[{"action":"create","type":"decision","slug":"passerelle","description":"la passerelle écoute sur 8080","body":"le service tourne sur le port 8080"}]"#
            .to_string(),
    }) as Arc<dyn Provider>;
    run_curation(
        json,
        "scripted-curator".to_string(),
        model_config,
        session.to_string(),
        working_dir.clone(),
    )
    .await;

    assert!(
        facts.get(&FactType::Decision, "passerelle").is_some(),
        "a successful run writes the curated fact"
    );
    assert!(
        SessionMemory::load(session).uncurated(50).is_empty(),
        "a successful run stamps the batch as curated"
    );

    drop(guard);
}

#[test]
fn fact_index_path_stays_outside_the_repo() {
    with_scope_root(|root| {
        let repo = root.join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();

        let index = fact_index_path(&repo);
        assert!(index.starts_with(root.join("data/kaji/memory/index")));
        assert!(!index.starts_with(&repo), "the index is never versioned");
        assert_eq!(index.extension().and_then(|e| e.to_str()), Some("db"));

        assert_ne!(
            index,
            fact_index_path(&root.join("other")),
            "each working dir gets its own index"
        );
    })
}
