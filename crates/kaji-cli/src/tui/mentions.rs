//! @-mentions in the composer (item 4 ante): typing `@<path>` completes files
//! and directories, and mentioned paths are embedded into the submitted
//! message — a file's contents, a directory's listing — so the agent sees the
//! exact context without searching.
//!
//! Two halves:
//! - completion: [`MentionIndex`] walks the project once (respecting
//!   `.gitignore` via the `ignore` crate) on a blocking task owned by
//!   `event_loop`, plus direct directory listings for `~/`, absolute, `./`,
//!   `../` fragments — those hit a single `read_dir` and stay inline;
//! - expansion: [`expand_mentions`] rewrites the submitted text, appending
//!   `<attached-file>` / `<attached-directory>` blocks. The chat line keeps
//!   the text as typed — only the model-bound message carries the payload.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub(crate) const INDEX_TTL: Duration = Duration::from_secs(60);
const MAX_INDEX_ENTRIES: usize = 20_000;
const MAX_COMPLETIONS: usize = 8;
const MAX_LISTING_SCAN: usize = 500;
const MAX_FILE_BYTES: usize = 64 * 1024;
const MAX_TOTAL_BYTES: usize = 256 * 1024;
const MAX_DIR_ENTRIES: usize = 200;

struct IndexedPath {
    /// Project-relative, directories with a trailing `/`.
    path: String,
    lower: String,
}

/// Project file index backing `@` completion. Built off the event loop and
/// swapped in as a whole snapshot; the walk stops at `MAX_INDEX_ENTRIES` so a
/// huge tree (vendor, target/) can't grind the build thread forever.
pub struct MentionIndex {
    entries: Vec<IndexedPath>,
    truncated: bool,
}

impl MentionIndex {
    /// Blocking walk — never call this from the event loop task. `App` only
    /// ever emits a build request (`App::take_mention_index_request`) that
    /// `event_loop` runs on `spawn_blocking`.
    pub fn build(root: PathBuf) -> Self {
        Self::build_capped(&root, MAX_INDEX_ENTRIES)
    }

    fn build_capped(root: &Path, cap: usize) -> Self {
        let mut entries = Vec::new();
        let mut truncated = false;
        let walker = ignore::WalkBuilder::new(root)
            .hidden(true)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .sort_by_file_name(|a, b| a.cmp(b))
            .build();
        for entry in walker.flatten() {
            if entry.path() == root {
                continue;
            }
            let Ok(rel) = entry.path().strip_prefix(root) else {
                continue;
            };
            let mut path = rel.to_string_lossy().replace('\\', "/");
            if entry.file_type().is_some_and(|t| t.is_dir()) {
                path.push('/');
            }
            let lower = path.to_lowercase();
            entries.push(IndexedPath { path, lower });
            if entries.len() >= cap {
                truncated = true;
                break;
            }
        }
        Self { entries, truncated }
    }

    /// The walk stopped at the cap — the dropdown says so rather than letting
    /// whole subtrees look nonexistent.
    pub fn truncated(&self) -> bool {
        self.truncated
    }

    /// Case-insensitive substring match over the project index, shortest
    /// first. `~/`, absolute, `./` and `../` fragments never reach here —
    /// [`complete_via_listing`] serves those from a single `read_dir`.
    pub fn complete(&self, fragment: &str) -> Vec<String> {
        let needle = fragment.to_lowercase();
        let mut matches: Vec<&IndexedPath> = self
            .entries
            .iter()
            .filter(|e| e.lower.contains(&needle))
            .collect();
        matches.sort_by(|a, b| {
            a.path
                .len()
                .cmp(&b.path.len())
                .then_with(|| a.lower.cmp(&b.lower))
        });
        matches
            .into_iter()
            .take(MAX_COMPLETIONS)
            .map(|e| e.path.clone())
            .collect()
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

/// Fragments the project index does not cover: they address files outside
/// (or relative around) the project root.
fn is_listing_fragment(fragment: &str) -> bool {
    fragment.starts_with("~/")
        || fragment.starts_with('/')
        || fragment.starts_with("./")
        || fragment.starts_with("../")
}

/// Completes `~/…`, `/…`, `./…`, `../…` fragments by listing the parent
/// directory directly, resolving relative fragments against `base`. Returns
/// `None` for plain project-relative fragments, which the index serves.
/// Reads at most `MAX_LISTING_SCAN` entries so a directory with a huge fanout
/// stays cheap enough to run inline on the keystroke path.
pub(crate) fn complete_via_listing(fragment: &str, base: &Path) -> Option<Vec<String>> {
    if !is_listing_fragment(fragment) {
        return None;
    }
    let expanded = expand_home(fragment);
    let path = Path::new(&expanded);
    let (dir, prefix) = if fragment.ends_with('/') {
        (path.to_path_buf(), String::new())
    } else {
        match path.parent() {
            Some(p) if !p.as_os_str().is_empty() => (
                p.to_path_buf(),
                path.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    .to_string(),
            ),
            _ => (path.to_path_buf(), String::new()),
        }
    };
    let dir = if dir.is_absolute() {
        dir
    } else {
        base.join(dir)
    };
    let mut out: Vec<String> = Vec::new();
    let prefix_lc = prefix.to_lowercase();
    for entry in std::fs::read_dir(&dir)
        .ok()?
        .flatten()
        .take(MAX_LISTING_SCAN)
    {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        if !prefix_lc.is_empty() && !name.to_lowercase().starts_with(&prefix_lc) {
            continue;
        }
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        out.push(display_path(fragment, &name, is_dir));
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

pub(crate) fn expand_home(fragment: &str) -> String {
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

/// Resolves a mention path the same way completion does: `~/` expanded,
/// absolute kept, anything else relative to `base`.
pub(crate) fn resolve(raw: &str, base: &Path) -> PathBuf {
    let expanded = expand_home(raw);
    let path = Path::new(&expanded);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

/// Rewrites the submitted text: every `@path` that resolves to an existing
/// file or directory (relative to `cwd`, with `~/` expansion) gets its
/// payload appended as an attachment block. Unresolvable mentions are left
/// as-is — the agent can still act on the raw path. Every block is rendered
/// under the budget still left, so the total never exceeds
/// `MAX_TOTAL_BYTES`.
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
        if total >= MAX_TOTAL_BYTES {
            break;
        }
        let path = resolve(raw, cwd);
        let remaining = MAX_TOTAL_BYTES - total;
        let block = if path.is_file() {
            render_file(raw, &path, MAX_FILE_BYTES.min(remaining))
        } else if path.is_dir() {
            render_dir(raw, &path, remaining)
        } else {
            None
        };
        if let Some(block) = block {
            total += block.len();
            attachments.push_str(&block);
        }
    }
    if attachments.is_empty() {
        return text.to_string();
    }
    format!("{text}\n{attachments}")
}

/// Reads at most `budget` bytes — a multi-GB log mentioned by accident costs
/// one bounded read, not its own size in RAM. `budget` caps the whole block,
/// wrapper included, so the caller's running total stays exact.
fn render_file(display: &str, path: &Path, budget: usize) -> Option<String> {
    render_file_from(std::fs::File::open(path).ok()?, display, budget)
}

/// Split from `render_file` so the bounded read can be tested against a
/// reader that refuses to serve more than the budget — the file-based test
/// only ever sees the trimmed output, which a full read would also produce.
fn render_file_from(reader: impl Read, display: &str, budget: usize) -> Option<String> {
    let open = format!("\n<attached-file path=\"{display}\">\n");
    let close = "\n</attached-file>\n";
    let content_budget = budget.checked_sub(open.len() + close.len())?;
    let mut bytes = Vec::new();
    reader
        .take(content_budget as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.contains(&0) {
        let block = format!(
            "\n<attached-file path=\"{display}\">(binary file — content skipped)</attached-file>\n"
        );
        return (block.len() <= budget).then_some(block);
    }
    let mut content = String::from_utf8_lossy(&bytes).into_owned();
    if content.len() > content_budget {
        const MARKER: &str = "\n… (truncated)";
        truncate_on_char_boundary(&mut content, content_budget.saturating_sub(MARKER.len()));
        content.push_str(MARKER);
    }
    Some(format!("{open}{content}{close}"))
}

/// `String::truncate` panics off a char boundary — a 64 KiB cut landing
/// inside a multibyte char is not exotic in a UTF-8 source tree.
fn truncate_on_char_boundary(s: &mut String, max: usize) {
    if s.len() <= max {
        return;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
}

fn render_dir(display: &str, path: &Path, budget: usize) -> Option<String> {
    const MARKER: &str = "… (listing truncated)\n";
    let open = format!("\n<attached-directory path=\"{display}\">\n");
    let close = "</attached-directory>\n";
    let listing_budget = budget.checked_sub(open.len() + close.len())?;
    let mut listing = String::new();
    let mut truncated = false;
    let mut count = 0;
    let mut entries = ignore::WalkBuilder::new(path)
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .max_depth(Some(3))
        .build()
        .flatten()
        .filter(|entry| entry.path() != path)
        .peekable();
    while let Some(entry) = entries.next() {
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
        // Room for the marker is only owed while entries remain: reserving it
        // for the last one would announce a truncation that never happened.
        let reserve = if entries.peek().is_some() {
            MARKER.len()
        } else {
            0
        };
        let line_len = rel.len() + suffix.len() + 1;
        if count >= MAX_DIR_ENTRIES || listing.len() + line_len + reserve > listing_budget {
            truncated = true;
            break;
        }
        listing.push_str(&rel);
        listing.push_str(suffix);
        listing.push('\n');
        count += 1;
    }
    if truncated {
        if listing.len() + MARKER.len() > listing_budget {
            // Not even room to say the listing was cut — say that instead of
            // shipping entries that would read as the whole directory.
            let note = format!(
                "\n<attached-directory path=\"{display}\">(budget exhausted)</attached-directory>\n"
            );
            return (note.len() <= budget).then_some(note);
        }
        listing.push_str(MARKER);
    }
    Some(format!("{open}{listing}{close}"))
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
    fn render_file_block_fits_the_budget() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("huge.log");
        std::fs::write(&path, "x".repeat(1024 * 1024)).unwrap();
        let block = render_file("huge.log", &path, MAX_FILE_BYTES).unwrap();
        assert!(block.len() <= MAX_FILE_BYTES, "{}", block.len());
        assert!(block.contains("… (truncated)"));
    }

    /// A reader that blows up past `remaining` bytes: an unbounded
    /// `read_to_end` fails the test instead of looping on infinite input.
    struct Fuse<R: Read> {
        inner: R,
        remaining: usize,
    }

    impl<R: Read> Read for Fuse<R> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.inner.read(buf)?;
            self.remaining = self
                .remaining
                .checked_sub(n)
                .expect("lecture non bornée : plus de budget + 1 octets demandés");
            Ok(n)
        }
    }

    #[test]
    fn render_file_never_requests_more_than_the_budget() {
        let reader = Fuse {
            inner: std::io::repeat(b'a'),
            remaining: MAX_FILE_BYTES + 1,
        };
        let block = render_file_from(reader, "infini.log", MAX_FILE_BYTES).unwrap();
        assert!(block.len() <= MAX_FILE_BYTES, "{}", block.len());
        assert!(block.contains("… (truncated)"));
    }

    #[test]
    fn render_file_truncates_on_a_char_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("multibyte.txt");
        std::fs::write(&path, "é".repeat(200)).unwrap();
        let block = render_file("multibyte.txt", &path, 120).unwrap();
        assert!(block.len() <= 120, "{}", block.len());
        assert!(!block.contains('\u{fffd}'), "coupé au milieu d'un char");
    }

    #[test]
    fn render_dir_respects_the_remaining_budget() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..200 {
            std::fs::write(dir.path().join(format!("file-{i:0>40}.txt")), "x").unwrap();
        }
        let block = render_dir("d/", dir.path(), 600).unwrap();
        assert!(block.len() <= 600, "{}", block.len());
        assert!(block.contains("… (listing truncated)"));
    }

    #[test]
    fn render_dir_does_not_claim_truncation_when_everything_fits() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("un-nom-de-fichier-assez-long.txt"), "x").unwrap();
        let full = render_dir("d/", dir.path(), usize::MAX).unwrap();
        let exact = render_dir("d/", dir.path(), full.len()).unwrap();
        assert_eq!(exact, full);
        assert!(!exact.contains("listing truncated"), "{exact}");
    }

    #[test]
    fn render_dir_says_the_budget_is_exhausted_instead_of_cutting_silently() {
        let dir = tempfile::tempdir().unwrap();
        let name = "un-nom-de-fichier-vraiment-tres-long.txt";
        std::fs::write(dir.path().join(name), "x").unwrap();
        let overhead = "\n<attached-directory path=\"d/\">\n</attached-directory>\n".len();
        let budget = overhead + 20;
        let block = render_dir("d/", dir.path(), budget).unwrap();
        assert!(block.contains("(budget exhausted)"), "{block}");
        assert!(!block.contains(name), "{block}");
        assert!(block.len() <= budget, "{}", block.len());
    }

    #[test]
    fn expand_mentions_total_never_exceeds_the_global_cap() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..3 {
            std::fs::write(
                dir.path().join(format!("big{i}.txt")),
                "x".repeat(100 * 1024),
            )
            .unwrap();
        }
        std::fs::write(dir.path().join("last.txt"), "y".repeat(60_000)).unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        for i in 0..200 {
            std::fs::write(sub.join(format!("entry-{i:0>60}.txt")), "z").unwrap();
        }
        let text = "@big0.txt @big1.txt @big2.txt @last.txt @sub/";
        let out = expand_mentions(text, dir.path());
        let attachments = out.len() - text.len() - 1;
        assert!(attachments <= MAX_TOTAL_BYTES, "{attachments}");
        assert!(out.contains("<attached-directory path=\"sub/\">"));
        assert!(out.contains("… (listing truncated)"));
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
        let idx = MentionIndex::build(dir.path().to_path_buf());
        let matches = idx.complete("app");
        assert!(matches.iter().any(|m| m == "src/tui/app.rs"));
        let dirs = idx.complete("tu");
        assert!(dirs.iter().any(|m| m == "src/tui/"));
        assert!(!idx.truncated());
    }

    #[test]
    fn index_honors_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "ignored.txt\n").unwrap();
        // `ignore` only applies .gitignore inside a git repo.
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join("ignored.txt"), "x").unwrap();
        std::fs::write(dir.path().join("kept.txt"), "x").unwrap();
        let idx = MentionIndex::build(dir.path().to_path_buf());
        let matches = idx.complete("txt");
        assert!(matches.iter().any(|m| m == "kept.txt"));
        assert!(!matches.iter().any(|m| m == "ignored.txt"));
    }

    #[test]
    fn index_flags_a_walk_stopped_at_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..5 {
            std::fs::write(dir.path().join(format!("f{i}.txt")), "x").unwrap();
        }
        let idx = MentionIndex::build_capped(dir.path(), 2);
        assert!(idx.truncated());
        assert_eq!(idx.entries.len(), 2);
    }

    #[test]
    fn listing_completes_dot_slash_fragments() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "x").unwrap();
        std::fs::create_dir(dir.path().join("Cargotown")).unwrap();
        let out = complete_via_listing("./Cargo", dir.path()).unwrap();
        assert!(out.iter().any(|m| m == "./Cargo.toml"), "{out:?}");
        assert!(out.iter().any(|m| m == "./Cargotown/"), "{out:?}");
    }

    #[test]
    fn listing_ignores_project_relative_fragments() {
        let dir = tempfile::tempdir().unwrap();
        assert!(complete_via_listing("src/tui", dir.path()).is_none());
    }

    #[test]
    fn display_path_preserves_fragment_notation() {
        assert_eq!(display_path("~/Doc", "Documents", true), "~/Documents/");
        assert_eq!(display_path("./sr", "src", true), "./src/");
        assert_eq!(display_path("/us", "usr", true), "/usr/");
    }
}
