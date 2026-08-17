//! The KAJI memory recall query must be read off the repaired conversation, as
//! the legacy loop does in `Agent::prepare_reply_context`: `fix_conversation`
//! merges consecutive user messages, so the query spans both of them instead of
//! only the last raw one.

use anyhow::Result;

use super::pipeline::test_pipeline;
use crate::conversation::message::Message;
use crate::kaji::SessionMemory;

#[tokio::test]
async fn recall_query_spans_merged_user_messages_like_the_legacy_loop() -> Result<()> {
    const FACT: &str = "quokka-handoff staging cluster eu-west-3";

    let (pipeline, api) = test_pipeline().await?;
    SessionMemory::load("memory-parity-fixture").remember(FACT, &["quokka"], None);

    pipeline
        .seed([
            Message::user().with_text("quokka"),
            Message::user().with_text("narwhal"),
        ])
        .await?;
    api.on("narwhal").reply("acknowledged");

    pipeline.resume().await?;

    assert!(
        api.calls()[0].system_contains(FACT),
        "recall must use the merged user text, not the last raw message"
    );

    Ok(())
}
