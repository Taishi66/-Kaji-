//! Read-only file pane (task 8) — what `Enter` on the fuzzy finder opens, and
//! the slot the SPEC panel lends for as long as it stays open.
//!
//! A viewer is a snapshot, not a live file handle: the file is read once,
//! bounded at [`READ_LIMIT`], and kept as display-ready lines. A multi-gigabyte
//! log opened by accident costs one bounded read, exactly like the @-mention
//! attachments (`mentions::render_file_from`).

use anyhow::{Result, bail};
use std::io::Read;
use std::path::Path;

/// Hard ceiling on what one viewer pulls into memory.
const READ_LIMIT: usize = 256 * 1024;
/// A NUL in the head of the file is what tells text from binary — reading the
/// whole buffer to answer that question would be work spent on a file that is
/// never going to be displayed anyway.
const BINARY_SNIFF: usize = 8 * 1024;
const TAB: &str = "    ";

#[derive(Debug)]
pub struct Viewer {
    /// As typed/selected — project-relative for anything the index served,
    /// which is also what `a` attaches to the composer as `@path`.
    pub path: String,
    /// Display-ready: tabs expanded, control characters neutralized.
    pub lines: Vec<String>,
    /// Index of the first visible line.
    pub scroll: usize,
    /// The file is larger than [`READ_LIMIT`] and the tail was not read.
    pub truncated: bool,
    pub binary: bool,
}

/// `display` is the path as the user knows it, `path` the resolved one to read.
pub fn load(display: &str, path: &Path) -> Result<Viewer> {
    if path.is_dir() {
        bail!("{display} est un dossier");
    }
    let file = std::fs::File::open(path)?;
    let size = file.metadata().map(|m| m.len()).unwrap_or_default();
    from_reader(display, file, size)
}

/// Split from [`load`] so the bounded read can be tested against a reader that
/// refuses to serve more than the budget — same seam as
/// `mentions::render_file_from`.
fn from_reader(display: &str, reader: impl Read, size: u64) -> Result<Viewer> {
    let mut bytes = Vec::new();
    reader.take(READ_LIMIT as u64 + 1).read_to_end(&mut bytes)?;
    let truncated = bytes.len() > READ_LIMIT;
    bytes.truncate(READ_LIMIT);
    let head = &bytes[..bytes.len().min(BINARY_SNIFF)];
    if head.contains(&0) {
        return Ok(Viewer {
            path: display.to_string(),
            lines: vec![format!("fichier binaire ({})", human_size(size))],
            scroll: 0,
            truncated: false,
            binary: true,
        });
    }
    let text = String::from_utf8_lossy(&bytes);
    let lines = text
        .lines()
        .map(|line| crate::tui::ui::sanitize_for_display(&line.replace('\t', TAB)))
        .collect();
    Ok(Viewer {
        path: display.to_string(),
        lines,
        scroll: 0,
        truncated,
        binary: false,
    })
}

/// What the truncation notice says was read — a truncated read always stopped
/// at exactly [`READ_LIMIT`].
pub fn read_limit_label() -> String {
    human_size(READ_LIMIT as u64)
}

fn human_size(bytes: u64) -> String {
    const KO: u64 = 1024;
    const MO: u64 = KO * KO;
    if bytes >= MO {
        format!("{:.1} Mo", bytes as f64 / MO as f64)
    } else if bytes >= KO {
        format!("{} Ko", bytes / KO)
    } else {
        format!("{bytes} o")
    }
}

impl Viewer {
    /// Highest scroll offset that still fills the viewport — scrolling past it
    /// would paint blank rows under the last line.
    pub fn max_scroll(&self, viewport: usize) -> usize {
        self.lines.len().saturating_sub(viewport.max(1))
    }

    pub fn scroll_down(&mut self, lines: usize, viewport: usize) {
        self.scroll = (self.scroll + lines).min(self.max_scroll(viewport));
    }

    pub fn scroll_up(&mut self, lines: usize) {
        self.scroll = self.scroll.saturating_sub(lines);
    }

    pub fn scroll_to_start(&mut self) {
        self.scroll = 0;
    }

    pub fn scroll_to_end(&mut self, viewport: usize) {
        self.scroll = self.max_scroll(viewport);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &tempfile::TempDir, name: &str, content: impl AsRef<[u8]>) -> std::path::PathBuf {
        let path = dir.path().join(name);
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn loads_a_text_file_as_display_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(&dir, "a.rs", "fn main() {}\nok\n");
        let viewer = load("a.rs", &path).unwrap();
        assert_eq!(viewer.lines, vec!["fn main() {}", "ok"]);
        assert_eq!(viewer.path, "a.rs");
        assert!(!viewer.truncated);
        assert!(!viewer.binary);
    }

    #[test]
    fn expands_tabs_and_neutralizes_control_characters() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(&dir, "t.txt", "\tindenté\nesc\u{1b}[31m");
        let viewer = load("t.txt", &path).unwrap();
        assert_eq!(viewer.lines[0], "    indenté");
        assert!(viewer.lines[1].contains('␛'), "{:?}", viewer.lines[1]);
    }

    #[test]
    fn a_file_past_the_limit_is_truncated_not_swallowed() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(&dir, "huge.log", "ligne de log\n".repeat(100_000));
        let viewer = load("huge.log", &path).unwrap();
        assert!(viewer.truncated);
        assert!(
            viewer.lines.len() < 100_000,
            "{} lignes",
            viewer.lines.len()
        );
        let kept: usize = viewer.lines.iter().map(String::len).sum();
        assert!(kept <= READ_LIMIT, "{kept} octets gardés");
        assert_eq!(read_limit_label(), "256 Ko");
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
                .expect("lecture non bornée : plus de limite + 1 octets demandés");
            Ok(n)
        }
    }

    #[test]
    fn never_reads_more_than_the_limit() {
        let reader = Fuse {
            inner: std::io::repeat(b'a'),
            remaining: READ_LIMIT + 1,
        };
        let viewer = from_reader("infini.log", reader, u64::MAX).unwrap();
        assert!(viewer.truncated);
        assert_eq!(viewer.lines.len(), 1);
        assert_eq!(viewer.lines[0].len(), READ_LIMIT);
    }

    #[test]
    fn a_binary_file_is_announced_with_its_size_instead_of_its_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(&dir, "bin.dat", [0u8, 159, 146, 150]);
        let viewer = load("bin.dat", &path).unwrap();
        assert!(viewer.binary);
        assert_eq!(viewer.lines, vec!["fichier binaire (4 o)"]);
    }

    #[test]
    fn a_directory_is_refused_rather_than_read() {
        let dir = tempfile::tempdir().unwrap();
        let err = load("sub/", dir.path()).unwrap_err();
        assert!(err.to_string().contains("dossier"), "{err}");
    }

    #[test]
    fn a_missing_file_reports_an_error_instead_of_panicking() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load("nope.txt", &dir.path().join("nope.txt")).is_err());
    }

    #[test]
    fn scroll_clamps_to_the_last_page_and_to_the_top() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(&dir, "n.txt", "x\n".repeat(100));
        let mut viewer = load("n.txt", &path).unwrap();
        assert_eq!(viewer.lines.len(), 100);

        viewer.scroll_down(1_000, 20);
        assert_eq!(viewer.scroll, 80, "jamais au-delà de la dernière page");
        viewer.scroll_up(5);
        assert_eq!(viewer.scroll, 75);
        viewer.scroll_up(1_000);
        assert_eq!(viewer.scroll, 0);
        viewer.scroll_to_end(20);
        assert_eq!(viewer.scroll, 80);
        viewer.scroll_to_start();
        assert_eq!(viewer.scroll, 0);
    }

    #[test]
    fn a_file_shorter_than_the_viewport_never_scrolls() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(&dir, "s.txt", "a\nb\n");
        let mut viewer = load("s.txt", &path).unwrap();
        viewer.scroll_down(10, 40);
        assert_eq!(viewer.scroll, 0);
    }

    #[test]
    fn human_size_scales_with_the_file() {
        assert_eq!(human_size(12), "12 o");
        assert_eq!(human_size(4096), "4 Ko");
        assert_eq!(human_size(3 * 1024 * 1024), "3.0 Mo");
    }
}
