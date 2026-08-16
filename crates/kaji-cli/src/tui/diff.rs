//! Minimal LCS line diff for the approval detail panel. No diff crate is a
//! declared dependency of this workspace, and the panel only ever compares two
//! short in-memory snippets, so the whole need is ~60 lines of DP.

/// Past this many DP cells the fine-grained diff is dropped for a block
/// delete/add — still a truthful rendering of the change, and it keeps a
/// pathological pair of large files from stalling the draw.
const MAX_MATRIX_CELLS: usize = 250_000;

/// Renders `before` → `after` as `-`/`+`/` `-prefixed lines, unchanged lines
/// kept as context.
pub fn line_diff(before: &str, after: &str) -> Vec<String> {
    let before: Vec<&str> = before.lines().collect();
    let after: Vec<&str> = after.lines().collect();

    let head = common_prefix(&before, &after);
    let tail = common_suffix(&before[head..], &after[head..]);
    let (before_mid, after_mid) = (
        &before[head..before.len() - tail],
        &after[head..after.len() - tail],
    );

    let mut out: Vec<String> = before[..head].iter().map(|line| context(line)).collect();
    if before_mid.len().saturating_mul(after_mid.len()) > MAX_MATRIX_CELLS {
        out.extend(before_mid.iter().map(|line| format!("-{line}")));
        out.extend(after_mid.iter().map(|line| format!("+{line}")));
    } else {
        out.extend(lcs_diff(before_mid, after_mid));
    }
    out.extend(
        before[before.len() - tail..]
            .iter()
            .map(|line| context(line)),
    );
    out
}

fn context(line: &str) -> String {
    format!(" {line}")
}

fn common_prefix(before: &[&str], after: &[&str]) -> usize {
    before
        .iter()
        .zip(after)
        .take_while(|(left, right)| left == right)
        .count()
}

fn common_suffix(before: &[&str], after: &[&str]) -> usize {
    before
        .iter()
        .rev()
        .zip(after.iter().rev())
        .take_while(|(left, right)| left == right)
        .count()
}

fn lcs_diff(before: &[&str], after: &[&str]) -> Vec<String> {
    let (rows, cols) = (before.len(), after.len());
    let width = cols + 1;
    let mut lcs = vec![0usize; (rows + 1) * width];
    for i in (0..rows).rev() {
        for j in (0..cols).rev() {
            lcs[i * width + j] = if before[i] == after[j] {
                lcs[(i + 1) * width + j + 1] + 1
            } else {
                lcs[(i + 1) * width + j].max(lcs[i * width + j + 1])
            };
        }
    }

    let mut out = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < rows && j < cols {
        if before[i] == after[j] {
            out.push(context(before[i]));
            i += 1;
            j += 1;
        } else if lcs[(i + 1) * width + j] >= lcs[i * width + j + 1] {
            out.push(format!("-{}", before[i]));
            i += 1;
        } else {
            out.push(format!("+{}", after[j]));
            j += 1;
        }
    }
    out.extend(before[i..].iter().map(|line| format!("-{line}")));
    out.extend(after[j..].iter().map(|line| format!("+{line}")));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_text_is_all_context() {
        assert_eq!(line_diff("a\nb", "a\nb"), [" a", " b"]);
    }

    #[test]
    fn a_replaced_line_shows_both_sides_in_place() {
        assert_eq!(
            line_diff("a\nb\nc", "a\nB\nc"),
            [" a", "-b", "+B", " c"],
            "context must frame the change, not be re-emitted around it"
        );
    }

    #[test]
    fn additions_and_deletions_are_signed() {
        assert_eq!(line_diff("a\nc", "a\nb\nc"), [" a", "+b", " c"]);
        assert_eq!(line_diff("a\nb\nc", "a\nc"), [" a", "-b", " c"]);
    }

    #[test]
    fn an_empty_side_is_a_pure_insert_or_delete() {
        assert_eq!(line_diff("", "a\nb"), ["+a", "+b"]);
        assert_eq!(line_diff("a\nb", ""), ["-a", "-b"]);
        assert!(line_diff("", "").is_empty());
    }

    #[test]
    fn a_move_is_reported_as_a_delete_and_an_insert() {
        assert_eq!(line_diff("a\nb", "b\na"), ["-a", " b", "+a"]);
    }

    /// The DP matrix is quadratic: a big pair must degrade to a block
    /// delete/add instead of allocating gigabytes mid-draw.
    #[test]
    fn oversized_input_degrades_to_a_block_diff_without_stalling() {
        let before = (0..1000)
            .map(|i| format!("old {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let after = (0..1000)
            .map(|i| format!("new {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let diff = line_diff(&before, &after);
        assert_eq!(diff.len(), 2000);
        assert!(diff[0].starts_with('-'));
        assert!(diff[1999].starts_with('+'));
    }
}
