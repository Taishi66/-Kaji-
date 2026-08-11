use crate::config::Config;
use crate::conversation::message::{Message, MessageContentBlock};
use rmcp::model::{ContentBlock, Role};
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::LazyLock;

pub const DEFAULT_KEEP_RAW_TURNS: usize = 2;
pub const DEFAULT_MAX_LINES: usize = 40;
const OMISSION_MARKER: &str = "lignes omises — résultat complet conservé en session";

pub struct CondenseBudget {
    pub max_lines: usize,
}

impl Default for CondenseBudget {
    fn default() -> Self {
        CondenseBudget {
            max_lines: DEFAULT_MAX_LINES,
        }
    }
}

#[derive(Default, Debug)]
pub struct CondenseStats {
    pub results_touched: usize,
    pub lines_before: usize,
    pub lines_after: usize,
    pub bytes_before: usize,
    pub bytes_after: usize,
    pub per_tool: BTreeMap<String, (usize, usize)>,
}

pub fn enabled() -> bool {
    !std::env::var("KAJI_CONDENSE")
        .map(|v| matches!(v.as_str(), "0" | "false" | "FALSE" | "no"))
        .unwrap_or(false)
}

pub fn keep_raw_turns() -> usize {
    match Config::global().get_param::<usize>("KAJI_CONDENSE_KEEP_TURNS") {
        Ok(v) if v > 0 => v,
        _ => DEFAULT_KEEP_RAW_TURNS,
    }
}

pub fn max_lines() -> usize {
    match Config::global().get_param::<usize>("KAJI_CONDENSE_MAX_LINES") {
        Ok(v) if v > 0 => v,
        _ => DEFAULT_MAX_LINES,
    }
}

/// A turn boundary is a user message carrying prompt text and no tool
/// results — the point where a new user instruction begins. Walking these
/// backwards from the end lets us find "the last N turns" without needing
/// an explicit turn counter on `Message`.
fn is_turn_boundary(message: &Message) -> bool {
    message.role == Role::User
        && message
            .content
            .iter()
            .any(|c| matches!(c, MessageContentBlock::Text(_)))
        && !message
            .content
            .iter()
            .any(|c| matches!(c, MessageContentBlock::ToolResponse(_)))
}

/// Index of the `keep_raw_turns`-th turn boundary counted from the end, i.e.
/// the cutoff before which tool-results are eligible for condensing. Fewer
/// boundaries than `keep_raw_turns` means the whole history is still fresh.
fn cutoff_index(messages: &[Message], keep_raw_turns: usize) -> usize {
    let mut boundaries_seen = 0;
    for (idx, message) in messages.iter().enumerate().rev() {
        if is_turn_boundary(message) {
            boundaries_seen += 1;
            if boundaries_seen == keep_raw_turns {
                return idx;
            }
        }
    }
    0
}

fn build_tool_names(messages: &[Message]) -> HashMap<String, String> {
    let mut names = HashMap::new();
    for message in messages {
        for content in &message.content {
            let MessageContentBlock::ToolRequest(request) = content else {
                continue;
            };
            let Ok(tool_call) = &request.tool_call else {
                continue;
            };
            names.insert(request.id.clone(), tool_call.name.to_string());
        }
    }
    names
}

fn strip_ansi(text: &str) -> std::borrow::Cow<'_, str> {
    static ANSI_ESCAPE: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]").expect("valid regex"));
    ANSI_ESCAPE.replace_all(text, "")
}

/// Collapse consecutive identical non-empty lines into one occurrence with a
/// `×N` suffix. Blank lines are left alone since repeated blank lines are
/// usually meaningful spacing, not noise.
fn dedup_consecutive(lines: &[&str]) -> Vec<String> {
    let mut out = Vec::with_capacity(lines.len());
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if line.is_empty() {
            out.push(line.to_string());
            i += 1;
            continue;
        }
        let mut count = 1;
        while i + count < lines.len() && lines[i + count] == line {
            count += 1;
        }
        out.push(if count > 1 {
            format!("{line} ×{count}")
        } else {
            line.to_string()
        });
        i += count;
    }
    out
}

/// Condense a tool-result body: strip ANSI, rtrim, dedup repeated lines, then
/// enforce a head/tail line budget. Returns `None` when the result is
/// identical to the input, so callers can tell an unchanged result from a
/// touched one without a separate diff — this is what makes re-condensing an
/// already-condensed result a no-op instead of re-truncating the marker.
pub fn condense_text(text: &str, max_lines: usize) -> Option<String> {
    let stripped = strip_ansi(text);
    let rtrimmed: Vec<&str> = stripped.lines().map(|line| line.trim_end()).collect();
    let deduped = dedup_consecutive(&rtrimmed);

    // head(>=1) + marker(1) + tail(>=1) needs at least 3 lines to represent a
    // truncation at all; below that, the two `.max(1)` floors would make the
    // truncated output longer than max_lines and a second pass would
    // re-truncate it, breaking idempotence. Clamp the working budget instead
    // of the raw parameter so tiny budgets still degrade to "head+marker+tail"
    // rather than panicking or oscillating.
    let max_lines = max_lines.max(3);

    let result = if deduped.len() > max_lines {
        let head = (max_lines * 60 / 100).max(1).min(deduped.len());
        let tail = (max_lines * 25 / 100).max(1).min(deduped.len() - head);
        let omitted = deduped.len() - head - tail;

        let mut lines: Vec<String> = deduped[..head].to_vec();
        lines.push(format!("[… {omitted} {OMISSION_MARKER}]"));
        lines.extend_from_slice(&deduped[deduped.len() - tail..]);
        lines.join("\n")
    } else {
        deduped.join("\n")
    };

    if result == text {
        None
    } else {
        Some(result)
    }
}

pub fn condense_history(
    messages: &[Message],
    keep_raw_turns: usize,
    budget: &CondenseBudget,
) -> (Vec<Message>, CondenseStats) {
    let cutoff = cutoff_index(messages, keep_raw_turns);
    let mut stats = CondenseStats::default();
    if cutoff == 0 {
        return (messages.to_vec(), stats);
    }

    let tool_names = build_tool_names(messages);
    let mut out = messages.to_vec();

    for message in &mut out[..cutoff] {
        for content in &mut message.content {
            let MessageContentBlock::ToolResponse(response) = content else {
                continue;
            };
            let Ok(result) = &mut response.tool_result else {
                continue;
            };
            let tool_name = tool_names
                .get(&response.id)
                .map(String::as_str)
                .unwrap_or("?");

            for block in &mut result.content {
                let ContentBlock::Text(text_content) = block else {
                    continue;
                };
                let Some(condensed) = condense_text(&text_content.text, budget.max_lines) else {
                    continue;
                };

                let bytes_before = text_content.text.len();
                let lines_before = text_content.text.lines().count();
                let bytes_after = condensed.len();
                let lines_after = condensed.lines().count();

                stats.results_touched += 1;
                stats.bytes_before += bytes_before;
                stats.bytes_after += bytes_after;
                stats.lines_before += lines_before;
                stats.lines_after += lines_after;
                let entry = stats
                    .per_tool
                    .entry(tool_name.to_string())
                    .or_insert((0, 0));
                entry.0 += bytes_before;
                entry.1 += bytes_after;

                text_content.text = condensed;
            }
        }
    }

    (out, stats)
}

static TOTAL_RESULTS_TOUCHED: AtomicU64 = AtomicU64::new(0);
static TOTAL_BYTES_BEFORE: AtomicU64 = AtomicU64::new(0);
static TOTAL_BYTES_AFTER: AtomicU64 = AtomicU64::new(0);

pub struct CondenseTotals {
    pub results_touched: u64,
    pub bytes_before: u64,
    pub bytes_after: u64,
}

pub fn record_totals(stats: &CondenseStats) {
    TOTAL_RESULTS_TOUCHED.fetch_add(stats.results_touched as u64, Ordering::Relaxed);
    TOTAL_BYTES_BEFORE.fetch_add(stats.bytes_before as u64, Ordering::Relaxed);
    TOTAL_BYTES_AFTER.fetch_add(stats.bytes_after as u64, Ordering::Relaxed);
}

pub fn totals() -> CondenseTotals {
    CondenseTotals {
        results_touched: TOTAL_RESULTS_TOUCHED.load(Ordering::Relaxed),
        bytes_before: TOTAL_BYTES_BEFORE.load(Ordering::Relaxed),
        bytes_after: TOTAL_BYTES_AFTER.load(Ordering::Relaxed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation::message::{Message, MessageContentBlock, ToolResponse};
    use rmcp::model::{CallToolRequestParams, CallToolResult, ContentBlock};

    fn user_text(t: &str) -> Message {
        Message::user().with_text(t)
    }
    fn tool_response(id: &str, text: &str) -> Message {
        let result = CallToolResult::success(vec![ContentBlock::text(text)]);
        Message::user().with_content(MessageContentBlock::ToolResponse(ToolResponse {
            id: id.into(),
            tool_result: Ok(result),
            metadata: None,
        }))
    }
    fn numbered(n: usize) -> String {
        (1..=n)
            .map(|i| format!("line_{i}"))
            .collect::<Vec<_>>()
            .join("\n")
    }
    fn tool_text(m: &Message) -> String {
        m.content
            .iter()
            .find_map(MessageContentBlock::as_tool_response)
            .and_then(|r| r.tool_result.as_ref().ok())
            .and_then(|result| result.content.first())
            .and_then(ContentBlock::as_text)
            .map(|t| t.text.to_string())
            .unwrap_or_default()
    }

    #[test]
    fn old_tool_result_is_condensed_recent_stays_raw() {
        let msgs = vec![
            user_text("q1"),
            tool_response("a", &numbered(200)), // vieux tour
            user_text("q2"),
            tool_response("b", &numbered(200)), // fenêtre (2e user-texte depuis la fin = q2)
            user_text("q3"),                    // tour courant
        ];
        let (out, stats) = condense_history(&msgs, 2, &CondenseBudget::default());
        let old = tool_text(&out[1]);
        assert!(old.contains("lignes omises"));
        assert!(old.contains("line_1\n") && old.contains("line_200"));
        assert!(!old.contains("line_100\n")); // milieu omis
        assert_eq!(tool_text(&out[3]), numbered(200)); // dans la fenêtre → brut
        assert_eq!(stats.results_touched, 1);
        assert!(stats.bytes_after < stats.bytes_before);
    }

    #[test]
    fn fewer_turns_than_window_changes_nothing() {
        let msgs = vec![user_text("q1"), tool_response("a", &numbered(200))];
        let (out, stats) = condense_history(&msgs, 2, &CondenseBudget::default());
        assert_eq!(tool_text(&out[1]), numbered(200));
        assert_eq!(stats.results_touched, 0);
    }

    #[test]
    fn dedup_repeated_lines_and_strip_ansi() {
        let raw = format!("\x1b[31mrouge\x1b[0m\n{}", ["same"; 5].join("\n"));
        let msgs = vec![
            user_text("q1"),
            tool_response("a", &raw),
            user_text("q2"),
            user_text("q3"),
        ];
        let (out, _) = condense_history(&msgs, 2, &CondenseBudget::default());
        let t = tool_text(&out[1]);
        assert!(t.contains("rouge") && !t.contains('\x1b'));
        assert!(t.contains("same ×5") && t.matches("same").count() == 1);
    }

    #[test]
    fn condense_is_idempotent_and_stats_do_not_double_count() {
        let msgs = vec![
            user_text("q1"),
            tool_response("a", &numbered(200)),
            user_text("q2"),
            user_text("q3"),
        ];
        let (once, s1) = condense_history(&msgs, 2, &CondenseBudget::default());
        let (twice, s2) = condense_history(&once, 2, &CondenseBudget::default());
        assert_eq!(tool_text(&once[1]), tool_text(&twice[1]));
        assert_eq!(s1.results_touched, 1);
        assert_eq!(s2.results_touched, 0);
    }

    #[test]
    fn single_line_json_and_non_text_and_current_turn_are_untouched() {
        let json_line = r#"{"a":1,"b":2}"#;
        let msgs = vec![
            user_text("q1"),
            tool_response("a", json_line),
            Message::user().with_content(MessageContentBlock::image("data", "image/png")),
            user_text("q2"),
            tool_response("b", &numbered(200)),
            user_text("q3"),
        ];
        let (out, stats) = condense_history(&msgs, 2, &CondenseBudget::default());
        assert_eq!(tool_text(&out[1]), json_line);
        assert!(matches!(out[2].content[0], MessageContentBlock::Image(_)));
        assert_eq!(tool_text(&out[4]), numbered(200)); // dans la fenêtre → brut
        assert_eq!(stats.results_touched, 0);
    }

    #[test]
    fn tiny_budget_does_not_panic() {
        let b = CondenseBudget { max_lines: 1 };
        let msgs = vec![
            user_text("q1"),
            tool_response("a", &numbered(50)),
            user_text("q2"),
            user_text("q3"),
        ];
        let (out, _) = condense_history(&msgs, 2, &b);
        assert!(tool_text(&out[1]).lines().count() <= 3); // head 1 + marqueur + tail 1
    }

    #[test]
    fn per_tool_stats_use_matching_request_name() {
        let msgs = vec![
            user_text("q1"),
            Message::assistant().with_tool_request("a", Ok(CallToolRequestParams::new("shell"))),
            tool_response("a", &numbered(200)),
            tool_response("orphan", &numbered(200)),
            user_text("q2"),
            user_text("q3"),
        ];
        let (_out, stats) = condense_history(&msgs, 2, &CondenseBudget::default());
        assert!(stats.per_tool.contains_key("shell"));
        assert!(stats.per_tool.contains_key("?"));
    }

    #[test]
    fn err_tool_result_is_untouched() {
        let msgs = vec![
            user_text("q1"),
            Message::user().with_content(MessageContentBlock::ToolResponse(ToolResponse {
                id: "a".into(),
                tool_result: Err(rmcp::model::ErrorData {
                    code: rmcp::model::ErrorCode::INTERNAL_ERROR,
                    message: std::borrow::Cow::from("boom"),
                    data: None,
                }),
                metadata: None,
            })),
            user_text("q2"),
            user_text("q3"),
        ];
        let (out, stats) = condense_history(&msgs, 2, &CondenseBudget::default());
        let response = out[1]
            .content
            .iter()
            .find_map(MessageContentBlock::as_tool_response)
            .unwrap();
        assert!(response.tool_result.is_err());
        assert_eq!(stats.results_touched, 0);
    }

    #[test]
    fn message_with_multiple_tool_response_blocks_condenses_each() {
        let make_result = |text: &str| CallToolResult::success(vec![ContentBlock::text(text)]);
        let msg = Message::user()
            .with_content(MessageContentBlock::ToolResponse(ToolResponse {
                id: "a".into(),
                tool_result: Ok(make_result(&numbered(200))),
                metadata: None,
            }))
            .with_content(MessageContentBlock::ToolResponse(ToolResponse {
                id: "b".into(),
                tool_result: Ok(make_result(&numbered(200))),
                metadata: None,
            }));
        let msgs = vec![user_text("q1"), msg, user_text("q2"), user_text("q3")];
        let (out, stats) = condense_history(&msgs, 2, &CondenseBudget::default());
        assert_eq!(stats.results_touched, 2);
        for content in &out[1].content {
            let MessageContentBlock::ToolResponse(response) = content else {
                panic!("expected ToolResponse");
            };
            let result = response.tool_result.as_ref().unwrap();
            let text = result.content[0].as_text().unwrap().text.as_str();
            assert!(text.contains("lignes omises"));
        }
    }

    #[test]
    fn condense_text_is_idempotent_for_tiny_budgets() {
        // Reported repro: max_lines=1 on 10 lines used to re-truncate a
        // second time (marker "8 lignes omises" then "1 lignes omises").
        let once = condense_text(&numbered(10), 1).expect("first pass should condense");
        let twice = condense_text(&once, 1);
        assert_eq!(twice, None, "second pass must be a no-op: {once:?}");

        for max_lines in [1usize, 2, 3, 40] {
            let source = numbered(200);
            let once = condense_text(&source, max_lines).expect("first pass should condense");
            let twice = condense_text(&once, max_lines).unwrap_or_else(|| once.clone());
            assert_eq!(
                twice, once,
                "max_lines={max_lines}: re-condensing an already-condensed result must not change it"
            );
        }
    }

    #[test]
    fn nested_non_text_content_block_in_tool_result_is_untouched() {
        let mixed_result = CallToolResult::success(vec![
            ContentBlock::text(numbered(200)),
            ContentBlock::image("base64data", "image/png"),
        ]);
        let msgs = vec![
            user_text("q1"),
            Message::user().with_content(MessageContentBlock::ToolResponse(ToolResponse {
                id: "a".into(),
                tool_result: Ok(mixed_result),
                metadata: None,
            })),
            user_text("q2"),
            user_text("q3"),
        ];
        let (out, stats) = condense_history(&msgs, 2, &CondenseBudget::default());
        let response = out[1]
            .content
            .iter()
            .find_map(MessageContentBlock::as_tool_response)
            .unwrap();
        let result = response.tool_result.as_ref().unwrap();
        assert!(result.content[0]
            .as_text()
            .unwrap()
            .text
            .contains("lignes omises"));
        assert!(matches!(result.content[1], ContentBlock::Image(_)));
        assert_eq!(stats.results_touched, 1);
    }

    #[test]
    fn empty_messages_do_not_panic() {
        let (out, stats) = condense_history(&[], 2, &CondenseBudget::default());
        assert!(out.is_empty());
        assert_eq!(stats.results_touched, 0);
    }

    #[test]
    fn totals_accumulate_across_calls() {
        let before = totals();
        let stats = CondenseStats {
            results_touched: 3,
            bytes_before: 100,
            bytes_after: 40,
            ..Default::default()
        };
        record_totals(&stats);
        let after = totals();
        assert_eq!(after.results_touched - before.results_touched, 3);
        assert_eq!(after.bytes_before - before.bytes_before, 100);
        assert_eq!(after.bytes_after - before.bytes_after, 40);
    }
}
