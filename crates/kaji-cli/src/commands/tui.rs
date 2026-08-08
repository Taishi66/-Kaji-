use anyhow::Result;
use std::path::PathBuf;

pub async fn handle_tui(spec: Option<PathBuf>) -> Result<()> {
    crate::tui::run(spec).await
}
