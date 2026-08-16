//! Call-level permission grants: `tool_name(spec)` rules, prefix derivation and matching.

use std::fmt;

use rmcp::model::JsonObject;

const SHELL_TOOL: &str = "shell";
const WRITE_TOOL: &str = "write";
const EDIT_TOOL: &str = "edit";

const PREFIX_GLOB: &str = " *";

/// Operators that chain several commands into one shell invocation.
const STAGE_SEPARATORS: [char; 4] = ['&', '|', ';', '\n'];

/// Operators that make a stage non-widenable: a rule may only allow it verbatim.
const SUBSTITUTION_OPERATORS: [&str; 4] = ["$(", "`", ">", "<"];

/// A single entry of a permission list: a bare tool name, or `tool_name(spec)`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GrantRule {
    pub tool_name: String,
    pub spec: Option<String>,
}

impl GrantRule {
    pub fn new(tool_name: &str, spec: Option<&str>) -> Self {
        Self {
            tool_name: tool_name.to_string(),
            spec: spec.map(str::to_string),
        }
    }

    /// An entry that does not parse as `tool_name(spec)` is a tool-wide rule, so a
    /// truncated entry such as `developer__shell(cargo` never widens anything.
    pub fn parse(entry: &str) -> Self {
        match entry
            .strip_suffix(')')
            .and_then(|body| body.split_once('('))
        {
            Some((tool_name, spec)) => Self::new(tool_name.trim(), Some(spec.trim())),
            None => Self::new(entry, None),
        }
    }

    pub fn covers(&self, other: &Self) -> bool {
        if self.tool_name != other.tool_name {
            return false;
        }
        let Some(spec) = &self.spec else {
            return true;
        };
        other
            .spec
            .as_ref()
            .is_some_and(|other_spec| rule_matches(spec, other_spec))
    }
}

impl fmt::Display for GrantRule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.spec {
            Some(spec) => write!(f, "{}({})", self.tool_name, spec),
            None => write!(f, "{}", self.tool_name),
        }
    }
}

/// Developer tools are advertised unprefixed, extension tools as `extension__tool`.
fn local_tool_name(tool_name: &str) -> &str {
    tool_name.rsplit("__").next().unwrap_or(tool_name)
}

pub fn is_shell_tool(tool_name: &str) -> bool {
    local_tool_name(tool_name) == SHELL_TOOL
}

/// The argument a grant spec constrains, for tools that take one.
pub fn primary_argument(tool_name: &str, arguments: &JsonObject) -> Option<String> {
    let key = match local_tool_name(tool_name) {
        SHELL_TOOL => "command",
        WRITE_TOOL | EDIT_TOOL => "path",
        _ => return None,
    };
    arguments
        .get(key)?
        .as_str()
        .map(|value| value.trim().to_string())
}

/// The spec to persist when the user grants a call, or `None` for a tool-wide grant.
pub fn derive_grant_spec(tool_name: &str, arguments: Option<&JsonObject>) -> Option<String> {
    let argument = primary_argument(tool_name, arguments?)?;
    if is_shell_tool(tool_name) {
        Some(derive_shell_grant(&argument))
    } else {
        Some(argument)
    }
}

/// Widens a shell command to its two-token prefix, unless the command carries an
/// operator or a flag that would let the prefix cover unrelated work.
pub fn derive_shell_grant(command: &str) -> String {
    let command = command.trim();
    if command.contains(STAGE_SEPARATORS) || contains_substitution(command) {
        return command.to_string();
    }
    let mut tokens = command.split_whitespace();
    let (Some(head), Some(second)) = (tokens.next(), tokens.next()) else {
        return command.to_string();
    };
    if second.starts_with('-') {
        return command.to_string();
    }
    format!("{head} {second}{PREFIX_GLOB}")
}

/// Splits a shell command on chaining operators, ignoring quoted ones.
/// `None` means the quoting could not be resolved and no widening may be applied.
pub fn split_shell_stages(command: &str) -> Option<Vec<String>> {
    let mut stages = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut chars = command.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '\\' if !in_single => {
                current.push(c);
                if let Some(escaped) = chars.next() {
                    current.push(escaped);
                }
            }
            '\'' if !in_double => {
                in_single = !in_single;
                current.push(c);
            }
            '"' if !in_single => {
                in_double = !in_double;
                current.push(c);
            }
            '&' | '|' if !in_single && !in_double => {
                if chars.peek() == Some(&c) {
                    chars.next();
                }
                stages.push(std::mem::take(&mut current));
            }
            ';' | '\n' if !in_single && !in_double => {
                stages.push(std::mem::take(&mut current));
            }
            _ => current.push(c),
        }
    }

    if in_single || in_double {
        return None;
    }
    stages.push(current);

    Some(
        stages
            .into_iter()
            .map(|stage| stage.trim().to_string())
            .filter(|stage| !stage.is_empty())
            .collect(),
    )
}

/// `spec` is either a literal, or a prefix terminated by ` *`.
pub fn rule_matches(spec: &str, argument: &str) -> bool {
    match spec.strip_suffix(PREFIX_GLOB) {
        Some(prefix) => argument == prefix || argument.starts_with(&format!("{prefix} ")),
        None => argument == spec,
    }
}

pub fn call_allowed_by(rule: &GrantRule, tool_name: &str, arguments: Option<&JsonObject>) -> bool {
    call_allowed_by_any(std::slice::from_ref(rule), tool_name, arguments)
}

/// A call is allowed when every stage of its primary argument matches at least one rule.
pub fn call_allowed_by_any(
    rules: &[GrantRule],
    tool_name: &str,
    arguments: Option<&JsonObject>,
) -> bool {
    let mut specs: Vec<&str> = Vec::new();
    for rule in rules.iter().filter(|rule| rule.tool_name == tool_name) {
        match &rule.spec {
            None => return true,
            Some(spec) => specs.push(spec),
        }
    }
    if specs.is_empty() {
        return false;
    }

    let Some(argument) = arguments.and_then(|arguments| primary_argument(tool_name, arguments))
    else {
        return false;
    };

    let (stages, literal_only) = match split_shell_stages_for(tool_name, &argument) {
        Some(stages) => (stages, false),
        None => (vec![argument.clone()], true),
    };

    stages.iter().all(|stage| {
        let literal = literal_only || contains_substitution(stage);
        specs.iter().any(|spec| {
            if literal {
                *spec == stage
            } else {
                rule_matches(spec, stage)
            }
        })
    })
}

fn split_shell_stages_for(tool_name: &str, argument: &str) -> Option<Vec<String>> {
    if is_shell_tool(tool_name) {
        split_shell_stages(argument)
    } else {
        Some(vec![argument.to_string()])
    }
}

fn contains_substitution(command: &str) -> bool {
    SUBSTITUTION_OPERATORS
        .iter()
        .any(|operator| command.contains(operator))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::object;
    use test_case::test_case;

    const SHELL: &str = "shell";

    fn shell(command: &str) -> JsonObject {
        object!({ "command": command })
    }

    #[test_case("cargo test -p foo", "cargo test *"; "two_token_prefix")]
    #[test_case("cargo test", "cargo test *"; "exactly_two_tokens_still_widens")]
    #[test_case("ls -la", "ls -la"; "flag_second_token_stays_exact")]
    #[test_case("rg", "rg"; "single_token_stays_exact")]
    #[test_case("git add -A && git commit -m x", "git add -A && git commit -m x"; "chained_stays_exact")]
    #[test_case("echo $(whoami)", "echo $(whoami)"; "substitution_stays_exact")]
    #[test_case("cargo test > out.txt", "cargo test > out.txt"; "redirection_stays_exact")]
    #[test_case("  cargo   test -p foo  ", "cargo test *"; "whitespace_normalized")]
    fn derives_shell_grant(command: &str, expected: &str) {
        assert_eq!(derive_shell_grant(command), expected);
    }

    #[test]
    fn derives_spec_per_tool() {
        assert_eq!(
            derive_grant_spec(SHELL, Some(&shell("cargo test -p foo"))),
            Some("cargo test *".to_string())
        );
        assert_eq!(
            derive_grant_spec(
                "developer__write",
                Some(&object!({ "path": "src/main.rs" }))
            ),
            Some("src/main.rs".to_string())
        );
        assert_eq!(
            derive_grant_spec("developer__edit", Some(&object!({ "path": "src/lib.rs" }))),
            Some("src/lib.rs".to_string())
        );
        assert_eq!(
            derive_grant_spec("other__tool", Some(&object!({ "command": "cargo test" }))),
            None
        );
        assert_eq!(derive_grant_spec(SHELL, None), None);
    }

    #[test]
    fn primary_arguments_survive_extension_prefixing() {
        assert_eq!(
            derive_grant_spec("developer__shell", Some(&shell("cargo test -p foo"))),
            Some("cargo test *".to_string())
        );
        assert_eq!(
            derive_grant_spec("write", Some(&object!({ "path": "src/main.rs" }))),
            Some("src/main.rs".to_string())
        );
        assert_eq!(
            derive_grant_spec(SHELL, Some(&object!({ "other": 1 }))),
            None
        );
    }

    #[test_case("cargo test", &["cargo test"]; "single_stage")]
    #[test_case("cargo test && rm -rf /", &["cargo test", "rm -rf /"]; "and_chain")]
    #[test_case("a | b || c ; d", &["a", "b", "c", "d"]; "every_separator")]
    #[test_case("echo \"a && b\"", &["echo \"a && b\""]; "double_quoted_operator")]
    #[test_case("echo 'a | b'", &["echo 'a | b'"]; "single_quoted_operator")]
    #[test_case("echo a\nrm -rf /", &["echo a", "rm -rf /"]; "newline_chain")]
    fn splits_shell_stages(command: &str, expected: &[&str]) {
        assert_eq!(
            split_shell_stages(command).expect("resolvable quoting"),
            expected
        );
    }

    #[test]
    fn unbalanced_quotes_are_unresolvable() {
        assert_eq!(split_shell_stages("echo \"a && b"), None);
    }

    #[test_case("cargo test *", "cargo test", true; "prefix_matches_bare_prefix")]
    #[test_case("cargo test *", "cargo test --all", true; "prefix_matches_extension")]
    #[test_case("cargo test *", "cargo testing", false; "prefix_is_token_bounded")]
    #[test_case("cargo test *", "cargo build", false; "prefix_rejects_other_command")]
    #[test_case("ls -la", "ls -la", true; "literal_matches")]
    #[test_case("ls -la", "ls -la /tmp", false; "literal_rejects_extension")]
    fn matches_rules(spec: &str, argument: &str, expected: bool) {
        assert_eq!(rule_matches(spec, argument), expected);
    }

    #[test_case("cargo test --all", true; "widened_call")]
    #[test_case("cargo test && rm -rf /", false; "chained_call_needs_every_stage")]
    #[test_case("cargo test | rm -rf /", false; "piped_call_needs_every_stage")]
    #[test_case("cargo test $(rm -rf /)", false; "substitution_is_not_widened")]
    #[test_case("cargo test > /etc/passwd", false; "redirection_is_not_widened")]
    #[test_case("cargo test \"a && b", false; "unresolvable_quoting_is_not_widened")]
    #[test_case("echo 'cargo test'", false; "other_command_denied")]
    fn prefix_rule_allows_only_matching_calls(command: &str, expected: bool) {
        let rule = GrantRule::new(SHELL, Some("cargo test *"));
        assert_eq!(
            call_allowed_by(&rule, SHELL, Some(&shell(command))),
            expected
        );
    }

    #[test]
    fn every_stage_may_match_a_different_rule() {
        let rules = [
            GrantRule::new(SHELL, Some("cargo test *")),
            GrantRule::new(SHELL, Some("head *")),
        ];
        assert!(call_allowed_by_any(
            &rules,
            SHELL,
            Some(&shell("cargo test --all | head -20"))
        ));
        assert!(!call_allowed_by_any(
            &rules,
            SHELL,
            Some(&shell("cargo test --all | tail -20"))
        ));
    }

    #[test]
    fn tool_wide_rule_allows_every_call() {
        let rule = GrantRule::new(SHELL, None);
        assert!(call_allowed_by(&rule, SHELL, Some(&shell("rm -rf /"))));
        assert!(call_allowed_by(&rule, SHELL, None));
        assert!(!call_allowed_by(&rule, "other__tool", None));
    }

    #[test]
    fn spec_rule_denies_a_tool_without_a_primary_argument() {
        let rule = GrantRule::new("other__tool", Some("anything"));
        assert!(!call_allowed_by(
            &rule,
            "other__tool",
            Some(&object!({ "command": "anything" }))
        ));
    }

    #[test_case("developer__shell", "developer__shell", None; "bare_tool_name")]
    #[test_case("developer__shell(cargo test *)", "developer__shell", Some("cargo test *"); "spec_rule")]
    #[test_case("developer__shell(echo (nested))", "developer__shell", Some("echo (nested)"); "nested_parentheses")]
    #[test_case("developer__shell(cargo", "developer__shell(cargo", None; "unterminated_entry_is_inert")]
    fn parses_entries(entry: &str, tool_name: &str, spec: Option<&str>) {
        let rule = GrantRule::parse(entry);
        assert_eq!(rule, GrantRule::new(tool_name, spec));
        assert_eq!(rule.to_string(), entry);
    }

    #[test_case(None, Some("cargo test *"), true; "tool_wide_covers_spec")]
    #[test_case(Some("cargo test *"), None, false; "spec_does_not_cover_tool_wide")]
    #[test_case(Some("cargo test *"), Some("cargo test"), true; "prefix_covers_bare_prefix")]
    #[test_case(Some("cargo test *"), Some("cargo test -p foo"), true; "prefix_covers_narrower_literal")]
    #[test_case(Some("cargo test *"), Some("cargo test -p *"), true; "prefix_covers_narrower_prefix")]
    #[test_case(Some("cargo test -p *"), Some("cargo test *"), false; "narrow_does_not_cover_wide")]
    #[test_case(Some("ls -la"), Some("ls -la"), true; "literal_covers_itself")]
    #[test_case(Some("ls -la"), Some("ls -la /tmp"), false; "literal_covers_nothing_else")]
    fn covers_narrower_rules(spec: Option<&str>, other: Option<&str>, expected: bool) {
        let rule = GrantRule::new(SHELL, spec);
        let other = GrantRule::new(SHELL, other);
        assert_eq!(rule.covers(&other), expected);
    }

    #[test]
    fn rules_never_cover_another_tool() {
        let rule = GrantRule::new(SHELL, None);
        assert!(!rule.covers(&GrantRule::new("developer__write", None)));
    }
}
