use super::diagnostics::{generate_diagnostics, DiagnosticsLevel};
use super::redact::redact_text;
use crate::session::SessionManager;
use anyhow::{Context, Result};
use std::fs;
use std::path::PathBuf;

/// Result of packaging a redacted bug-report bundle.
pub struct RageBundle {
    /// Absolute or relative path of the written bundle.
    pub path: PathBuf,
    /// Number of secret values masked.
    pub redaction_count: usize,
    /// Session covered by the bundle, when one was found.
    pub session_id: Option<String>,
    /// Diagnostics level used (Summary when no session exists).
    pub level: DiagnosticsLevel,
}

/// Pick the most recently updated session.
async fn latest_session_id(session_manager: &SessionManager) -> Result<Option<String>> {
    let mut sessions = session_manager.list_sessions().await?;
    sessions.sort_by_key(|session| std::cmp::Reverse(session.updated_at));
    Ok(sessions.into_iter().next().map(|session| session.id))
}

/// Build a redacted diagnostics bundle for bug reports. Uses the given session
/// (or the most recent one) for the full report; falls back to a summary-only
/// report when no session exists.
pub async fn generate_rage_bundle(
    session_manager: &SessionManager,
    session_id: Option<String>,
    output_path: Option<PathBuf>,
) -> Result<RageBundle> {
    let session_id = match session_id {
        Some(id) => Some(id),
        None => latest_session_id(session_manager).await?,
    };

    let (report, level) = match &session_id {
        Some(id) => (
            generate_diagnostics(session_manager, id, DiagnosticsLevel::Full).await?,
            DiagnosticsLevel::Full,
        ),
        // Summary never touches the session, so an empty id is safe when there
        // is no session to export.
        None => (
            generate_diagnostics(session_manager, "", DiagnosticsLevel::Summary).await?,
            DiagnosticsLevel::Summary,
        ),
    };

    let serialized =
        serde_json::to_string_pretty(&report).context("serialize diagnostics report")?;
    let (redacted, redaction_count) = redact_text(&serialized);

    let path = output_path.unwrap_or_else(|| {
        let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S");
        PathBuf::from(format!("kaji-rage-{timestamp}.json"))
    });
    fs::write(&path, redacted)
        .with_context(|| format!("failed to write rage bundle to {}", path.display()))?;

    Ok(RageBundle {
        path,
        redaction_count,
        session_id,
        level,
    })
}
