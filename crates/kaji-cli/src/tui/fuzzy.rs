//! Fuzzy matching for the file finder (`Ctrl+P` / `/files`, task 8).
//!
//! Pure and dependency-free — no fuzzy crate enters the lock for this. The
//! candidates handed in are the pre-lowercased paths [`MentionIndex`] already
//! stores, so a keystroke over a 20k-entry project allocates nothing but the
//! result vector.
//!
//! Two matching modes, fzf's convention: a plain term matches as a
//! subsequence, a `'`-prefixed term matches as an exact substring (the escape
//! hatch for "I know the name, stop guessing"). Whitespace separates terms,
//! which are ANDed.
//!
//! [`MentionIndex`]: crate::tui::mentions::MentionIndex

/// Per matched character.
const MATCH: u32 = 10;
/// The character directly follows the previous match.
const CONSECUTIVE: u32 = 8;
/// The character opens a path segment (start of the string, or right after a
/// `/`) — what makes `tui` prefer `src/tui/mod.rs` over `src/statui.rs`.
const SEGMENT_START: u32 = 15;
/// The character sits in the last segment: matching a file name outranks
/// matching the directories leading to it.
const FILE_NAME: u32 = 6;
/// A long path pays for its length, gently — the score-desc/length-asc sort in
/// [`rank`] is what breaks real ties.
const LENGTH_DIVISOR: u32 = 4;

/// Score of `query` against an already-lowercased candidate path. `None` when
/// any term fails to match — the candidate is then out of the result list, not
/// merely ranked low. An empty query matches everything with score 0.
pub fn score(query: &str, candidate_lower: &str) -> Option<u32> {
    let lowered = query.to_lowercase();
    let terms: Vec<&str> = lowered.split_whitespace().collect();
    score_terms(&terms, candidate_lower)
}

/// Ranks already-lowercased candidates, best first: score desc, then shortest
/// path, then input order — total and deterministic, so a redraw never
/// reshuffles equally-good matches. The empty query returns every candidate in
/// alphabetical order. Capping is the caller's call.
pub fn rank<'a, I>(query: &str, candidates: I) -> Vec<(usize, u32)>
where
    I: IntoIterator<Item = &'a str>,
{
    let lowered = query.to_lowercase();
    let terms: Vec<&str> = lowered.split_whitespace().collect();
    let mut matched: Vec<(usize, u32, &str)> = candidates
        .into_iter()
        .enumerate()
        .filter_map(|(idx, candidate)| {
            score_terms(&terms, candidate).map(|score| (idx, score, candidate))
        })
        .collect();
    if terms.is_empty() {
        matched.sort_by(|a, b| a.2.cmp(b.2));
    } else {
        matched.sort_by(|a, b| {
            b.1.cmp(&a.1)
                .then_with(|| a.2.len().cmp(&b.2.len()))
                .then_with(|| a.0.cmp(&b.0))
        });
    }
    matched
        .into_iter()
        .map(|(idx, score, _)| (idx, score))
        .collect()
}

fn score_terms(terms: &[&str], candidate: &str) -> Option<u32> {
    let name_start = name_start(candidate);
    let mut total = 0u32;
    for term in terms {
        total += term_score(term, candidate, name_start)?;
    }
    Some(total.saturating_sub(candidate.len() as u32 / LENGTH_DIVISOR))
}

/// Byte offset of the last path segment. Directories carry a trailing `/` in
/// the index, which is part of their name, not a separator before an empty one.
fn name_start(candidate: &str) -> usize {
    let trimmed = candidate.strip_suffix('/').unwrap_or(candidate);
    trimmed.rfind('/').map(|i| i + 1).unwrap_or(0)
}

/// A file-name match is tried first and kept when it lands: `app` on
/// `src/mod/app.rs` must not be spent on the `a` of a directory.
fn term_score(term: &str, candidate: &str, name_start: usize) -> Option<u32> {
    match term.strip_prefix('\'') {
        Some(exact) => exact_score(exact, candidate, name_start),
        None => subsequence_score(term, candidate, name_start, name_start)
            .or_else(|| subsequence_score(term, candidate, 0, name_start)),
    }
}

#[allow(clippy::string_slice)] // `name_start` follows an ASCII `/` or is 0.
fn exact_score(needle: &str, candidate: &str, name_start: usize) -> Option<u32> {
    if needle.is_empty() {
        return Some(0);
    }
    let at = candidate[name_start..]
        .find(needle)
        .map(|pos| pos + name_start)
        .or_else(|| candidate.find(needle))?;
    let total = needle
        .char_indices()
        .map(|(offset, _)| char_score(candidate, at + offset, offset > 0, name_start))
        .sum();
    Some(total)
}

#[allow(clippy::string_slice)] // `from` is 0 or `name_start`, itself past an ASCII `/`.
fn subsequence_score(term: &str, candidate: &str, from: usize, name_start: usize) -> Option<u32> {
    let mut wanted = term.chars().peekable();
    let mut total = 0u32;
    let mut previous_end = usize::MAX;
    for (offset, c) in candidate[from..].char_indices() {
        let Some(&next) = wanted.peek() else {
            break;
        };
        if c != next {
            continue;
        }
        wanted.next();
        let at = from + offset;
        total += char_score(candidate, at, previous_end == at, name_start);
        previous_end = at + c.len_utf8();
    }
    wanted.next().is_none().then_some(total)
}

#[allow(clippy::string_slice)] // `at` comes from `char_indices` on `candidate`.
fn char_score(candidate: &str, at: usize, consecutive: bool, name_start: usize) -> u32 {
    let mut score = MATCH;
    if consecutive {
        score += CONSECUTIVE;
    }
    if at == 0 || candidate[..at].ends_with('/') {
        score += SEGMENT_START;
    }
    if at >= name_start {
        score += FILE_NAME;
    }
    score
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ranked<'a>(query: &str, candidates: &[&'a str]) -> Vec<&'a str> {
        rank(query, candidates.iter().copied())
            .into_iter()
            .map(|(idx, _)| candidates[idx])
            .collect()
    }

    #[test]
    fn subsequence_matches_are_case_insensitive() {
        assert!(score("APP", "crates/kaji-cli/src/tui/app.rs").is_some());
        assert!(score("aps", "crates/app.rs").is_some());
        assert!(score("zzz", "crates/app.rs").is_none());
    }

    #[test]
    fn a_file_name_match_outranks_a_directory_match() {
        let name = score("app", "src/mod/app.rs").expect("file name match");
        let dir = score("app", "src/app/mod.rs").expect("directory match");
        assert!(name > dir, "name {name} vs dir {dir}");
    }

    #[test]
    fn a_segment_start_outranks_a_match_inside_a_segment() {
        let start = score("tui", "src/tui.rs").expect("segment start");
        let inside = score("tui", "src/atui.rs").expect("inside a segment");
        assert!(start > inside, "start {start} vs inside {inside}");
    }

    #[test]
    fn consecutive_characters_outrank_scattered_ones() {
        let together = score("ab", "zzab.rs").expect("consecutive");
        let scattered = score("ab", "zazb.rs").expect("scattered");
        assert!(together > scattered, "{together} vs {scattered}");
    }

    #[test]
    fn a_shorter_path_outranks_a_longer_one_at_equal_shape() {
        let short = score("app", "app.rs").expect("short");
        let long = score("app", "crates/kaji-cli/src/tui/app.rs").expect("long");
        assert!(short > long, "{short} vs {long}");
    }

    #[test]
    fn quote_prefix_restricts_to_an_exact_substring() {
        assert!(score("'app.rs", "src/app.rs").is_some());
        assert!(
            score("'app.rs", "src/apxp.rs").is_none(),
            "exact mode must not fall back to a subsequence"
        );
        assert!(
            score("app.rs", "src/apxp.rs").is_some(),
            "fuzzy still fuzzy"
        );
    }

    #[test]
    fn whitespace_separated_terms_are_anded() {
        assert!(score("src rs", "src/tui/app.rs").is_some());
        assert!(score("src zzz", "src/tui/app.rs").is_none());
        assert!(
            score("tui 'app.rs", "src/tui/app.rs").is_some(),
            "modes mix inside one query"
        );
    }

    #[test]
    fn an_empty_query_returns_every_candidate_alphabetically() {
        let candidates = ["src/zeta.rs", "README.md", "src/alpha.rs"];
        assert_eq!(
            ranked("   ", &candidates),
            vec!["README.md", "src/alpha.rs", "src/zeta.rs"]
        );
    }

    #[test]
    fn rank_orders_by_score_then_length_then_input_order() {
        let candidates = [
            "vendor/lib/apparel/index.rs",
            "src/app.rs",
            "app.rs",
            "src/apx.rs",
        ];
        assert_eq!(ranked("app", &candidates)[0], "app.rs");
        assert_eq!(ranked("app", &candidates)[1], "src/app.rs");
        assert!(
            !ranked("app", &candidates).contains(&"src/apx.rs"),
            "apx has no second p"
        );
    }

    #[test]
    fn equal_candidates_keep_their_input_order() {
        let candidates = ["ab", "ac"];
        assert_eq!(
            rank("a", candidates.iter().copied()),
            rank("a", candidates.iter().copied()),
            "ranking is deterministic"
        );
        assert_eq!(ranked("a", &candidates), vec!["ab", "ac"]);
    }

    #[test]
    fn a_trailing_slash_does_not_hide_a_directory_name() {
        let dir = score("tui", "src/tui/").expect("directory match");
        let deep = score("tui", "src/tui/mod.rs").expect("file under it");
        assert!(dir > deep, "dir {dir} vs deep {deep}");
    }

    #[test]
    fn non_ascii_paths_never_panic() {
        assert!(score("ésu", "docs/été/résumé.md").is_some());
        assert!(score("'été", "docs/été/résumé.md").is_some());
        assert!(score("zz", "docs/été/résumé.md").is_none());
    }
}
