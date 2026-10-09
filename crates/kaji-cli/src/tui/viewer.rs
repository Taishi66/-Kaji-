//! Read-only file pane (task 8) — what `Enter` on the fuzzy finder opens, and
//! the slot the SPEC panel lends for as long as it stays open.
//!
//! A viewer is a snapshot, not a live file handle: the file is read once,
//! bounded at [`READ_LIMIT`], and kept as display-ready lines. A multi-gigabyte
//! log opened by accident costs one bounded read, exactly like the @-mention
//! attachments (`mentions::render_file_from`).

use anyhow::Result;
use std::io::Read;
use std::path::Path;

#[derive(Debug)]
pub struct Request {
    pub id: u64,
    pub display: String,
    pub path: std::path::PathBuf,
    pub scroll: usize,
    pub reload: bool,
}

pub struct Loaded {
    pub request: Request,
    pub result: Result<Viewer>,
}

pub struct Reader {
    sender: Option<std::sync::mpsc::SyncSender<Request>>,
    pub results: tokio::sync::mpsc::Receiver<Loaded>,
    busy: bool,
    cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    stopped: std::sync::mpsc::Receiver<()>,
}

impl Reader {
    pub fn new() -> Result<Self> {
        let (sender, jobs) = std::sync::mpsc::sync_channel::<Request>(1);
        let (results_tx, results) = tokio::sync::mpsc::channel(1);
        let (stopped_tx, stopped) = std::sync::mpsc::channel();
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_cancel = cancelled.clone();
        // A single detached worker bounds concurrent reads. A stalled remote
        // mount must not hold Tokio's blocking-pool shutdown open.
        std::thread::Builder::new()
            .name("kaji-preview".to_owned())
            .spawn(move || {
                while let Ok(request) = jobs.recv() {
                    let result = load_cancellable(&request.display, &request.path, &worker_cancel);
                    if results_tx
                        .blocking_send(Loaded { request, result })
                        .is_err()
                    {
                        break;
                    }
                }
                let _ = stopped_tx.send(());
            })?;
        Ok(Self {
            sender: Some(sender),
            results,
            busy: false,
            cancelled,
            stopped,
        })
    }

    pub fn busy(&self) -> bool {
        self.busy
    }

    pub fn start(&mut self, request: Request) -> Result<()> {
        if self.busy {
            anyhow::bail!("a preview is already loading");
        }
        self.sender
            .as_ref()
            .expect("reader running")
            .try_send(request)?;
        self.busy = true;
        Ok(())
    }

    pub fn completed(&mut self) {
        self.busy = false;
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::Relaxed);
        self.sender.take();
        // Converters observe cancellation every 20 ms and reap their child.
        // A blocked filesystem read is allowed to outlive this bounded wait.
        let _ = self
            .stopped
            .recv_timeout(std::time::Duration::from_millis(100));
    }
}

/// Hard ceiling on what one viewer pulls into memory.
const READ_LIMIT: usize = 256 * 1024;
/// Bounds retained line metadata even for newline-only input.
const LINE_LIMIT: usize = 4096;
const DISPLAY_LIMIT: usize = 256 * 1024;
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
    /// The source, displayed text or line count exceeded its preview budget.
    pub truncated: bool,
    pub binary: bool,
    pub image: Option<super::documents::ImagePreview>,
    pub layout: std::cell::RefCell<Option<TextLayout>>,
}

#[derive(Debug)]
pub struct TextLayout {
    width: usize,
    pub lines: Vec<String>,
    pub truncated: bool,
}

/// `display` is the path as the user knows it, `path` the resolved one to read.
pub fn load(display: &str, path: &Path) -> Result<Viewer> {
    load_cancellable(display, path, &std::sync::atomic::AtomicBool::new(false))
}

fn load_cancellable(
    display: &str,
    path: &Path,
    cancelled: &std::sync::atomic::AtomicBool,
) -> Result<Viewer> {
    if cancelled.load(std::sync::atomic::Ordering::Relaxed) {
        anyhow::bail!("preview cancelled");
    }
    let file = super::fileio::open_regular(path)?;
    let size = file.metadata().map(|m| m.len()).unwrap_or_default();
    match super::documents::load(path, file.try_clone()?, cancelled) {
        Ok(Some(super::documents::Document::Text { text, truncated })) => {
            return Ok(from_text(display, &text, truncated));
        }
        Ok(Some(super::documents::Document::Image(image))) => {
            return Ok(Viewer {
                path: display.to_owned(),
                lines: vec!["image preview".to_owned()],
                scroll: 0,
                truncated: false,
                binary: true,
                image: Some(image),
                layout: Default::default(),
            });
        }
        Err(error) => {
            return Ok(Viewer {
                path: display.to_owned(),
                lines: vec![
                    format!("Preview unavailable: {error}"),
                    "The file was not sent to the model.".to_owned(),
                ],
                scroll: 0,
                truncated: false,
                binary: true,
                image: None,
                layout: Default::default(),
            })
        }
        Ok(None) => {}
    }
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
    let utf16 = bytes.starts_with(&[0xff, 0xfe]) || bytes.starts_with(&[0xfe, 0xff]);
    if !utf16 && (bytes.contains(&0) || invalid_utf8(&bytes, truncated)) {
        return Ok(Viewer {
            path: display.to_string(),
            lines: vec![format!("fichier binaire ({})", human_size(size))],
            scroll: 0,
            truncated: false,
            binary: true,
            image: None,
            layout: Default::default(),
        });
    }
    let text = if utf16 {
        let little = bytes[0] == 0xff;
        let units = bytes[2..]
            .chunks_exact(2)
            .map(|b| {
                if little {
                    u16::from_le_bytes([b[0], b[1]])
                } else {
                    u16::from_be_bytes([b[0], b[1]])
                }
            })
            .collect::<Vec<_>>();
        String::from_utf16_lossy(&units)
    } else {
        let text = match std::str::from_utf8(&bytes) {
            Ok(text) => text,
            Err(error) => std::str::from_utf8(&bytes[..error.valid_up_to()])?,
        };
        text.trim_start_matches('\u{feff}').to_owned()
    };
    Ok(from_text(display, &text, truncated))
}

fn invalid_utf8(bytes: &[u8], truncated: bool) -> bool {
    matches!(std::str::from_utf8(bytes), Err(error) if !truncated || error.error_len().is_some())
}

fn from_text(display: &str, text: &str, mut truncated: bool) -> Viewer {
    let mut lines = Vec::new();
    let mut remaining = DISPLAY_LIMIT;
    for line in text.lines() {
        if lines.len() == LINE_LIMIT || remaining == 0 {
            truncated = true;
            break;
        }
        let safe = crate::tui::ui::sanitize_for_display(&line.replace('\t', TAB));
        let mut end = safe.len().min(remaining);
        while !safe.is_char_boundary(end) {
            end -= 1;
        }
        truncated |= end < safe.len();
        remaining -= end;
        lines.push(safe.get(..end).expect("UTF-8 boundary").to_owned());
        if end < safe.len() {
            break;
        }
    }
    Viewer {
        path: display.to_string(),
        lines,
        scroll: 0,
        truncated,
        binary: false,
        image: None,
        layout: Default::default(),
    }
}

/// What the truncation notice says was read — a truncated read always stopped
/// at exactly [`READ_LIMIT`].
pub fn read_limit_label() -> String {
    human_size(READ_LIMIT as u64)
}

pub fn preview_limit_label() -> &'static str {
    "preview limited: 256 KB text / 4096 lines"
}

/// Shared with the composer's image attachments (`mentions`), so a file's
/// weight reads the same whether the viewer announces it or a `画` line does.
pub(crate) fn human_size(bytes: u64) -> String {
    const KO: u64 = 1024;
    const MO: u64 = KO * KO;
    if bytes >= MO {
        format!("{:.1} MB", bytes as f64 / MO as f64)
    } else if bytes >= KO {
        format!("{} KB", bytes / KO)
    } else {
        format!("{bytes} o")
    }
}

impl Viewer {
    pub fn editable(&self) -> bool {
        !self.binary && !super::documents::is_text_document(Path::new(&self.path))
    }
    pub fn prepare_layout(&self, width: usize) {
        if self.binary || !super::documents::is_text_document(Path::new(&self.path)) {
            return;
        }
        let width = width.max(2);
        if self
            .layout
            .borrow()
            .as_ref()
            .is_some_and(|layout| layout.width == width)
        {
            return;
        }
        let mut lines = Vec::new();
        let mut truncated = false;
        'source: for line in &self.lines {
            let mut start = 0;
            let mut used = 0;
            let mut last_space = None;
            for (at, ch) in line.char_indices() {
                let mut buffer = [0; 4];
                let cells = super::gitstatus::display_width(ch.encode_utf8(&mut buffer));
                if used + cells > width && at > start {
                    if lines.len() == LINE_LIMIT {
                        truncated = true;
                        break 'source;
                    }
                    let split = last_space.filter(|&split| split > start).unwrap_or(at);
                    lines.push(line.get(start..split).expect("UTF-8 boundary").to_owned());
                    start = split;
                    used = super::gitstatus::display_width(
                        line.get(start..at).expect("UTF-8 boundary"),
                    );
                    last_space = None;
                }
                used += cells;
                if ch.is_whitespace() {
                    last_space = Some(at + ch.len_utf8());
                }
            }
            if lines.len() == LINE_LIMIT {
                truncated = true;
                break;
            }
            lines.push(line.get(start..).expect("UTF-8 boundary").to_owned());
        }
        *self.layout.borrow_mut() = Some(TextLayout {
            width,
            lines,
            truncated,
        });
    }

    pub fn line_count(&self) -> usize {
        self.layout
            .borrow()
            .as_ref()
            .map_or(self.lines.len(), |layout| layout.lines.len())
    }

    /// Highest scroll offset that still fills the viewport — scrolling past it
    /// would paint blank rows under the last line.
    pub fn max_scroll(&self, viewport: usize) -> usize {
        self.line_count().saturating_sub(viewport.max(1))
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
    use std::io::Cursor;

    #[tokio::test]
    async fn a_worker_returns_a_snapshot_and_refuses_parallel_jobs() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(&dir, "document.txt", "read on a worker");
        let request = || Request {
            id: 1,
            display: "document.txt".to_owned(),
            path: path.clone(),
            scroll: 0,
            reload: false,
        };
        let mut reader = Reader::new().unwrap();
        reader.start(request()).unwrap();
        assert!(reader.start(request()).is_err());
        let loaded = tokio::time::timeout(std::time::Duration::from_secs(2), reader.results.recv())
            .await
            .unwrap()
            .unwrap();
        reader.completed();
        assert_eq!(loaded.result.unwrap().lines, ["read on a worker"]);
        assert!(!reader.busy());
    }

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
        assert_eq!(read_limit_label(), "256 KB");
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
        assert_eq!(human_size(4096), "4 KB");
        assert_eq!(human_size(3 * 1024 * 1024), "3.0 MB");
    }

    #[test]
    fn utf16_text_and_a_truncated_utf8_character_are_readable() {
        for little in [true, false] {
            let mut bytes = if little {
                vec![0xff, 0xfe]
            } else {
                vec![0xfe, 0xff]
            };
            for word in "Bonjour 世界\nSuite".encode_utf16() {
                bytes.extend(if little {
                    word.to_le_bytes()
                } else {
                    word.to_be_bytes()
                });
            }
            let viewer = from_reader("text.txt", Cursor::new(bytes), 32).unwrap();
            assert_eq!(viewer.lines, ["Bonjour 世界", "Suite"]);
        }
        let mut bytes = vec![b'a'; READ_LIMIT - 1];
        bytes.extend("界".as_bytes());
        let viewer = from_reader("cut.txt", Cursor::new(bytes), READ_LIMIT as u64 + 2).unwrap();
        assert!(viewer.truncated && !viewer.binary);
        assert!(!viewer.lines[0].contains('\u{fffd}'));
    }

    #[test]
    fn line_metadata_expansion_and_late_binary_bytes_stay_bounded() {
        let viewer = from_reader("newlines.txt", std::io::repeat(b'\n'), u64::MAX).unwrap();
        assert!(viewer.truncated);
        assert_eq!(viewer.lines.len(), LINE_LIMIT);
        let viewer = from_reader("tabs.txt", std::io::repeat(b'\t'), u64::MAX).unwrap();
        assert!(viewer.truncated);
        assert!(viewer.lines.iter().map(String::len).sum::<usize>() <= DISPLAY_LIMIT);
        let mut bytes = vec![b'a'; 9000];
        bytes.push(0);
        let viewer = from_reader("tail.bin", Cursor::new(bytes), 9001).unwrap();
        assert!(viewer.binary);
    }

    #[test]
    fn document_layout_preserves_unicode_and_reflows_only_when_width_changes() {
        let text = "Bonjour 世界 et une longue phrase à lire intégralement.";
        let viewer = from_text("document.docx", text, false);
        viewer.prepare_layout(12);
        let pointer = {
            let first = viewer.layout.borrow();
            let first = first.as_ref().unwrap();
            assert_eq!(first.lines.concat(), text);
            assert!(first
                .lines
                .iter()
                .all(|line| super::super::gitstatus::display_width(line) <= 12));
            first.lines.as_ptr()
        };
        viewer.prepare_layout(12);
        assert_eq!(
            viewer.layout.borrow().as_ref().unwrap().lines.as_ptr(),
            pointer
        );
        viewer.prepare_layout(40);
        assert_eq!(
            viewer.layout.borrow().as_ref().unwrap().lines.concat(),
            text
        );
    }

    #[test]
    fn a_narrow_document_cannot_create_unbounded_row_metadata() {
        let viewer = from_text("large.pdf", &"x".repeat(DISPLAY_LIMIT), false);
        viewer.prepare_layout(2);
        let layout = viewer.layout.borrow();
        let layout = layout.as_ref().unwrap();
        assert!(layout.truncated);
        assert_eq!(layout.lines.len(), LINE_LIMIT);
        assert!(layout.lines.iter().map(String::len).sum::<usize>() <= DISPLAY_LIMIT);
    }
}
