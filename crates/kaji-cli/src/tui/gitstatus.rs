//! Lualine-style status bar contents (task 15) — the bottom line's working
//! directory, branch and repository state.
//!
//! Two `git` calls, both off the event loop: `status --porcelain=v2 --branch`
//! for the branch, the upstream gap and the file counts, `diff HEAD
//! --shortstat` for the line counts. Parsing and rendering are pure so the
//! whole bar is unit-testable without a terminal.

use crate::tui::theme;
use crate::tui::ui::sanitize_for_display;
use ratatui::style::Style;
use ratatui::text::Span;
use std::path::Path;

const DIR_GLYPH: &str = "📁 ";
const SEPARATOR: &str = " · ";
const ELLIPSIS: &str = "…";
const CLEAN: &str = "✓";

/// Detached HEAD has no branch name to show, so the bar shows the commit it
/// sits on — abbreviated the way git itself abbreviates.
const DETACHED_SHA_CHARS: usize = 7;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GitStatus {
    /// Branch name, or the full commit oid when `detached`.
    pub branch: String,
    pub detached: bool,
    pub ahead: usize,
    pub behind: usize,
    pub upstream: bool,
    pub staged: usize,
    pub modified: usize,
    pub untracked: usize,
    pub insertions: usize,
    pub deletions: usize,
}

/// `None` when `text` is not a `--porcelain=v2 --branch` block (no
/// `# branch.head` header) — which is what a failed or unrelated `git` call
/// produces.
///
/// A single file counts in both `staged` and `modified` when its `XY` says so:
/// the two columns answer different questions (index vs. working tree) and the
/// bar reports both, exactly as `git status` does.
pub fn parse_porcelain_v2(text: &str) -> Option<GitStatus> {
    let mut status = GitStatus::default();
    let mut head: Option<String> = None;
    let mut oid = String::new();

    for line in text.lines() {
        if let Some(header) = line.strip_prefix("# ") {
            let Some((key, value)) = header.split_once(' ') else {
                continue;
            };
            match key {
                "branch.oid" => oid = value.to_string(),
                "branch.head" => head = Some(value.to_string()),
                "branch.upstream" => status.upstream = true,
                "branch.ab" => {
                    let (ahead, behind) = parse_ab(value);
                    status.ahead = ahead;
                    status.behind = behind;
                }
                _ => {}
            }
            continue;
        }
        let Some((kind, rest)) = line.split_once(' ') else {
            continue;
        };
        match kind {
            "1" | "2" => {
                let mut xy = rest.chars();
                if xy.next().is_some_and(|x| x != '.') {
                    status.staged += 1;
                }
                if xy.next().is_some_and(|y| y != '.') {
                    status.modified += 1;
                }
            }
            "u" => status.modified += 1,
            "?" => status.untracked += 1,
            _ => {}
        }
    }

    let head = head?;
    if head == "(detached)" {
        status.detached = true;
        status.branch = oid;
    } else {
        status.branch = head;
    }
    Some(status)
}

/// `# branch.ab +A -B` — absent when the branch has no upstream, which leaves
/// both counts at 0.
fn parse_ab(value: &str) -> (usize, usize) {
    let mut ab = (0usize, 0usize);
    for field in value.split_whitespace() {
        if let Some(count) = field.strip_prefix('+').and_then(|n| n.parse().ok()) {
            ab.0 = count;
        } else if let Some(count) = field.strip_prefix('-').and_then(|n| n.parse().ok()) {
            ab.1 = count;
        }
    }
    ab
}

/// `(insertions, deletions)` out of a `--shortstat` line; a missing side is 0,
/// and so is an empty line (a clean tree prints nothing).
pub fn parse_shortstat(text: &str) -> (usize, usize) {
    let mut counts = (0usize, 0usize);
    for field in text.split(',') {
        let mut words = field.split_whitespace();
        let Some(Ok(count)) = words.next().map(str::parse::<usize>) else {
            continue;
        };
        match words.next() {
            Some(word) if word.starts_with("insertion") => counts.0 = count,
            Some(word) if word.starts_with("deletion") => counts.1 = count,
            _ => {}
        }
    }
    counts
}

/// `None` outside a repository, or when git is missing or fails.
///
/// Blocking by design — `event_loop` runs it on a blocking task. The second
/// call is allowed to fail on its own: a repository without a commit yet has no
/// `HEAD` to diff against, and the file counts are still worth showing.
pub fn read(dir: &Path) -> Option<GitStatus> {
    let porcelain = git_output(dir, &["status", "--porcelain=v2", "--branch"])?;
    let mut status = parse_porcelain_v2(&porcelain)?;
    if let Some(shortstat) = git_output(dir, &["diff", "HEAD", "--shortstat"]) {
        let (insertions, deletions) = parse_shortstat(&shortstat);
        status.insertions = insertions;
        status.deletions = deletions;
    }
    Some(status)
}

fn git_output(dir: &Path, args: &[&str]) -> Option<String> {
    let output = kaji::subprocess::git_command()
        .current_dir(dir)
        .args(args)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The bar's left side, fitted into `width` cells: `📁 {dir}` then one group of
/// segments per repository fact, joined by `SEPARATOR`. `status` is `None`
/// outside a repository, which leaves the directory alone on the bar.
pub fn render(status: Option<&GitStatus>, dir: &Path, width: usize) -> Vec<Span<'static>> {
    let tail = join_groups(status.map(repo_groups).unwrap_or_default());
    let tail_width: usize = tail.iter().map(Span::width).sum();
    let budget = width.saturating_sub(tail_width + display_width(DIR_GLYPH));

    let mut spans = vec![
        Span::styled(DIR_GLYPH, theme::dim()),
        Span::styled(truncate_left(&dir_label(dir), budget), theme::dim()),
    ];
    spans.extend(tail);
    spans
}

fn join_groups(groups: Vec<Vec<Span<'static>>>) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for group in groups {
        spans.push(Span::styled(SEPARATOR, theme::dim()));
        spans.extend(group);
    }
    spans
}

fn repo_groups(status: &GitStatus) -> Vec<Vec<Span<'static>>> {
    let mut groups = vec![vec![Span::styled(branch_label(status), theme::title())]];

    let upstream = counters(&[
        (status.ahead, "↑", theme::text()),
        (status.behind, "↓", theme::text()),
    ]);
    if !upstream.is_empty() {
        groups.push(upstream);
    }

    let files = counters(&[
        (status.staged, "●", theme::accent()),
        (status.modified, "✚", theme::text()),
        (status.untracked, "…", theme::dim()),
    ]);
    groups.push(if files.is_empty() {
        vec![Span::styled(CLEAN, theme::dim())]
    } else {
        files
    });

    let diff = counters(&[
        (status.insertions, "+", theme::text()),
        (status.deletions, "−", theme::accent()),
    ]);
    if !diff.is_empty() {
        groups.push(diff);
    }

    groups
}

/// One space-joined group of `{glyph}{count}` segments, zeros dropped — an
/// empty group is what tells [`repo_groups`] the whole group has nothing to say.
fn counters(items: &[(usize, &str, Style)]) -> Vec<Span<'static>> {
    let mut group: Vec<Span<'static>> = Vec::new();
    for (count, glyph, style) in items {
        if *count == 0 {
            continue;
        }
        if !group.is_empty() {
            group.push(Span::raw(" "));
        }
        group.push(Span::styled(format!("{glyph}{count}"), *style));
    }
    group
}

fn branch_label(status: &GitStatus) -> String {
    let branch = sanitize_for_display(&status.branch);
    if status.detached {
        format!(
            "@{}",
            branch.chars().take(DETACHED_SHA_CHARS).collect::<String>()
        )
    } else {
        branch
    }
}

fn dir_label(dir: &Path) -> String {
    dir_label_with(dir, std::env::var_os("HOME"))
}

/// Env-free core so tests never touch the process-global `HOME` — the same
/// contract as [`crate::tui::mentions::expand_home`].
fn dir_label_with(dir: &Path, home: Option<std::ffi::OsString>) -> String {
    if let Some(home) = home {
        if let Ok(rest) = dir.strip_prefix(Path::new(&home)) {
            if rest.as_os_str().is_empty() {
                return "~".to_string();
            }
            return format!("~/{}", rest.display());
        }
    }
    dir.display().to_string()
}

/// Keeps the tail of `label` — the leaf directory is what identifies it — and
/// marks what it dropped with a leading `…`.
fn truncate_left(label: &str, budget: usize) -> String {
    if display_width(label) <= budget {
        return label.to_string();
    }
    let mut used = display_width(ELLIPSIS);
    if used > budget {
        return String::new();
    }
    let mut kept = 0;
    let mut buffer = [0u8; 4];
    for c in label.chars().rev() {
        let cell = display_width(c.encode_utf8(&mut buffer));
        if used + cell > budget {
            break;
        }
        used += cell;
        kept += 1;
    }
    let tail: String = label.chars().skip(label.chars().count() - kept).collect();
    format!("{ELLIPSIS}{tail}")
}

/// Cells, not chars: ratatui measures every span with unicode-width, so a
/// `chars().count()` budget would let `📁` or a CJK path overflow the bar.
fn display_width(text: &str) -> usize {
    Span::raw(text).width()
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    const HEADERS: &str = "# branch.oid 3f96ad57532e560881c039e54985ef5a585c78b2\n\
                           # branch.head feat/kaji-init\n";

    fn parse_with(entries: &str) -> GitStatus {
        parse_porcelain_v2(&format!("{HEADERS}{entries}\n")).expect("porcelain v2 block")
    }

    #[test_case("1 M. N... 100644 100644 100644 aaa bbb a.txt", (1, 0, 0); "staged_only")]
    #[test_case("1 .M N... 100644 100644 100644 aaa bbb a.txt", (0, 1, 0); "modified_only")]
    #[test_case("1 MM N... 100644 100644 100644 aaa bbb a.txt", (1, 1, 0); "same_file_staged_and_modified")]
    #[test_case("2 R. N... 100644 100644 100644 aaa bbb R100 new.txt\told.txt", (1, 0, 0); "rename_is_staged")]
    #[test_case("u UU N... 100644 100644 100644 100644 aaa bbb ccc a.txt", (0, 1, 0); "unmerged_is_modified")]
    #[test_case("? new.txt", (0, 0, 1); "untracked")]
    #[test_case("! ignored.txt", (0, 0, 0); "ignored_is_not_counted")]
    fn porcelain_v2_counts_entries(entry: &str, expected: (usize, usize, usize)) {
        let status = parse_with(entry);
        assert_eq!(
            (status.staged, status.modified, status.untracked),
            expected,
            "{entry}"
        );
    }

    #[test]
    fn porcelain_v2_reads_the_branch_and_the_upstream_gap() {
        let status = parse_porcelain_v2(
            "# branch.oid 3f96ad57532e560881c039e54985ef5a585c78b2\n\
             # branch.head feat/kaji-init\n\
             # branch.upstream kaji-origin/feat/kaji-init\n\
             # branch.ab +3 -2\n",
        )
        .expect("porcelain v2 block");

        assert_eq!(status.branch, "feat/kaji-init");
        assert!(!status.detached);
        assert!(status.upstream);
        assert_eq!((status.ahead, status.behind), (3, 2));
    }

    #[test]
    fn a_branch_without_upstream_has_no_ahead_behind() {
        let status = parse_with("");

        assert!(!status.upstream);
        assert_eq!((status.ahead, status.behind), (0, 0));
    }

    #[test]
    fn a_detached_head_carries_the_commit_instead_of_a_branch() {
        let status = parse_porcelain_v2(
            "# branch.oid 3f96ad57532e560881c039e54985ef5a585c78b2\n# branch.head (detached)\n",
        )
        .expect("porcelain v2 block");

        assert!(status.detached);
        assert_eq!(status.branch, "3f96ad57532e560881c039e54985ef5a585c78b2");
    }

    #[test]
    fn text_that_is_not_a_porcelain_block_is_rejected() {
        assert_eq!(parse_porcelain_v2(""), None);
        assert_eq!(parse_porcelain_v2("fatal: not a git repository\n"), None);
    }

    #[test_case(" 3 files changed, 2 insertions(+), 1 deletion(-)", (2, 1); "both_sides")]
    #[test_case(" 1 file changed, 4 insertions(+)", (4, 0); "insertions_only")]
    #[test_case(" 1 file changed, 7 deletions(-)", (0, 7); "deletions_only")]
    #[test_case("", (0, 0); "clean_tree_prints_nothing")]
    fn shortstat_reads_both_counts(text: &str, expected: (usize, usize)) {
        assert_eq!(parse_shortstat(text), expected, "{text}");
    }

    fn dirty() -> GitStatus {
        GitStatus {
            branch: "main".to_string(),
            ahead: 1,
            behind: 2,
            upstream: true,
            staged: 3,
            modified: 4,
            untracked: 5,
            insertions: 12,
            deletions: 6,
            ..GitStatus::default()
        }
    }

    fn rendered(status: Option<&GitStatus>, dir: &str, width: usize) -> String {
        render(status, Path::new(dir), width)
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn every_segment_shows_up_on_a_dirty_repository() {
        let _theme = theme::test_guard();

        let line = rendered(Some(&dirty()), "/tmp/project", 120);

        for expected in [
            "📁 /tmp/project",
            "main",
            "↑1",
            "↓2",
            "●3",
            "✚4",
            "…5",
            "+12",
            "−6",
        ] {
            assert!(line.contains(expected), "{expected} manquant dans {line:?}");
        }
    }

    #[test]
    fn zero_counters_are_dropped_and_a_clean_tree_says_so() {
        let _theme = theme::test_guard();
        let status = GitStatus {
            branch: "main".to_string(),
            upstream: true,
            ..GitStatus::default()
        };

        let line = rendered(Some(&status), "/tmp/project", 120);

        assert_eq!(line, "📁 /tmp/project · main · ✓");
    }

    #[test]
    fn a_detached_head_renders_as_an_abbreviated_commit() {
        let _theme = theme::test_guard();
        let status = GitStatus {
            branch: "3f96ad57532e560881c039e54985ef5a585c78b2".to_string(),
            detached: true,
            ..GitStatus::default()
        };

        let line = rendered(Some(&status), "/tmp/project", 120);

        assert!(line.contains("@3f96ad5"), "{line:?}");
        assert!(!line.contains("3f96ad57532"), "{line:?}");
    }

    #[test]
    fn the_branch_is_sanitized_before_it_reaches_the_bar() {
        let _theme = theme::test_guard();
        let status = GitStatus {
            branch: "ma\u{1b}[31mlicious".to_string(),
            ..GitStatus::default()
        };

        let line = rendered(Some(&status), "/tmp/project", 120);

        assert!(!line.contains('\u{1b}'), "{line:?}");
    }

    #[test]
    fn outside_a_repository_only_the_directory_shows() {
        let _theme = theme::test_guard();

        let line = rendered(None, "/tmp/project", 120);

        assert_eq!(line, "📁 /tmp/project");
    }

    #[test]
    fn the_home_prefix_is_abbreviated() {
        assert_eq!(
            dir_label_with(
                Path::new("/Users/moi/workspace/kaji"),
                Some("/Users/moi".into())
            ),
            "~/workspace/kaji"
        );
        assert_eq!(
            dir_label_with(Path::new("/Users/moi"), Some("/Users/moi".into())),
            "~"
        );
        assert_eq!(
            dir_label_with(Path::new("/opt/kaji"), Some("/Users/moi".into())),
            "/opt/kaji"
        );
        assert_eq!(dir_label_with(Path::new("/opt/kaji"), None), "/opt/kaji");
    }

    /// The bar owns exactly one line: the directory gives up its head — the
    /// part a `…` can stand for — so the repository state stays readable.
    #[test]
    fn a_narrow_bar_truncates_the_directory_from_the_left() {
        let _theme = theme::test_guard();
        let status = GitStatus {
            branch: "main".to_string(),
            modified: 2,
            ..GitStatus::default()
        };

        let spans = render(
            Some(&status),
            Path::new("/Users/moi/workspace/kaji/crates/kaji-cli"),
            30,
        );
        let width: usize = spans.iter().map(Span::width).sum();
        let line: String = spans.iter().map(|span| span.content.as_ref()).collect();

        assert!(width <= 30, "{width} cellules pour {line:?}");
        assert!(
            line.contains("kaji-cli"),
            "la queue du chemin reste: {line:?}"
        );
        assert!(line.contains("main"), "l'état du dépôt reste: {line:?}");
        assert!(
            line.contains(&format!("{DIR_GLYPH}{ELLIPSIS}")),
            "troncature marquée: {line:?}"
        );
    }

    #[test]
    fn a_bar_too_narrow_for_the_directory_keeps_the_repository_state() {
        let _theme = theme::test_guard();

        let spans = render(Some(&dirty()), Path::new("/Users/moi/workspace/kaji"), 4);
        let line: String = spans.iter().map(|span| span.content.as_ref()).collect();

        assert!(line.contains("main"), "{line:?}");
    }

    fn run_git(dir: &Path, args: &[&str]) {
        let output = kaji::subprocess::git_command()
            .current_dir(dir)
            .args(args)
            .output()
            .expect("git available for tests");
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn read_reports_the_branch_and_the_working_tree_state_of_a_temp_repo() {
        let dir = tempfile::tempdir().unwrap();
        run_git(dir.path(), &["init", "-q"]);
        run_git(dir.path(), &["config", "user.email", "test@example.com"]);
        run_git(dir.path(), &["config", "user.name", "test"]);
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        run_git(dir.path(), &["add", "a.txt"]);
        run_git(dir.path(), &["commit", "-q", "-m", "init"]);

        std::fs::write(dir.path().join("a.txt"), "two\n").unwrap();
        std::fs::write(dir.path().join("s.txt"), "staged\n").unwrap();
        run_git(dir.path(), &["add", "s.txt"]);
        std::fs::write(dir.path().join("u.txt"), "untracked\n").unwrap();

        let status = read(dir.path()).expect("should detect the git repo");

        assert!(!status.branch.is_empty(), "{status:?}");
        assert!(!status.detached, "{status:?}");
        assert!(!status.upstream, "{status:?}");
        assert_eq!(
            (status.staged, status.modified, status.untracked),
            (1, 1, 1),
            "{status:?}"
        );
        assert!(status.insertions > 0, "{status:?}");
        assert!(status.deletions > 0, "{status:?}");
    }

    #[test]
    fn read_returns_none_outside_a_repo() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read(dir.path()), None);
    }
}
