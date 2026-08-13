use anyhow::Result;
use std::path::PathBuf;

use crate::cli::Identifier;
use crate::session::{build_session, SessionBuilderConfig};
use kaji::config::Config;

pub async fn handle_tui(
    spec: Option<PathBuf>,
    identifier: Option<Identifier>,
    resume: bool,
) -> Result<()> {
    let spec = crate::tui::resolve_spec(spec)?;
    let kaji_mode = Config::global().get_kaji_mode().unwrap_or_default();
    let session_id =
        crate::cli::get_or_create_session_id(identifier, resume, false, kaji_mode).await?;
    let session = build_session(SessionBuilderConfig {
        resume,
        session_id,
        interactive: true,
        ..Default::default()
    })
    .await;
    let (mut agent, session_id, conversation) = session.into_parts();
    // Only the TUI exposes `/checkpoints` and `/restore`, so this is the
    // sole production caller that needs a `CheckpointStore` wired in — see
    // `Agent::wire_checkpoint_store`'s doc comment for why other
    // `build_session` callers (kaji run, doctor, review, ...) skip it.
    agent.wire_checkpoint_store(&session_id).await;
    crate::tui::run(agent, session_id, conversation, spec, resume).await
}
