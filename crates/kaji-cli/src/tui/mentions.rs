//! @-mentions in the composer (item 4 ante): typing `@<path>` completes files
//! and directories, and mentioned paths are embedded into the submitted
//! message — a file's contents, a directory's listing — so the agent sees the
//! exact context without searching.
//!
//! Two halves:
//! - completion: [`MentionIndex`] walks the project once (respecting
//!   `.gitignore` via the `ignore` crate), cached for a TTL, plus direct
//!   directory listings for `~/`, absolute, `./`, `../` fragments;
//! - expansion: [`expand_mentions`] rewrites the submitted text, appending
//!   `<attached-file>` / `<attached-directory>` blocks. The chat line keeps
//!   the text as typed — only the model-bound message carries the payload.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const INDEX_TTL: Duration = Duration::from_secs(60);
const MAX_INDEX_ENTRIES: usize = 20_000;
const MAX_COMPLETIONS: usize = 8;
const MAX_FILE_BYTES: usize = 64 * 1024;
const MAX_TOTAL_BYTES: usize = 256 * 1024;
const MAX_DIR_ENTRIES: usize = 200;

/// Project file index backing `@` completion. Rebuilt lazily on first use and
/// at most once per `INDEX_TTL`; the walk stops at `MAX_INDEX_ENTRIES` so a
/// huge tree (vendor, target/) can't stall the composer.
pub struct MentionIndex {
    root: PathBuf,
    /// Project-relative paths, directories with a trailing `/`.
    paths: Vec<String>,
    built_at: Option<Instant>,
}

impl MentionIndex {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            paths: Vec::new(),
            built_at: None,
        }
    }

    fn ensure_fresh(&mut self) {
        let stale = self
            .built_at
            .map(|t| t.elapsed() > INDEX_TTL)
            .unwrap_or(true);
        if stale {
            self.build();
        }
    }

    fn build(&mut self) {
        let mut paths = Vec::new();
        let walker = ignore::WalkBuilder::new(&self.root)
            .hidden(true)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .sort_by_file_name(|a, b| a.cmp(b))
            .build();
        for entry in walker.flatten() {
            if entry.path() == self.root {
                continue;
            }
            let Ok(rel) = entry.path().strip_prefix(&self.root) else {
                continue;
            };
            let mut s = rel.to_string_lossy().replace('\\', "/");
            if entry.file_type().is_some_and(|t| t.is_dir()) {
                s.push('/');
            }
            paths.push(s);
            if paths.len() >= MAX_INDEX_ENTRIES {
                break;
            }
        }
        self.paths = paths;
        self.built_at = Some(Instant::now());
    }

    /// Completions for the fragment after `@`. `~/`, absolute, `./` and `../`
    /// fragments bypass the index with a direct directory listing of their
    /// parent; anything else is a case-insensitive substring match over the
    /// project index, shortest first.
    pub fn complete(&mut self, fragment: &str) -> Vec<String> {
        if let Some(listing) = complete_via_listing(fragment) {
            return listing;
        }
        self.ensure_fresh();
        let needle = fragment.to_lowercase();
        let mut scored: Vec<&String> = self
            .paths
            .iter()
            .filter(|p| p.to_lowercase().contains(&needle))
            .collect();
        scored.sort_by_key(|p| (p.len(), p.to_lowercase()));
        scored.into_iter().take(MAX_COMPLETIONS).cloned().collect()
    }
}

/// The live `@` fragment at the end of the composer input, if any. A mention
/// token starts at the beginning of the input or right after whitespace and
/// runs to the end without a space — `foo@bar` mid-word is not a mention.
pub fn active_fragment(input: &str) -> Option<&str> {
    let tail = input.rsplit_once(|c: char| c.is_whitespace());
    let token = match tail {
        Some((_, last)) => last,
        None => input,
    };
    token.strip_prefix('@').filter(|f| !f.is_empty())
}

/// Completes `~/…`, `/…`, `./…`, `../…` fragments by listing the parent
/// directory directly — these address files outside (or relative around) the
/// project root, which the index does not cover. Returns `None` for plain
/// project-relative fragments.
fn complete_via_listing(fragment: &str) -> Option<Vec<String>> {
    let expanded = expand_home(fragment);
    let path = Path::new(&expanded);
    let is_special = fragment.starts_with("~/")
        || fragment.starts_with('/')
        || fragment.starts_with("./")
        || fragment.starts_with("../");
    if !is_special {
        return None;
    }
    let (parent, prefix) = if fragment.ends_with('/') {
        (path.to_path_buf(), "")
    } else {
        match path.parent() {
            Some(p) if !p.as_os_str().is_empty() => (
                p.to_path_buf(),
                path.file_name().and_then(|n| n.to_str()).unwrap_or(""),
            ),
            _ => (path.to_path_buf(), ""),
        }
    };
    let mut out: Vec<String> = Vec::new();
    let entries = std::fs::read_dir(&parent).ok()?;
    let prefix_lc = prefix.to_lowercase();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        if !prefix_lc.is_empty() && !name.to_lowercase().starts_with(&prefix_lc) {
            continue;
        }
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let display = display_path(fragment, &name, is_dir);
        out.push(display);
        if out.len() >= MAX_COMPLETIONS {
            break;
        }
    }
    out.sort();
    Some(out)
}

/// Rebuilds the completion string in the fragment's own notation so the
/// substitution replaces the fragment cleanly (`~/x`, `./x`, absolute…).
fn display_path(fragment: &str, name: &str, is_dir: bool) -> String {
    let mut s = match fragment.rsplit_once('/') {
        Some((head, _)) => format!("{head}/{name}"),
        None => name.to_string(),
    };
    if is_dir {
        s.push('/');
    }
    s
}

fn expand_home(fragment: &str) -> String {
    expand_home_with(fragment, std::env::var_os("HOME"))
}

/// Env-free core so tests never touch the process-global `HOME` (parallel
/// suites read it without the env lock — mutating it crashes them).
fn expand_home_with(fragment: &str, home: Option<std::ffi::OsString>) -> String {
    if let Some(rest) = fragment.strip_prefix("~/") {
        if let Some(home) = home {
            return PathBuf::from(home)
                .join(rest)
                .to_string_lossy()
                .into_owned();
        }
    }
    fragment.to_string()
}

/// Rewrites the submitted text: every `@path` that resolves to an existing
/// file or directory (relative to `cwd`, with `~/` expansion) gets its
/// payload appended as an attachment block. Unresolvable mentions are left
/// as-is — the agent can still act on the raw path. Sizes are capped so a
/// stray mention can't blow up the context window.
pub fn expand_mentions(text: &str, cwd: &Path) -> String {
    let mut attachments = String::new();
    let mut total = 0usize;
    for token in text.split_whitespace() {
        let Some(raw) = token.strip_prefix('@') else {
            continue;
        };
        if raw.is_empty() {
            continue;
        }
        let expanded = expand_home(raw);
        let path = {
            let p = Path::new(&expanded);
            if p.is_absolute() {
                p.to_path_buf()
            } else {
                cwd.join(p)
            }
        };
        if total >= MAX_TOTAL_BYTES {
            break;
        }
        if path.is_file() {
            if let Some(block) =
                render_file(raw, &path, MAX_FILE_BYTES.min(MAX_TOTAL_BYTES - total))
            {
                total += block.len();
                attachments.push_str(&block);
            }
        } else if path.is_dir() {
            let block = render_dir(raw, &path);
            total += block.len();
            attachments.push_str(&block);
        }
    }
    if attachments.is_empty() {
        return text.to_string();
    }
    format!("{text}\n{attachments}")
}

fn render_file(display: &str, path: &Path, budget: usize) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.contains(&0) {
        return Some(format!(
            "\n<attached-file path=\"{display}\">(binary file — content skipped)</attached-file>\n"
        ));
    }
    let mut content = String::from_utf8_lossy(&bytes).into_owned();
    if content.len() > budget {
        content.truncate(budget);
        while !content.is_char_boundary(content.len()) {
            content.pop();
        }
        content.push_str("\n… (truncated)");
    }
    Some(format!(
        "\n<attached-file path=\"{display}\">\n{content}\n</attached-file>\n"
    ))
}

fn render_dir(display: &str, path: &Path) -> String {
    let mut listing = String::new();
    let walker = ignore::WalkBuilder::new(path)
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .max_depth(Some(3))
        .build();
    let mut count = 0;
    for entry in walker.flatten() {
        if entry.path() == path {
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(path)
            .unwrap_or(entry.path())
            .to_string_lossy()
            .replace('\\', "/");
        let suffix = if entry.file_type().is_some_and(|t| t.is_dir()) {
            "/"
        } else {
            ""
        };
        listing.push_str(&rel);
        listing.push_str(suffix);
        listing.push('\n');
        count += 1;
        if count >= MAX_DIR_ENTRIES {
            listing.push_str("… (listing truncated)\n");
            break;
        }
    }
    format!("\n<attached-directory path=\"{display}\">\n{listing}</attached-directory>\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_fragment_detects_trailing_mention() {
        assert_eq!(active_fragment("@src/mai"), Some("src/mai"));
        assert_eq!(active_fragment("regarde @src/mai"), Some("src/mai"));
        assert_eq!(active_fragment("@"), None);
        assert_eq!(active_fragment("foo@bar"), None);
        assert_eq!(active_fragment("hello world"), None);
        assert_eq!(active_fragment("@src done"), None);
    }

    #[test]
    fn expand_mentions_embeds_file_contents() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello kaji").unwrap();
        let out = expand_mentions("lis @a.txt stp", dir.path());
        assert!(out.contains("lis @a.txt stp"));
        assert!(out.contains("<attached-file path=\"a.txt\">"));
        assert!(out.contains("hello kaji"));
    }

    #[test]
    fn expand_mentions_lists_directories() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("b.rs"), "fn b() {}").unwrap();
        let out = expand_mentions("@sub/", dir.path());
        assert!(out.contains("<attached-directory path=\"sub/\">"));
        assert!(out.contains("b.rs"));
    }

    #[test]
    fn expand_mentions_leaves_unknown_paths_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let out = expand_mentions("regarde @nope/rien.txt", dir.path());
        assert_eq!(out, "regarde @nope/rien.txt");
    }

    #[test]
    fn expand_mentions_skips_binary_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("bin.dat"), [0u8, 159, 146, 150]).unwrap();
        let out = expand_mentions("@bin.dat", dir.path());
        assert!(out.contains("binary file — content skipped"));
    }

    #[test]
    fn expand_home_with_resolves_tilde_against_the_given_home() {
        let home = Some(std::ffi::OsString::from("/home/u"));
        assert_eq!(expand_home_with("~/x.txt", home.clone()), "/home/u/x.txt");
        assert_eq!(expand_home_with("plain", home.clone()), "plain");
        assert_eq!(expand_home_with("~/x.txt", None), "~/x.txt");
    }

    #[test]
    fn index_completes_project_paths() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/tui")).unwrap();
        std::fs::write(dir.path().join("src/tui/app.rs"), "x").unwrap();
        std::fs::write(dir.path().join("README.md"), "x").unwrap();
        let mut idx = MentionIndex::new(dir.path().to_path_buf());
        let matches = idx.complete("app");
        assert!(matches.iter().any(|m| m == "src/tui/app.rs"));
        let dirs = idx.complete("tu");
        assert!(dirs.iter().any(|m| m == "src/tui/"));
    }

    #[test]
    fn index_honors_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "ignored.txt\n").unwrap();
        // `ignore` only applies .gitignore inside a git repo.
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join("ignored.txt"), "x").unwrap();
        std::fs::write(dir.path().join("kept.txt"), "x").unwrap();
        let mut idx = MentionIndex::new(dir.path().to_path_buf());
        let matches = idx.complete("txt");
        assert!(matches.iter().any(|m| m == "kept.txt"));
        assert!(!matches.iter().any(|m| m == "ignored.txt"));
    }

    #[test]
    fn listing_completes_dot_slash_fragments() {
        let cwd = std::env::current_dir().unwrap();
        let out = complete_via_listing("./Cargo").unwrap();
        assert!(out.iter().any(|m| m == "./Cargo.toml"), "{out:?}");
        drop(cwd);
    }

    #[test]
    fn display_path_preserves_fragment_notation() {
        assert_eq!(display_path("~/Doc", "Documents", true), "~/Documents/");
        assert_eq!(display_path("./sr", "src", true), "./src/");
        assert_eq!(display_path("/us", "usr", true), "/usr/");
    }
}
