//! Async curator turning the raw memory journal into durable, portable facts.
//!
//! The curator is the only writer of `created_by: curator` facts. It is
//! fail-closed by construction: any parse, model, or index error aborts the run
//! before `mark_curated`, so the batch is replayed on the next trigger instead
//! of being silently dropped. A run writes at most [`CURATOR_CAP`] facts,
//! redacts everything landing in the project scope (facts travel with the repo),
//! and never rewrites a fact the user authored.

use std::path::Path;

use anyhow::Result;
use kaji_core::facts::{validate_slug, CreatedBy, Fact, FactIndex, FactStore, FactType};
use kaji_providers::model::ModelConfig;
use serde::Deserialize;

use crate::config::Config;
use crate::conversation::message::Message;
use crate::kaji::{fact_index_path, project_facts_dir, user_facts_dir, SessionMemory};
use crate::model_config::{complete_with_model, get_fast_model, model_config_from_user_config};
use crate::providers::base::Provider;
use crate::session::redact_text;

/// Facts a single curation run may write. Bounds the blast radius of a
/// misbehaving (or prompt-injected) model.
pub const CURATOR_CAP: usize = 5;

/// Journal entries handed to the curator per run.
const UNCURATED_BATCH: usize = 50;

const SYSTEM_PROMPT: &str = r#"You are the memory curator of a coding agent. From the raw journal entries below, extract at most 5 durable facts worth remembering across sessions: decisions (choices with lasting consequences), gotchas (non-obvious traps), preferences (how the user wants things done), references (pointers to resources). Skip small talk, one-off task chatter, and anything already covered by an existing fact unless it needs updating.

Existing facts (type-slug: description):
{existing_facts_index}

Reply with a JSON array only, no prose:
[{"action":"create|update","type":"decision|gotcha|preference|reference","slug":"kebab-case","description":"one line","body":"the fact, self-contained"}]
Reply [] if nothing is worth keeping."#;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CurationOutcome {
    pub created: usize,
    pub updated: usize,
    /// Facts the curator meant to write but couldn't. Non-zero forbids stamping
    /// the batch as curated: the entries must survive for the next run.
    pub failed: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CuratorOp {
    pub action: String,
    pub r#type: String,
    pub slug: String,
    pub description: String,
    pub body: String,
}

/// Parse the model's reply into curator ops. Strict: a stray word of prose, an
/// unknown action, or an unknown fact type fails the whole run.
pub fn parse_curator_ops(response: &str) -> Result<Vec<CuratorOp>> {
    let ops: Vec<CuratorOp> = serde_json::from_str(strip_code_fence(response.trim()))?;
    for op in &ops {
        if op.action != "create" && op.action != "update" {
            anyhow::bail!("unknown curator action: {}", op.action);
        }
        if FactType::parse(&op.r#type).is_none() {
            anyhow::bail!("unknown fact type: {}", op.r#type);
        }
    }
    Ok(ops)
}

fn strip_code_fence(text: &str) -> &str {
    let Some(rest) = text.strip_prefix("```") else {
        return text;
    };
    let Some((_, body)) = rest.split_once('\n') else {
        return text;
    };
    let body = body.trim_end();
    body.strip_suffix("```").unwrap_or(body).trim()
}

/// Write the ops, capped and guarded. A guard never aborts the run: it logs,
/// counts the op into [`CurationOutcome::failed`], and moves on — so one bad op
/// costs the batch a replay rather than voiding the facts that did land.
///
/// Ops beyond the cap are counted as failed too. They are not lost: the batch
/// stays uncurated and replays, and the run converges because each pass feeds
/// the model a longer existing-facts index.
///
/// Descriptions are flattened to a single line: `FactStore` renders them into
/// the generated `MEMORY.md`, where a newline would inject extra index rows.
pub fn apply_ops(
    ops: Vec<CuratorOp>,
    project: &FactStore,
    user: &FactStore,
    session_id: &str,
    today: &str,
) -> CurationOutcome {
    let mut outcome = CurationOutcome {
        failed: ops.len().saturating_sub(CURATOR_CAP),
        ..CurationOutcome::default()
    };

    for op in ops.into_iter().take(CURATOR_CAP) {
        let Some(fact_type) = FactType::parse(&op.r#type) else {
            tracing::warn!(fact_type = %op.r#type, "curator op with unknown fact type skipped");
            continue;
        };
        if !validate_slug(&op.slug) {
            tracing::warn!(slug = %op.slug, "curator op with invalid slug skipped");
            continue;
        }
        if redact_text(&op.slug).0 != op.slug {
            tracing::warn!("curator op with a secret-shaped slug skipped");
            outcome.failed += 1;
            continue;
        }

        let project_scope = fact_type != FactType::Preference;
        let store = if project_scope { project } else { user };

        let existing = store.get(&fact_type, &op.slug);
        if existing
            .as_ref()
            .is_some_and(|fact| fact.created_by == CreatedBy::User)
        {
            tracing::warn!(slug = %op.slug, "curator op on a user-authored fact skipped");
            continue;
        }

        let (description, body) = if project_scope {
            (redact_text(&op.description).0, redact_text(&op.body).0)
        } else {
            (op.description, op.body)
        };
        let description = description.replace(['\n', '\r'], " ");

        let updating = op.action == "update" && existing.is_some();
        let fact = match existing {
            Some(previous) if updating => Fact {
                fact_type,
                slug: op.slug,
                description,
                date: previous.date,
                session: session_id.to_string(),
                created_by: previous.created_by,
                body,
            },
            _ => Fact {
                fact_type,
                slug: op.slug,
                description,
                date: today.to_string(),
                session: session_id.to_string(),
                created_by: CreatedBy::Curator,
                body,
            },
        };

        if let Err(err) = store.write(&fact) {
            tracing::warn!(slug = %fact.slug, error = %err, "curator fact write failed");
            outcome.failed += 1;
            continue;
        }
        if updating {
            outcome.updated += 1;
        } else {
            outcome.created += 1;
        }
    }

    outcome
}

/// Resolve the model a curation run uses. `KAJI_MEMORY_CURATOR_MODEL` wins
/// outright — over `KAJI_FAST_MODEL` too — so the curator can be pinned to a
/// model that honours the JSON reply contract. Without it, the fast model.
pub async fn curator_model(provider_name: &str, model_config: &ModelConfig) -> Result<ModelConfig> {
    match configured_curator_model_name() {
        Some(name) if name != model_config.model_name => {
            Ok(model_config_from_user_config(provider_name, name)?
                .with_request_headers(model_config.request_headers.clone()))
        }
        Some(_) => Ok(model_config.clone()),
        None => get_fast_model(provider_name, model_config).await,
    }
}

/// Curate this session's uncurated journal entries into facts.
///
/// Entries are only stamped as curated once every op landed on disk and the
/// recall index is refreshed; any failure — propagated or counted in
/// [`CurationOutcome::failed`] — leaves the batch pending for the next run.
pub async fn curate(
    provider: &dyn Provider,
    provider_name: &str,
    model_config: &ModelConfig,
    session_id: &str,
    working_dir: &Path,
) -> Result<CurationOutcome> {
    let mut memory = SessionMemory::load(session_id);
    let entries = memory.uncurated(UNCURATED_BATCH);
    if entries.is_empty() {
        return Ok(CurationOutcome::default());
    }

    let curator = curator_model(provider_name, model_config).await?;

    let project = FactStore::new(project_facts_dir(working_dir));
    let user = FactStore::new(user_facts_dir());
    let system = SYSTEM_PROMPT.replace(
        "{existing_facts_index}",
        &existing_facts_index(&project, &user),
    );

    let journal = entries
        .iter()
        .enumerate()
        .map(|(i, entry)| format!("{}. {}", i + 1, entry.text))
        .collect::<Vec<_>>()
        .join("\n");

    let (response, _) = complete_with_model(
        provider,
        &curator,
        model_config,
        session_id,
        &system,
        &[Message::user().with_text(journal)],
        &[],
    )
    .await?;

    let reply: String = response
        .content
        .iter()
        .filter_map(|c| c.as_text())
        .collect();
    let ops = parse_curator_ops(&reply)?;

    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let outcome = apply_ops(ops, &project, &user, session_id, &today);

    let mut index = FactIndex::open(&fact_index_path(working_dir))?;
    index.rebuild_if_stale(&[("project", &project), ("user", &user)])?;

    if outcome.failed > 0 {
        tracing::warn!(
            failed = outcome.failed,
            "curator fact writes failed; journal batch left uncurated for retry"
        );
        return Ok(outcome);
    }

    let ids: Vec<u64> = entries.iter().map(|entry| entry.id).collect();
    memory.mark_curated(&ids);

    Ok(outcome)
}

fn existing_facts_index(project: &FactStore, user: &FactStore) -> String {
    let lines: Vec<String> = project
        .list()
        .into_iter()
        .chain(user.list())
        .map(|fact| {
            format!(
                "{}-{}: {}",
                fact.fact_type.as_str(),
                fact.slug,
                fact.description
            )
        })
        .collect();
    if lines.is_empty() {
        "(none)".to_string()
    } else {
        lines.join("\n")
    }
}

fn configured_curator_model_name() -> Option<String> {
    Config::global()
        .get_param::<String>("KAJI_MEMORY_CURATOR_MODEL")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}
