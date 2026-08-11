//! Everything specific to skills: filesystem discovery (`SKILL.md` walking +
//! built-ins) and the runtime MCP client (`client` submodule). User-facing
//! CRUD lives in `crate::sources`, which generalizes across source types.

mod arguments;
mod builtin;
pub mod client;

pub use client::{SkillsClient, EXTENSION_NAME};

use crate::config::{paths::Paths, Config};
use crate::plugins::installed_plugin_skill_dirs;
use crate::sources::parse_frontmatter;
use agent_client_protocol::Error;
use anyhow::Result;
use arguments::apply_skill_arguments;
use kaji_sdk_types::custom_requests::{SourceEntry, SourceType};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use tracing::warn;

#[derive(Debug, Deserialize)]
pub struct SkillFrontmatter {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: String,
    /// Free-form bag for caller-defined fields. Per the agentskills.io spec
    /// (<https://agentskills.io/specification#frontmatter>), arbitrary
    /// metadata lives in this nested mapping so it doesn't collide with
    /// reserved frontmatter fields.
    #[serde(default)]
    pub metadata: HashMap<String, Value>,
}

/// Canonical writable location for global user skills: `~/.agents/skills`.
pub fn global_skills_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".agents").join("skills"))
}

/// Canonical writable location for project-scoped skills:
/// `<project>/.agents/skills`.
pub fn project_skills_dir(project_dir: &Path) -> PathBuf {
    project_dir.join(".agents").join("skills")
}

pub(crate) fn skills_dir_global_or_err() -> Result<PathBuf, Error> {
    global_skills_dir()
        .ok_or_else(|| Error::internal_error().data("Could not determine home directory"))
}

pub(crate) fn skills_dir_project_or_err(project_dir: &str) -> Result<PathBuf, Error> {
    if project_dir.trim().is_empty() {
        return Err(
            Error::invalid_params().data("projectDir must not be empty when global is false")
        );
    }
    Ok(project_skills_dir(Path::new(project_dir)))
}

pub(crate) fn skill_base_dir(global: bool, project_dir: Option<&str>) -> Result<PathBuf, Error> {
    if global {
        skills_dir_global_or_err()
    } else {
        let pd = project_dir.ok_or_else(|| {
            Error::invalid_params().data("projectDir is required when global is false")
        })?;
        skills_dir_project_or_err(pd)
    }
}

pub(crate) fn validate_skill_name(name: &str) -> Result<(), Error> {
    if name.is_empty() {
        return Err(Error::invalid_params().data("Skill name must not be empty"));
    }
    if name.len() > 64 {
        return Err(Error::invalid_params().data(format!(
            "Invalid skill name \"{}\". Names must be at most 64 characters.",
            name
        )));
    }
    if !name
        .chars()
        .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
    {
        return Err(Error::invalid_params().data(format!(
            "Invalid skill name \"{}\". Names may only contain lowercase letters, digits, and hyphens.",
            name
        )));
    }
    if name.starts_with('-') || name.ends_with('-') {
        return Err(Error::invalid_params().data(format!(
            "Invalid skill name \"{}\". Names must not start or end with a hyphen.",
            name
        )));
    }
    Ok(())
}

const DEFAULT_KAJI_DOCS_ROOT: &str = "https://goose-docs.ai";
const KAJI_DOCS_ROOT_PLACEHOLDER: &str = "{{KAJI_DOCS_ROOT}}";

/// Substitute the `{{KAJI_DOCS_ROOT}}` placeholder in the builtin
/// `kaji-doc-guide` skill with the resolved docs root. Resolution is
/// deterministic: the configured `KAJI_DOCS_ROOT` if set, otherwise the
/// canonical online docs root.
fn resolve_docs_root_placeholder(skill: &SourceEntry, content: &str, docs_root: &str) -> String {
    if skill.name != "kaji-doc-guide" || skill.source_type != SourceType::BuiltinSkill {
        return content.to_string();
    }

    content.replace(KAJI_DOCS_ROOT_PLACEHOLDER, docs_root)
}

/// Default cap on estimated content tokens rendered by [`loaded_skill_context`]
/// before the content is truncated. Zero or invalid values for
/// `KAJI_SKILL_MAX_TOKENS` fall back to this default (same shape as
/// `context_mgmt::condense::max_lines`).
const DEFAULT_SKILL_MAX_TOKENS: usize = 24_000;

pub fn skill_max_tokens() -> usize {
    match Config::global().get_param::<usize>("KAJI_SKILL_MAX_TOKENS") {
        Ok(v) if v > 0 => v,
        _ => DEFAULT_SKILL_MAX_TOKENS,
    }
}

/// Truncate `content` at `limit` estimated tokens, cutting chars
/// proportionally to the token estimate so the operation stays simple and
/// deterministic (no incremental re-tokenization). Appends a marker pointing
/// back at `load_skill` for the files listed in the header so the model can
/// fetch precise sections instead of guessing.
fn truncate_content_for_budget(skill_name: &str, content: &str, limit: usize) -> String {
    let estimated_tokens = crate::token_counter::count_tokens_sync(content);
    if estimated_tokens <= limit {
        return content.to_string();
    }

    let keep_ratio = limit as f64 / estimated_tokens as f64;
    let char_count = content.chars().count();
    let keep_chars = ((char_count as f64) * keep_ratio).floor() as usize;
    let truncated: String = content.chars().take(keep_chars).collect();

    format!(
        "{truncated}\n\n[… contenu tronqué : skill de ~{estimated_tokens} tokens, limite {limit}. \
         Fichiers précis chargeables via load_skill(name: \"{skill_name}/<chemin>\") — voir la \
         liste ci-dessus. Limite ajustable via KAJI_SKILL_MAX_TOKENS.]"
    )
}

fn loaded_skill_context(skill: &SourceEntry, content: &str) -> Result<String> {
    let docs_root = Config::global()
        .get_kaji_docs_root()?
        .unwrap_or_else(|| DEFAULT_KAJI_DOCS_ROOT.to_string());
    let content = resolve_docs_root_placeholder(skill, content, &docs_root);

    let title = format!("{} ({})", skill.name, skill.source_type);
    let mut output = format!("# Loaded Skill: {title}\n\n{}\n", skill.description);

    if !skill.supporting_files.is_empty() {
        let skill_dir = Path::new(&skill.path);
        output.push_str(&format!(
            "\n## Supporting Files\n\nSkill directory: {}\n\n\
             Relative paths in this skill resolve from the skill directory. \
             The shell tool runs in the session working directory, so use the \
             resolved path below or `cd` into the skill directory before running \
             supporting scripts.\n\n",
            skill.path
        ));
        for file in &skill.supporting_files {
            if let Ok(relative) = Path::new(file).strip_prefix(skill_dir) {
                let rel_str = relative.to_string_lossy().replace('\\', "/");
                let resolved_path = Path::new(file).to_string_lossy().replace('\\', "/");
                output.push_str(&format!(
                    "- {} → {} (load_skill(name: \"{}/{}\"))\n",
                    rel_str, resolved_path, skill.name, rel_str
                ));
            }
        }
    }

    let content = truncate_content_for_budget(&skill.name, &content, skill_max_tokens());
    output.push_str(&format!("\n## Content\n\n{}\n", content));

    Ok(output)
}

pub fn loaded_skill_context_with_args(skill: &SourceEntry, args: Option<&str>) -> Result<String> {
    let content = if let Some(args) = args {
        apply_skill_arguments(&skill.content, args, &skill_argument_names(skill))?
    } else {
        skill.content.clone()
    };

    loaded_skill_context(skill, &content)
}

pub fn skill_argument_hint(skill: &SourceEntry) -> Option<String> {
    skill
        .properties
        .get("argument-hint")
        .and_then(|value| value.as_str())
        .filter(|hint| !hint.is_empty())
        .map(str::to_string)
}

pub fn skill_argument_names(skill: &SourceEntry) -> Vec<String> {
    skill
        .properties
        .get("arguments")
        .and_then(|value| value.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str())
                .filter(|name| !name.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn canonicalize_or_original(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn inferred_discoverable_skill_root(path: &Path) -> Option<PathBuf> {
    let canonical_path = canonicalize_or_original(path);

    let mut global_roots = Vec::new();
    if let Some(global_root) = global_skills_dir() {
        global_roots.push(global_root);
    }
    global_roots.push(Paths::config_dir().join("skills"));
    if let Some(home) = dirs::home_dir() {
        global_roots.push(home.join(".claude").join("skills"));
        global_roots.push(home.join(".config").join("agents").join("skills"));
    }
    global_roots.extend(installed_plugin_skill_dirs());

    for root in global_roots {
        let canonical_root = canonicalize_or_original(&root);
        if canonical_path.starts_with(&canonical_root) {
            return Some(canonical_root);
        }
    }

    canonical_path.ancestors().find_map(|ancestor| {
        let parent = ancestor.parent()?;
        let is_project_skills_root = ancestor.file_name().and_then(|name| name.to_str())
            == Some("skills")
            && matches!(
                parent.file_name().and_then(|name| name.to_str()),
                Some(".kaji") | Some(".claude") | Some(".agents")
            );
        is_project_skills_root.then(|| ancestor.to_path_buf())
    })
}

pub(crate) fn resolve_discoverable_skill_dir(path: &str) -> Result<PathBuf, Error> {
    if path.is_empty() {
        return Err(Error::invalid_params().data("Source path must not be empty"));
    }

    let canonical_dir = Path::new(path)
        .canonicalize()
        .map_err(|_| Error::invalid_params().data(format!("Source \"{}\" not found", path)))?;

    if inferred_discoverable_skill_root(&canonical_dir).is_none()
        || !canonical_dir.is_dir()
        || !canonical_dir.join("SKILL.md").is_file()
    {
        return Err(Error::invalid_params().data(format!("Source \"{}\" not found", path)));
    }

    Ok(canonical_dir)
}

pub(crate) fn resolve_skill_dir(path: &str) -> Result<PathBuf, Error> {
    resolve_discoverable_skill_dir(path)
}

pub(crate) fn is_global_skill_dir(path: &Path) -> bool {
    global_skills_dir().as_deref().is_some_and(|root| {
        canonicalize_or_original(path).starts_with(canonicalize_or_original(root))
    })
}

pub(crate) fn infer_skill_name(dir: &Path) -> String {
    let md = dir.join("SKILL.md");
    if let Ok(raw) = std::fs::read_to_string(&md) {
        if let Ok(Some((meta, _))) = parse_frontmatter::<SkillFrontmatter>(&raw) {
            if let Some(n) = meta.name.filter(|n| !n.is_empty()) {
                return n;
            }
        }
    }
    dir.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unnamed")
        .to_string()
}

pub(crate) fn build_skill_md(
    name: &str,
    description: &str,
    content: &str,
    metadata: &HashMap<String, Value>,
) -> String {
    let safe_desc = description.replace('\'', "''");
    let mut md = String::from("---\n");
    md.push_str(&format!("name: {}\n", name));
    md.push_str(&format!("description: '{}'\n", safe_desc));
    if !metadata.is_empty() {
        md.push_str("metadata:\n");
        // Use YAML for the nested metadata block. We render it with serde_yaml
        // and indent every line by two spaces so it nests under `metadata:`.
        let yaml = serde_yaml::to_string(metadata).unwrap_or_default();
        for line in yaml.lines() {
            if line.is_empty() {
                continue;
            }
            md.push_str("  ");
            md.push_str(line);
            md.push('\n');
        }
    }
    md.push_str("---\n");
    if !content.is_empty() {
        md.push('\n');
        md.push_str(content);
        md.push('\n');
    }
    md
}

pub(crate) fn parse_skill_frontmatter(raw: &str) -> (String, String) {
    if !raw.trim_start().starts_with("---") {
        return (String::new(), raw.to_string());
    }
    match parse_frontmatter::<SkillFrontmatter>(raw) {
        Ok(Some((meta, body))) => (meta.description, body),
        _ => (String::new(), raw.to_string()),
    }
}

/// Every directory the agent reads skills from, paired with whether each is a
/// global (home-rooted) location. Order matches discovery precedence: project
/// dirs first, then global dirs.
pub fn all_skill_dirs(working_dir: Option<&Path>) -> Vec<(PathBuf, bool)> {
    let mut dirs: Vec<(PathBuf, bool)> = Vec::new();

    if let Some(wd) = working_dir {
        dirs.push((wd.join(".agents").join("skills"), false));
        dirs.push((wd.join(".kaji").join("skills"), false));
        dirs.push((wd.join(".claude").join("skills"), false));
    }

    let home = dirs::home_dir();
    if let Some(h) = home.as_ref() {
        dirs.push((h.join(".agents").join("skills"), true));
    }
    dirs.push((Paths::config_dir().join("skills"), true));
    if let Some(h) = home.as_ref() {
        dirs.push((h.join(".claude").join("skills"), true));
        dirs.push((h.join(".config").join("agents").join("skills"), true));
    }

    dirs.extend(
        installed_plugin_skill_dirs()
            .into_iter()
            .map(|dir| (dir, true)),
    );

    dirs
}

fn parse_skill_content(content: &str, path: &Path, global: bool) -> Option<SourceEntry> {
    let (metadata, body): (SkillFrontmatter, String) = match parse_frontmatter(content) {
        Ok(Some(parsed)) => parsed,
        Ok(None) => return None,
        Err(e) => {
            warn!("Failed to parse skill frontmatter: {}", e);
            return None;
        }
    };

    let name = match metadata.name.filter(|n| !n.is_empty()) {
        Some(n) => n,
        None => {
            warn!(
                "Skill at '{}' is missing a required 'name' in frontmatter, skipping",
                path.display()
            );
            return None;
        }
    };

    if name.contains('/') {
        warn!("Skill name '{}' contains '/', skipping", name);
        return None;
    }

    Some(SourceEntry {
        source_type: SourceType::Skill,
        name,
        description: metadata.description,
        content: body,
        path: path.to_string_lossy().into_owned(),
        global,
        writable: true,
        supporting_files: Vec::new(),
        properties: metadata.metadata,
    })
}

fn should_skip_dir(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(|name| name.to_str()),
        Some(".git") | Some(".hg") | Some(".svn")
    )
}

fn walk_files_recursively<F, G>(
    dir: &Path,
    visited_dirs: &mut HashSet<PathBuf>,
    should_descend: &mut G,
    visit_file: &mut F,
) where
    F: FnMut(&Path),
    G: FnMut(&Path) -> bool,
{
    let canonical_dir = match std::fs::canonicalize(dir) {
        Ok(path) => path,
        Err(_) => return,
    };

    if !visited_dirs.insert(canonical_dir) {
        return;
    }

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if should_descend(&path) {
                walk_files_recursively(&path, visited_dirs, should_descend, visit_file);
            }
        } else if path.is_file() {
            visit_file(&path);
        }
    }
}

fn scan_skills_from_dir(dir: &Path, global: bool, seen: &mut HashSet<String>) -> Vec<SourceEntry> {
    let mut skill_files = Vec::new();
    let mut visited_dirs = HashSet::new();

    walk_files_recursively(
        dir,
        &mut visited_dirs,
        &mut |path| !should_skip_dir(path),
        &mut |path| {
            if path.file_name().and_then(|name| name.to_str()) == Some("SKILL.md") {
                skill_files.push(path.to_path_buf());
            }
        },
    );

    let mut sources = Vec::new();
    for skill_file in skill_files {
        let Some(skill_dir) = skill_file.parent() else {
            continue;
        };
        let content = match std::fs::read_to_string(&skill_file) {
            Ok(c) => c,
            Err(e) => {
                warn!("Failed to read skill file {}: {}", skill_file.display(), e);
                continue;
            }
        };

        if let Some(mut source) = parse_skill_content(&content, skill_dir, global) {
            if !seen.contains(&source.name) {
                let mut files = Vec::new();
                let mut visited_support_dirs = HashSet::new();
                walk_files_recursively(
                    skill_dir,
                    &mut visited_support_dirs,
                    &mut |path| !should_skip_dir(path) && !path.join("SKILL.md").is_file(),
                    &mut |path| {
                        if path.file_name().and_then(|n| n.to_str()) != Some("SKILL.md") {
                            files.push(path.to_string_lossy().into_owned());
                        }
                    },
                );
                source.supporting_files = files;

                seen.insert(source.name.clone());
                sources.push(source);
            }
        }
    }
    sources
}

/// Discover skills from all configured filesystem locations and built-ins.
/// Each returned entry has `global` set according to the directory it was
/// found in (or `true` for built-ins).
pub fn discover_skills(working_dir: Option<&Path>) -> Vec<SourceEntry> {
    let mut sources: Vec<SourceEntry> = Vec::new();
    let mut seen = HashSet::new();

    for (dir, is_global) in all_skill_dirs(working_dir) {
        for source in scan_skills_from_dir(&dir, is_global, &mut seen) {
            sources.push(source);
        }
    }

    for content in builtin::get_all() {
        if let Some(source) = parse_skill_content(content, &PathBuf::new(), true) {
            if !seen.contains(&source.name) {
                seen.insert(source.name.clone());
                let path = format!("builtin://skills/{}", source.name);
                sources.push(SourceEntry {
                    source_type: SourceType::BuiltinSkill,
                    path,
                    ..source
                });
            }
        }
    }

    sources
}

pub fn list_installed_skills(working_dir: Option<&Path>) -> Vec<SourceEntry> {
    let fallback;
    let wd = match working_dir {
        Some(p) => Some(p),
        None => {
            fallback = std::env::current_dir().ok();
            fallback.as_deref()
        }
    };
    discover_skills(wd)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn skill_with_content(content: &str) -> SourceEntry {
        SourceEntry {
            source_type: SourceType::Skill,
            name: "test-skill".to_string(),
            description: "Test skill".to_string(),
            content: content.to_string(),
            path: String::new(),
            global: false,
            writable: true,
            supporting_files: Vec::new(),
            properties: HashMap::from([(
                "arguments".to_string(),
                json!(["component", "from", "to"]),
            )]),
        }
    }

    fn builtin_kaji_doc_guide_skill() -> SourceEntry {
        SourceEntry {
            source_type: SourceType::BuiltinSkill,
            name: "kaji-doc-guide".to_string(),
            description: "Test docs skill".to_string(),
            content: "Docs root: {{KAJI_DOCS_ROOT}}.".to_string(),
            path: "builtin://skills/kaji-doc-guide".to_string(),
            global: true,
            writable: true,
            supporting_files: Vec::new(),
            properties: HashMap::new(),
        }
    }

    #[test]
    fn loaded_skill_context_with_args_replaces_arguments_placeholder_with_raw_args() {
        let skill = skill_with_content("Review $ARGUMENTS carefully.");

        let rendered = loaded_skill_context_with_args(&skill, Some("src/foo.rs --strict")).unwrap();

        assert!(rendered.contains("Review src/foo.rs --strict carefully."));
    }

    #[test]
    fn loaded_skill_context_with_args_uses_context_without_args() {
        let skill = skill_with_content("Review the code carefully.");

        let rendered = loaded_skill_context_with_args(&skill, None).unwrap();

        assert!(rendered.contains("# Loaded Skill: test-skill (skill)"));
        assert!(rendered.contains("## Content\n\nReview the code carefully."));
    }

    #[test]
    fn loaded_skill_context_shows_resolved_paths_for_supporting_files() {
        let skill_dir = std::env::temp_dir().join("kaji-test-skill");
        let script_path = skill_dir.join("scripts").join("my-tool.exe");
        let mut skill = skill_with_content("Run scripts/my-tool.exe.");
        skill.path = skill_dir.to_string_lossy().into_owned();
        skill.supporting_files = vec![script_path.to_string_lossy().into_owned()];

        let rendered = loaded_skill_context_with_args(&skill, None).unwrap();
        let resolved_path = script_path.to_string_lossy().replace('\\', "/");

        assert!(rendered.contains("Relative paths in this skill resolve from the skill directory"));
        assert!(rendered.contains("scripts/my-tool.exe"));
        assert!(rendered.contains(&resolved_path));
        assert!(rendered.contains("load_skill(name: \"test-skill/scripts/my-tool.exe\")"));

        let supporting_files_pos = rendered.find("## Supporting Files").unwrap();
        let content_pos = rendered.find("## Content").unwrap();
        assert!(
            supporting_files_pos < content_pos,
            "path info (Skill directory / Supporting Files) must precede Content so the model \
             sees where to look before it starts reading a possibly huge body"
        );
    }

    #[test]
    fn loaded_skill_context_puts_title_and_description_before_content() {
        let skill = skill_with_content("Review the code carefully.");

        let rendered = loaded_skill_context_with_args(&skill, None).unwrap();
        let title_pos = rendered.find("# Loaded Skill: test-skill (skill)").unwrap();
        let description_pos = rendered.find("Test skill").unwrap();
        let content_pos = rendered.find("## Content").unwrap();

        assert!(title_pos < description_pos);
        assert!(description_pos < content_pos);
    }

    #[test]
    fn skill_max_tokens_defaults_when_unset() {
        let _guard = env_lock::lock_env([("KAJI_SKILL_MAX_TOKENS", None::<&str>)]);
        assert_eq!(skill_max_tokens(), DEFAULT_SKILL_MAX_TOKENS);
    }

    #[test]
    fn skill_max_tokens_reads_env_override() {
        let _guard = env_lock::lock_env([("KAJI_SKILL_MAX_TOKENS", Some("500"))]);
        assert_eq!(skill_max_tokens(), 500);
    }

    #[test]
    fn skill_max_tokens_falls_back_on_zero_or_invalid() {
        {
            let _guard = env_lock::lock_env([("KAJI_SKILL_MAX_TOKENS", Some("0"))]);
            assert_eq!(skill_max_tokens(), DEFAULT_SKILL_MAX_TOKENS);
        }
        {
            let _guard = env_lock::lock_env([("KAJI_SKILL_MAX_TOKENS", Some("not-a-number"))]);
            assert_eq!(skill_max_tokens(), DEFAULT_SKILL_MAX_TOKENS);
        }
    }

    #[test]
    fn loaded_skill_context_under_limit_is_unchanged() {
        let _guard = env_lock::lock_env([("KAJI_SKILL_MAX_TOKENS", None::<&str>)]);
        let skill = skill_with_content("Short instructions that stay well under budget.");

        let rendered = loaded_skill_context_with_args(&skill, None).unwrap();

        assert!(rendered.contains("## Content\n\nShort instructions that stay well under budget."));
        assert!(!rendered.contains("contenu tronqué"));
    }

    #[test]
    fn loaded_skill_context_over_limit_is_truncated_with_marker_and_header_intact() {
        let _guard = env_lock::lock_env([("KAJI_SKILL_MAX_TOKENS", Some("10"))]);
        let huge_content = "word ".repeat(5_000);
        let skill = skill_with_content(&huge_content);

        let rendered = loaded_skill_context_with_args(&skill, None).unwrap();

        assert!(rendered.contains("# Loaded Skill: test-skill (skill)"));
        assert!(rendered.contains("Test skill"));
        assert!(rendered.contains("[… contenu tronqué : skill de ~"));
        assert!(rendered.contains("tokens, limite 10."));
        assert!(rendered
            .contains("Fichiers précis chargeables via load_skill(name: \"test-skill/<chemin>\")"));
        assert!(rendered.contains("Limite ajustable via KAJI_SKILL_MAX_TOKENS."));
        assert!(rendered.len() < huge_content.len());
    }

    #[test]
    fn resolve_docs_root_placeholder_substitutes_builtin_kaji_doc_guide_root() {
        let skill = builtin_kaji_doc_guide_skill();

        let rendered = resolve_docs_root_placeholder(&skill, &skill.content, "/tmp/kaji docs/root");

        assert_eq!(rendered, "Docs root: /tmp/kaji docs/root.");
    }

    #[test]
    fn resolve_docs_root_placeholder_ignores_non_builtin_kaji_doc_guide_skills() {
        let mut skill = builtin_kaji_doc_guide_skill();
        skill.source_type = SourceType::Skill;

        let rendered = resolve_docs_root_placeholder(&skill, &skill.content, "/tmp/kaji-docs");

        assert_eq!(rendered, skill.content);
    }
}
