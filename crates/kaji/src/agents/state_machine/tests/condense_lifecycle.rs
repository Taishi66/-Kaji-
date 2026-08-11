//! Covers the condense transform wired into `stream_response_from_provider`
//! (Task 2 of the condense feature): once a tool-result falls outside the
//! `keep_raw_turns` freshness window, the outbound copy sent to the provider
//! has it compressed while recent turns and the stored session conversation
//! stay untouched.

use anyhow::Result;
use rmcp::model::{CallToolRequestParams, CallToolResult, ContentBlock};

use super::calculator_extension::ADD;
use super::pipeline::{test_pipeline, TestPipeline};
use crate::conversation::message::Message;

fn numbered_lines(n: usize) -> String {
    (1..=n)
        .map(|i| format!("line_{i}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Seeds a turn-one tool call/response pair carrying a 200-line result,
/// directly into session history (bypassing the dummy API, same idiom as
/// `compaction_lifecycle.rs`), so turns two and three can walk it out of the
/// freshness window.
async fn seed_old_tool_result(pipeline: &TestPipeline) -> Result<()> {
    seed_tool_result(pipeline, "turn one", "big-result").await
}

/// Same idiom as `seed_old_tool_result`, generalized so a caller can seed
/// several turns' worth of tool exchanges directly into history without
/// going through the dummy API for each one.
async fn seed_tool_result(
    pipeline: &TestPipeline,
    user_text: &str,
    tool_call_id: &str,
) -> Result<()> {
    let request = Message::assistant().with_tool_request(
        tool_call_id,
        Ok(CallToolRequestParams::new(ADD).with_arguments(serde_json::Map::new())),
    );
    let response = Message::user().with_tool_response(
        tool_call_id,
        Ok(CallToolResult::success(vec![ContentBlock::text(
            numbered_lines(200),
        )])),
    );
    pipeline
        .seed([Message::user().with_text(user_text), request, response])
        .await
}

#[tokio::test]
async fn old_tool_result_is_condensed_by_the_third_turn() -> Result<()> {
    // Pin KAJI_CONDENSE to its default (unset = enabled) for the duration of
    // this test. Without this, `kill_switch_keeps_old_tool_result_raw` below
    // can run concurrently on another test thread and flip the process-wide
    // env var to "0" mid-flight, since env vars are process state shared
    // across all tests in this binary.
    let _guard = env_lock::lock_env([("KAJI_CONDENSE", None::<&str>)]);
    let (pipeline, api) = test_pipeline().await?;
    seed_old_tool_result(&pipeline).await?;

    api.on("turn two").reply("ok two");
    api.on("turn three").reply("ok three");
    pipeline.run(["turn two", "turn three"]).await?;

    let last = api.last_call();
    assert!(
        last.input_contains("lignes omises"),
        "old tool-result from turn one should be condensed by turn three's inference"
    );
    assert!(
        !last.input_contains("line_100"),
        "middle of the old tool-result should have been omitted"
    );
    assert!(
        last.input_contains("line_1"),
        "head of the old tool-result should be kept"
    );

    Ok(())
}

#[tokio::test]
async fn kill_switch_keeps_old_tool_result_raw() -> Result<()> {
    let _guard = env_lock::lock_env([("KAJI_CONDENSE", Some("0"))]);
    let (pipeline, api) = test_pipeline().await?;
    seed_old_tool_result(&pipeline).await?;

    api.on("turn two").reply("ok two");
    api.on("turn three").reply("ok three");
    pipeline.run(["turn two", "turn three"]).await?;

    let last = api.last_call();
    assert!(
        last.input_contains("line_100"),
        "KAJI_CONDENSE=0 should keep the old tool-result raw"
    );

    Ok(())
}

#[tokio::test]
async fn previous_turn_tool_result_stays_raw_despite_real_turn_context_events() -> Result<()> {
    // Regression for the turn-context freshness-window bug: seeding two
    // turns' worth of tool results directly (turn one and turn two), then
    // running turn three for real, produces exactly the shape that broke
    // `is_turn_boundary` — a real MOIM turn-context event (moim.rs) trails
    // the current turn's user message. With keep_raw_turns=2, turn two and
    // turn three are the fresh window: turn one's result is old and must be
    // condensed, but turn two's must stay raw. Before the fix, that trailing
    // turn-context event counted as an extra boundary and pulled the cutoff
    // past turn two, condensing it too.
    let _guard = env_lock::lock_env([("KAJI_CONDENSE", None::<&str>)]);
    let (pipeline, api) = test_pipeline().await?;
    seed_tool_result(&pipeline, "turn one", "big-result-1").await?;
    seed_tool_result(&pipeline, "turn two", "big-result-2").await?;

    api.on("turn three").reply("ok three");
    pipeline.run(["turn three"]).await?;

    let last = api.last_call();
    assert!(
        last.input_contains("lignes omises"),
        "turn one's tool-result is outside the freshness window and should be condensed"
    );
    assert_eq!(
        last.input_occurrences("line_100"),
        1,
        "turn two's tool-result is inside the freshness window and must stay raw"
    );

    Ok(())
}
