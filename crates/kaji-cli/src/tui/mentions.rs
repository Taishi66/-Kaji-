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
//!   `<attached-file>` / `<attached-directory>` blocks, and lifts image
//!   mentions out of the text into [`MentionImage`] attachments the caller
//!   turns into multimodal message content (S1). The chat line keeps the text
//!   as typed — only the model-bound message carries the payload.
//!
//! S1 n'attache que des images fixes : un GIF animé est refusé ici, avec sa
//! raison, plutôt que renvoyé bien plus tard par une erreur d'API du provider.
//! Seul le GIF est inspecté — un WebP animé passe et part au provider, qui
//! tranche. Périmètre assumé : le GIF est la forme animée qu'un utilisateur
//! mentionne en pratique, et la reconnaître demande six octets d'en-tête là où
//! le WebP demanderait de parcourir ses chunks RIFF.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine as _;

use crate::tui::theme;
use crate::tui::viewer::human_size;

pub(crate) const INDEX_TTL: Duration = Duration::from_secs(60);
const MAX_INDEX_ENTRIES: usize = 20_000;
const MAX_COMPLETIONS: usize = 8;
const MAX_LISTING_SCAN: usize = 500;
const MAX_FILE_BYTES: usize = 64 * 1024;
const MAX_TOTAL_BYTES: usize = 256 * 1024;
const MAX_DIR_ENTRIES: usize = 200;
/// Caps S1 : au-delà, l'image n'est pas attachée et le message part quand
/// même — ce sont les deux limites qu'un provider vision fait respecter de
/// toute façon, appliquées avant de charger quoi que ce soit en mémoire.
const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;
const MAX_IMAGES: usize = 3;
const GIF_MIME: &str = "image/gif";
const IMAGE_MIMES: [(&str, &str); 5] = [
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("webp", "image/webp"),
    ("gif", GIF_MIME),
];

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

    /// Fuzzy-ranked paths for the file finder (task 8), best first. Kept apart
    /// from [`MentionIndex::complete`] because they answer different
    /// questions: the dropdown finishes the fragment being typed, the finder
    /// looks for a file. Both read the same snapshot and the same
    /// pre-lowercased entries.
    pub fn search(&self, query: &str, cap: usize) -> Vec<String> {
        crate::tui::fuzzy::rank(query, self.entries.iter().map(|e| e.lower.as_str()))
            .into_iter()
            .take(cap)
            .map(|(idx, _)| self.entries[idx].path.clone())
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

/// An image mention resolved into provider-ready content: base64 payload plus
/// its mime type, as `MessageContentBlock::Image` wants them. `bytes` is the
/// file's size on disk, not the encoded length — it's what the placeholder
/// line reports, and what the caps are measured against.
#[derive(Debug, Clone, PartialEq)]
pub struct MentionImage {
    pub name: String,
    pub mime: String,
    pub data: String,
    pub bytes: u64,
}

impl MentionImage {
    /// The chat line standing in for the image: the terminal renders nothing
    /// of the picture itself in v1, so the transcript says what left with the
    /// message.
    pub fn placeholder(&self) -> String {
        format!(
            "{} {} ({})",
            theme::IMAGE_GLYPH,
            self.name,
            human_size(self.bytes)
        )
    }
}

/// What a submitted line expands into: the model-bound text, the images
/// lifted out of it, and the lines explaining every image that was refused.
#[derive(Debug, Default)]
pub struct MentionExpansion {
    pub text: String,
    pub images: Vec<MentionImage>,
    pub notices: Vec<String>,
}

/// Rewrites the submitted text: every `@path` that resolves to an existing
/// file or directory (relative to `cwd`, with `~/` expansion) gets its
/// payload appended as an attachment block. A mention whose extension is an
/// image format leaves the text alone and becomes an attachment instead.
/// Unresolvable mentions are left as-is — the agent can still act on the raw
/// path. Every block is rendered under the budget still left, so the total
/// never exceeds `MAX_TOTAL_BYTES`; images have their own caps and never
/// borrow from that budget.
pub fn expand_mentions(text: &str, cwd: &Path) -> MentionExpansion {
    let mut attachments = String::new();
    let mut total = 0usize;
    let mut images = Vec::new();
    let mut notices = Vec::new();
    for token in text.split_whitespace() {
        let Some(raw) = token.strip_prefix('@') else {
            continue;
        };
        if raw.is_empty() {
            continue;
        }
        let path = resolve(raw, cwd);
        if let Some(mime) = image_mime(&path) {
            if path.is_file() {
                match load_image(&path, mime, images.len()) {
                    Ok(image) => images.push(image),
                    Err(notice) => notices.push(notice),
                }
                continue;
            }
        }
        if total >= MAX_TOTAL_BYTES {
            continue;
        }
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
    let text = if attachments.is_empty() {
        text.to_string()
    } else {
        format!("{text}\n{attachments}")
    };
    MentionExpansion {
        text,
        images,
        notices,
    }
}

/// The mime type an image mention would carry, from its extension alone —
/// the routing decision has to be made before the file is opened, and a
/// provider reads the mime we declare, not the bytes' magic number.
fn image_mime(path: &Path) -> Option<&'static str> {
    let extension = path.extension()?.to_str()?.to_lowercase();
    IMAGE_MIMES
        .iter()
        .find(|(candidate, _)| *candidate == extension)
        .map(|(_, mime)| *mime)
}

/// Loads an image mention under the S1 caps. The size is read from the
/// metadata first: a mistyped `@video.png` of three gigabytes costs a `stat`,
/// not three gigabytes of RAM. Refusals come back as the line the composer
/// shows, so the message itself still goes out.
fn load_image(path: &Path, mime: &str, already_attached: usize) -> Result<MentionImage, String> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned());
    if already_attached >= MAX_IMAGES {
        return Err(refused(
            &name,
            &format!("{MAX_IMAGES} images maximum par message"),
        ));
    }
    let bytes = std::fs::metadata(path)
        .map_err(|e| refused(&name, &e.to_string()))?
        .len();
    if bytes > MAX_IMAGE_BYTES {
        return Err(refused(
            &name,
            &format!(
                "{} dépasse la limite de {} par image",
                human_size(bytes),
                human_size(MAX_IMAGE_BYTES)
            ),
        ));
    }
    let raw = std::fs::read(path).map_err(|e| refused(&name, &e.to_string()))?;
    if mime == GIF_MIME && gif_is_animated(&raw) {
        return Err(refused(
            &name,
            "GIF animé : S1 n'attache que des images fixes — exporter une image du GIF",
        ));
    }
    Ok(MentionImage {
        name,
        mime: mime.to_string(),
        data: base64::prelude::BASE64_STANDARD.encode(&raw),
        bytes,
    })
}

fn refused(name: &str, reason: &str) -> String {
    format!("{} {name} — non attachée : {reason}", theme::IMAGE_GLYPH)
}

/// Un GIF porte-t-il plus d'une image ? S1 n'attache que des GIF fixes, et le
/// refus doit tomber ici plutôt qu'au provider : local, immédiat, actionnable.
///
/// La question se tranche en **marchant les blocs**, jamais en cherchant un
/// motif dans les octets — les données LZW peuvent contenir n'importe quelle
/// séquence, y compris celle d'un descripteur d'image. Une structure qu'on
/// n'arrive pas à marcher rend `false` : on ne refuse que ce qu'on a su
/// prouver, le provider tranche le reste.
fn gif_is_animated(bytes: &[u8]) -> bool {
    let Some(mut cursor) = bytes
        .strip_prefix(b"GIF87a")
        .or_else(|| bytes.strip_prefix(b"GIF89a"))
    else {
        return false;
    };

    // Logical screen descriptor : 7 octets dont le champ « packed » en 5e
    // position, qui annonce la table de couleurs globale et sa taille.
    let Some((packed, rest)) = cursor.get(4).copied().zip(cursor.get(7..)) else {
        return false;
    };
    cursor = rest;
    let Some(rest) = skip_color_table(cursor, packed) else {
        return false;
    };
    cursor = rest;

    let mut frames = 0usize;
    loop {
        let Some((&block, rest)) = cursor.split_first() else {
            return false;
        };
        cursor = rest;
        let rest = match block {
            GIF_TRAILER => return false,
            GIF_EXTENSION => cursor.get(1..).and_then(skip_sub_blocks),
            GIF_IMAGE_DESCRIPTOR => {
                frames += 1;
                if frames > 1 {
                    return true;
                }
                // 9 octets de descripteur dont le « packed » final, puis la
                // table locale, la taille de code LZW et les données.
                cursor
                    .get(8)
                    .copied()
                    .zip(cursor.get(9..))
                    .and_then(|(packed, rest)| skip_color_table(rest, packed))
                    .and_then(|rest| rest.get(1..))
                    .and_then(skip_sub_blocks)
            }
            _ => return false,
        };
        match rest {
            Some(rest) => cursor = rest,
            None => return false,
        }
    }
}

const GIF_EXTENSION: u8 = 0x21;
const GIF_IMAGE_DESCRIPTOR: u8 = 0x2c;
const GIF_TRAILER: u8 = 0x3b;

/// Saute la table de couleurs annoncée par un champ « packed » : bit 7 pour sa
/// présence, bits 0-2 pour sa taille.
fn skip_color_table(cursor: &[u8], packed: u8) -> Option<&[u8]> {
    if packed & 0x80 == 0 {
        return Some(cursor);
    }
    cursor.get(3 * (1usize << ((packed & 0x07) + 1))..)
}

/// Saute une chaîne de sous-blocs — chacun préfixé de sa longueur, la chaîne
/// close par un bloc vide.
fn skip_sub_blocks(mut cursor: &[u8]) -> Option<&[u8]> {
    loop {
        let (&len, rest) = cursor.split_first()?;
        cursor = rest;
        if len == 0 {
            return Some(cursor);
        }
        cursor = cursor.get(usize::from(len)..)?;
    }
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
        let out = expand_mentions("lis @a.txt stp", dir.path()).text;
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
        let out = expand_mentions("@sub/", dir.path()).text;
        assert!(out.contains("<attached-directory path=\"sub/\">"));
        assert!(out.contains("b.rs"));
    }

    #[test]
    fn expand_mentions_leaves_unknown_paths_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let out = expand_mentions("regarde @nope/rien.txt", dir.path()).text;
        assert_eq!(out, "regarde @nope/rien.txt");
    }

    #[test]
    fn expand_mentions_skips_binary_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("bin.dat"), [0u8, 159, 146, 150]).unwrap();
        let out = expand_mentions("@bin.dat", dir.path()).text;
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
        let out = expand_mentions(text, dir.path()).text;
        let attachments = out.len() - text.len() - 1;
        assert!(attachments <= MAX_TOTAL_BYTES, "{attachments}");
        assert!(out.contains("<attached-directory path=\"sub/\">"));
        assert!(out.contains("… (listing truncated)"));
    }

    /// Les octets d'en-tête des formats v1 — la reconnaissance se fait sur
    /// l'extension, mais des fixtures aux bons nombres magiques gardent les
    /// tests honnêtes vis-à-vis de ce qu'un provider recevrait vraiment.
    const PNG: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];

    fn write_png(dir: &Path, name: &str) {
        std::fs::write(dir.join(name), PNG).unwrap();
    }

    #[test]
    fn image_mentions_attach_instead_of_inlining() {
        use base64::Engine as _;

        let dir = tempfile::tempdir().unwrap();
        write_png(dir.path(), "capture.png");
        let out = expand_mentions("regarde @capture.png stp", dir.path());
        assert_eq!(out.text, "regarde @capture.png stp");
        assert!(out.notices.is_empty(), "{:?}", out.notices);
        assert_eq!(out.images.len(), 1);
        let image = &out.images[0];
        assert_eq!(image.name, "capture.png");
        assert_eq!(image.mime, "image/png");
        assert_eq!(image.bytes, PNG.len() as u64);
        assert_eq!(
            base64::prelude::BASE64_STANDARD
                .decode(&image.data)
                .unwrap(),
            PNG
        );
    }

    #[test]
    fn image_mentions_cover_the_v1_formats() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["a.png", "b.jpg", "c.jpeg", "d.webp", "e.gif", "f.PNG"] {
            std::fs::write(dir.path().join(name), PNG).unwrap();
        }
        let mimes: Vec<String> = ["a.png", "b.jpg", "c.jpeg", "d.webp", "e.gif", "f.PNG"]
            .iter()
            .map(|name| {
                let out = expand_mentions(&format!("@{name}"), dir.path());
                assert_eq!(out.images.len(), 1, "{name}: {out:?}");
                out.images[0].mime.clone()
            })
            .collect();
        assert_eq!(
            mimes,
            [
                "image/png",
                "image/jpeg",
                "image/jpeg",
                "image/webp",
                "image/gif",
                "image/png",
            ]
        );
    }

    /// Un GIF minimal à `frames` images : en-tête, écran logique sans table
    /// globale, puis une extension de contrôle graphique et un descripteur
    /// d'image par frame.
    fn gif(frames: usize) -> Vec<u8> {
        let mut bytes = b"GIF89a".to_vec();
        bytes.extend_from_slice(&[0x01, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00]);
        for _ in 0..frames {
            bytes.extend_from_slice(&[0x21, 0xf9, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00]);
            bytes.extend_from_slice(&[0x2c, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00]);
            bytes.extend_from_slice(&[0x02, 0x02, 0x4c, 0x01, 0x00]);
        }
        bytes.push(0x3b);
        bytes
    }

    /// S1 attache « gif (non animé) ». Sans ce refus local, un GIF animé part
    /// jusqu'au provider et revient en erreur d'API, bien plus tard et sans
    /// dire quoi faire.
    #[test]
    fn an_animated_gif_is_refused_before_it_reaches_the_provider() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("boucle.gif"), gif(2)).unwrap();
        let out = expand_mentions("@boucle.gif", dir.path());
        assert!(out.images.is_empty(), "{out:?}");
        assert_eq!(out.notices.len(), 1);
        assert!(out.notices[0].contains("boucle.gif"), "{}", out.notices[0]);
        assert!(out.notices[0].contains("animé"), "{}", out.notices[0]);
    }

    #[test]
    fn a_still_gif_is_still_attached() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("fixe.gif"), gif(1)).unwrap();
        let out = expand_mentions("@fixe.gif", dir.path());
        assert_eq!(out.images.len(), 1, "{out:?}");
        assert_eq!(out.images[0].mime, GIF_MIME);
    }

    /// On ne refuse que ce qu'on a su prouver : un fichier `.gif` qu'on
    /// n'arrive pas à marcher part au provider, qui tranche.
    #[test]
    fn an_unparseable_gif_is_left_to_the_provider() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("tronque.gif"), b"GIF89a\x01\x00\x01").unwrap();
        let out = expand_mentions("@tronque.gif", dir.path());
        assert_eq!(out.images.len(), 1, "{out:?}");
    }

    #[test]
    fn a_non_image_extension_still_inlines_its_text() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("notes.md"), "du texte").unwrap();
        let out = expand_mentions("@notes.md", dir.path());
        assert!(out.images.is_empty());
        assert!(out.text.contains("<attached-file path=\"notes.md\">"));
        assert!(out.text.contains("du texte"));
    }

    #[test]
    fn a_message_carries_at_most_three_images() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["a.png", "b.png", "c.png", "d.png"] {
            write_png(dir.path(), name);
        }
        let out = expand_mentions("@a.png @b.png @c.png @d.png", dir.path());
        assert_eq!(out.images.len(), MAX_IMAGES);
        assert_eq!(out.notices.len(), 1, "{:?}", out.notices);
        assert!(out.notices[0].contains("d.png"), "{}", out.notices[0]);
        assert!(out.notices[0].contains('3'), "{}", out.notices[0]);
        assert!(!out.images.iter().any(|image| image.name == "d.png"));
    }

    #[test]
    fn an_image_over_the_size_cap_is_refused_and_the_rest_of_the_message_goes_out() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("enorme.png"),
            vec![0u8; (MAX_IMAGE_BYTES + MAX_IMAGE_BYTES / 10) as usize],
        )
        .unwrap();
        let out = expand_mentions("compare @enorme.png à ça", dir.path());
        assert!(out.images.is_empty());
        assert_eq!(out.text, "compare @enorme.png à ça");
        assert_eq!(out.notices.len(), 1, "{:?}", out.notices);
        assert!(out.notices[0].contains("enorme.png"), "{}", out.notices[0]);
        assert!(out.notices[0].contains("5.5 Mo"), "{}", out.notices[0]);
        assert!(
            out.notices[0].contains("limite de 5.0 Mo"),
            "{}",
            out.notices[0]
        );
    }

    /// La borne est inclusive : une image pile à la limite passe, la même
    /// plus un octet est refusée — sinon « 5 Mo » n'aurait pas de sens.
    #[test]
    fn the_size_cap_is_inclusive() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("pile.png"),
            vec![0u8; MAX_IMAGE_BYTES as usize],
        )
        .unwrap();
        std::fs::write(
            dir.path().join("un-de-trop.png"),
            vec![0u8; MAX_IMAGE_BYTES as usize + 1],
        )
        .unwrap();
        let out = expand_mentions("@pile.png", dir.path());
        assert_eq!(out.images.len(), 1, "{:?}", out.notices);
        let out = expand_mentions("@un-de-trop.png", dir.path());
        assert!(out.images.is_empty());
        assert_eq!(out.notices.len(), 1);
    }

    #[test]
    fn images_never_eat_the_text_attachment_budget() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("gros.png"), vec![0u8; 400 * 1024]).unwrap();
        std::fs::write(dir.path().join("notes.txt"), "contenu texte").unwrap();
        let out = expand_mentions("@gros.png @notes.txt", dir.path());
        assert_eq!(out.images.len(), 1);
        assert!(out.text.contains("contenu texte"), "{}", out.text);
        assert!(out.text.len() < MAX_TOTAL_BYTES);
    }

    #[test]
    fn the_placeholder_names_the_file_and_its_size() {
        let image = MentionImage {
            name: "capture.png".to_string(),
            mime: "image/png".to_string(),
            data: String::new(),
            bytes: 1_258_291,
        };
        assert_eq!(image.placeholder(), "画 capture.png (1.2 Mo)");
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
