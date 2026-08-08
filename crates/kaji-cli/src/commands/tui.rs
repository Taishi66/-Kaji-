use anyhow::Result;
use std::path::PathBuf;

use crate::session::{build_session, SessionBuilderConfig};
use kaji::config::Config;

pub async fn handle_tui(spec: Option<PathBuf>) -> Result<()> {
    let kaji_mode = Config::global().get_kaji_mode().unwrap_or_default();
    let session_id = crate::cli::get_or_create_session_id(None, false, false, kaji_mode).await?;
    let session = build_session(SessionBuilderConfig {
        session_id,
        interactive: true,
        ..Default::default()
    })
    .await;
    let (agent, session_id, conversation) = session.into_parts();
    crate::tui::run(agent, session_id, conversation, spec).await
}
