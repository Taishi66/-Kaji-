//! Call-level permission grants: `tool_name(spec)` rules, prefix derivation and matching.

use std::fmt;

use rmcp::model::JsonObject;

const SHELL_TOOLS: [&str; 2] = ["shell", "developer__shell"];
const PATH_TOOLS: [&str; 4] = ["write", "developer__write", "edit", "developer__edit"];

const PREFIX_GLOB: &str = " *";
const ESCAPED_STAR: &str = "\\*";

/// Operators that chain several commands into one shell invocation.
const STAGE_SEPARATORS: [char; 5] = ['&', '|', ';', '\n', '\r'];

/// Operators that make a stage non-widenable: a rule may only allow it verbatim.
const SUBSTITUTION_OPERATORS: [&str; 4] = ["$(", "`", ">", "<"];

/// What a grant constrains its tool's primary argument to.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Spec {
    /// The argument must be this text, character for character.
    Exact(String),
    /// The argument must be this text, or start with it followed by a space.
    Prefix(String),
}

impl Spec {
    pub fn exact(text: &str) -> Self {
        Self::Exact(text.to_string())
    }

    pub fn prefix(text: &str) -> Self {
        Self::Prefix(text.to_string())
    }

    /// Reads a serialized spec. A trailing ` *` is the prefix glob; `\*` is a literal
    /// star, so an exact command such as `rm -rf *` never re-reads as a prefix.
    pub fn parse(text: &str) -> Self {
        let text = text.trim();
        if let Some(head) = text.strip_suffix(ESCAPED_STAR) {
            Self::Exact(format!("{head}*"))
        } else if let Some(prefix) = text.strip_suffix(PREFIX_GLOB) {
            Self::Prefix(prefix.to_string())
        } else {
            Self::Exact(text.to_string())
        }
    }

    pub fn matches(&self, argument: &str) -> bool {
        match self {
            Self::Exact(text) => argument == text,
            Self::Prefix(prefix) => {
                argument == prefix || argument.starts_with(&format!("{prefix} "))
            }
        }
    }

    pub fn covers(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Exact(text), Self::Exact(other_text)) => text == other_text,
            (Self::Exact(_), Self::Prefix(_)) => false,
            (Self::Prefix(_), Self::Exact(other_text)) => self.matches(other_text),
            (Self::Prefix(_), Self::Prefix(other_prefix)) => self.matches(other_prefix),
        }
    }

    fn is_exactly(&self, argument: &str) -> bool {
        matches!(self, Self::Exact(text) if text == argument)
    }
}

impl fmt::Display for Spec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Prefix(prefix) => write!(f, "{prefix}{PREFIX_GLOB}"),
            Self::Exact(text) => match escapable_star(text) {
                Some(head) => write!(f, "{head}{ESCAPED_STAR}"),
                None => write!(f, "{text}"),
            },
        }
    }
}

/// A trailing star that would be re-read as a glob or as an escape needs escaping.
fn escapable_star(text: &str) -> Option<&str> {
    let head = text.strip_suffix('*')?;
    matches!(head.chars().last(), Some(' ') | Some('\\')).then_some(head)
}

/// A single entry of a permission list: a bare tool name, or `tool_name(spec)`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GrantRule {
    pub tool_name: String,
    pub spec: Option<Spec>,
}

impl GrantRule {
    pub fn new(tool_name: &str, spec: Option<Spec>) -> Self {
        Self {
            tool_name: tool_name.to_string(),
            spec,
        }
    }

    /// An entry that does not parse as `tool_name(spec)` is a tool-wide rule, so a
    /// truncated entry such as `developer__shell(cargo` never widens anything.
    pub fn parse(entry: &str) -> Self {
        match entry
            .strip_suffix(')')
            .and_then(|body| body.split_once('('))
        {
            Some((tool_name, spec)) => Self::new(tool_name.trim(), Some(Spec::parse(spec))),
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
            .is_some_and(|other_spec| spec.covers(other_spec))
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

pub fn is_shell_tool(tool_name: &str) -> bool {
    SHELL_TOOLS.contains(&tool_name)
}

fn is_path_tool(tool_name: &str) -> bool {
    PATH_TOOLS.contains(&tool_name)
}

/// The argument a grant spec constrains, for tools that take one.
pub fn primary_argument(tool_name: &str, arguments: &JsonObject) -> Option<String> {
    let key = if is_shell_tool(tool_name) {
        "command"
    } else if is_path_tool(tool_name) {
        "path"
    } else {
        return None;
    };
    arguments
        .get(key)?
        .as_str()
        .map(|value| value.trim().to_string())
}

/// The spec to persist when the user grants a call, or `None` for a tool-wide grant.
pub fn derive_grant_spec(tool_name: &str, arguments: Option<&JsonObject>) -> Option<Spec> {
    let argument = primary_argument(tool_name, arguments?)?;
    if is_shell_tool(tool_name) {
        Some(derive_shell_grant(&argument))
    } else {
        Some(Spec::Exact(argument))
    }
}

/// Widens a shell command to its two-token prefix, unless the command carries an
/// operator or a flag that would let the prefix cover unrelated work.
pub fn derive_shell_grant(command: &str) -> Spec {
    let command = normalize_whitespace(command.trim());
    if command.contains(STAGE_SEPARATORS)
        || contains_substitution(&command)
        || split_shell_stages(&command).is_none()
    {
        return Spec::Exact(command);
    }
    let mut tokens = command.split(' ');
    let (Some(head), Some(second)) = (tokens.next(), tokens.next()) else {
        return Spec::Exact(command);
    };
    if second.starts_with('-') {
        return Spec::Exact(command);
    }
    Spec::Prefix(format!("{head} {second}"))
}

/// Collapses runs of unquoted spaces and tabs so that a derived grant and the command
/// it came from compare equal. Line breaks separate stages, so they are never
/// collapsed: doing so would splice a chained command into a single one.
fn normalize_whitespace(text: &str) -> String {
    let mut normalized = String::with_capacity(text.len());
    let mut in_single = false;
    let mut in_double = false;
    let mut pending_space = false;
    let mut chars = text.chars().peekable();

    while let Some(c) = chars.next() {
        if matches!(c, ' ' | '\t') && !in_single && !in_double {
            pending_space = !normalized.is_empty();
            continue;
        }
        if pending_space {
            normalized.push(' ');
            pending_space = false;
        }
        match c {
            '\\' if !in_single => {
                normalized.push(c);
                if let Some(escaped) = chars.next() {
                    normalized.push(escaped);
                }
            }
            '\'' if !in_double => {
                in_single = !in_single;
                normalized.push(c);
            }
            '"' if !in_single => {
                in_double = !in_double;
                normalized.push(c);
            }
            _ => normalized.push(c),
        }
    }

    normalized
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
            ';' | '\n' | '\r' if !in_single && !in_double => {
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

pub fn call_allowed_by(rule: &GrantRule, tool_name: &str, arguments: Option<&JsonObject>) -> bool {
    call_allowed_by_any(std::slice::from_ref(rule), tool_name, arguments)
}

/// A call is allowed when the whole argument is granted verbatim, or when every stage
/// of it matches at least one rule.
pub fn call_allowed_by_any(
    rules: &[GrantRule],
    tool_name: &str,
    arguments: Option<&JsonObject>,
) -> bool {
    let specs = match collect_specs(rules, tool_name) {
        SpecSet::ToolWide => return true,
        SpecSet::None => return false,
        SpecSet::Specs(specs) => specs,
    };
    let Some(call) = call_argument(tool_name, arguments) else {
        return false;
    };

    if specs.iter().any(|spec| spec.is_exactly(&call.argument)) {
        return true;
    }
    if call.stages.is_empty() {
        return false;
    }

    call.stages.iter().all(|stage| {
        let literal = call.unresolved_quoting || contains_substitution(stage);
        specs.iter().any(|spec| {
            if literal {
                spec.is_exactly(stage)
            } else {
                spec.matches(stage)
            }
        })
    })
}

/// A call is denied as soon as any stage of it matches any rule: a denial must not be
/// escaped by chaining the denied command with an unrelated one.
pub fn call_denied_by_any(
    rules: &[GrantRule],
    tool_name: &str,
    arguments: Option<&JsonObject>,
) -> bool {
    let specs = match collect_specs(rules, tool_name) {
        SpecSet::ToolWide => return true,
        SpecSet::None => return false,
        SpecSet::Specs(specs) => specs,
    };
    let Some(call) = call_argument(tool_name, arguments) else {
        return false;
    };

    specs.iter().any(|spec| {
        spec.matches(&call.argument) || call.stages.iter().any(|stage| spec.matches(stage))
    })
}

enum SpecSet<'a> {
    ToolWide,
    Specs(Vec<&'a Spec>),
    None,
}

fn collect_specs<'a>(rules: &'a [GrantRule], tool_name: &str) -> SpecSet<'a> {
    let mut specs = Vec::new();
    for rule in rules.iter().filter(|rule| rule.tool_name == tool_name) {
        match &rule.spec {
            None => return SpecSet::ToolWide,
            Some(spec) => specs.push(spec),
        }
    }
    if specs.is_empty() {
        SpecSet::None
    } else {
        SpecSet::Specs(specs)
    }
}

struct CallArgument {
    argument: String,
    stages: Vec<String>,
    unresolved_quoting: bool,
}

/// Stages are cut from the raw command, then normalized one by one: collapsing first
/// would erase the line breaks the split relies on.
fn call_argument(tool_name: &str, arguments: Option<&JsonObject>) -> Option<CallArgument> {
    let argument = primary_argument(tool_name, arguments?)?;
    if !is_shell_tool(tool_name) {
        return Some(CallArgument {
            stages: vec![argument.clone()],
            argument,
            unresolved_quoting: false,
        });
    }
    let stages = split_shell_stages(&argument);
    let argument = normalize_whitespace(&argument);
    match stages {
        Some(stages) => Some(CallArgument {
            argument,
            stages: stages
                .iter()
                .map(|stage| normalize_whitespace(stage))
                .collect(),
            unresolved_quoting: false,
        }),
        None => Some(CallArgument {
            stages: vec![argument.clone()],
            argument,
            unresolved_quoting: true,
        }),
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

    fn rule(spec: Spec) -> GrantRule {
        GrantRule::new(SHELL, Some(spec))
    }

    #[test_case("cargo test -p foo", "cargo test *"; "two_token_prefix")]
    #[test_case("cargo test", "cargo test *"; "exactly_two_tokens_still_widens")]
    #[test_case("ls -la", "ls -la"; "flag_second_token_stays_exact")]
    #[test_case("rg", "rg"; "single_token_stays_exact")]
    #[test_case("git add -A && git commit -m x", "git add -A && git commit -m x"; "chained_stays_exact")]
    #[test_case("echo $(whoami)", "echo $(whoami)"; "substitution_stays_exact")]
    #[test_case("cargo test > out.txt", "cargo test > out.txt"; "redirection_stays_exact")]
    #[test_case("  cargo   test -p foo  ", "cargo test *"; "whitespace_normalized")]
    #[test_case("rm -rf *", "rm -rf \\*"; "literal_star_is_escaped")]
    #[test_case("ls -la *", "ls -la \\*"; "literal_star_after_flag_is_escaped")]
    #[test_case("grep -r \"a  b\"", "grep -r \"a  b\""; "quoted_whitespace_is_preserved")]
    #[test_case("echo \"unbalanced", "echo \"unbalanced"; "unresolvable_quoting_stays_exact")]
    #[test_case("cargo test\nrm -rf /", "cargo test\nrm -rf /"; "newline_chain_stays_exact")]
    #[test_case("cargo test\r\nrm -rf /", "cargo test\r\nrm -rf /"; "carriage_return_chain_stays_exact")]
    fn derives_shell_grant(command: &str, serialized: &str) {
        assert_eq!(derive_shell_grant(command).to_string(), serialized);
    }

    #[test_case("cargo test -p foo"; "widened")]
    #[test_case("cargo test"; "bare_prefix")]
    #[test_case("ls -la"; "flagged")]
    #[test_case("rg"; "single_token")]
    #[test_case("git add -A && git commit -m x"; "chained")]
    #[test_case("echo $(whoami)"; "substitution")]
    #[test_case("cargo test > out.txt"; "redirection")]
    #[test_case("  cargo   test -p foo  "; "irregular_whitespace")]
    #[test_case("rm -rf *"; "literal_star")]
    #[test_case("grep -r \"a  b\""; "quoted_whitespace")]
    #[test_case("echo \"unbalanced"; "unresolved_quoting")]
    #[test_case("cargo test\nrm -rf /"; "newline_chain")]
    #[test_case("cargo test\r\nrm -rf /"; "carriage_return_chain")]
    fn a_derived_grant_allows_the_command_it_came_from(command: &str) {
        let derived = rule(derive_shell_grant(command));
        assert!(call_allowed_by(&derived, SHELL, Some(&shell(command))));
    }

    #[test]
    fn a_literal_star_grant_does_not_become_a_prefix() {
        let derived = derive_shell_grant("rm -rf *");
        assert_eq!(derived, Spec::exact("rm -rf *"));
        assert_eq!(GrantRule::parse("shell(rm -rf \\*)"), rule(derived.clone()));

        let derived = rule(derived);
        assert!(!call_allowed_by(&derived, SHELL, Some(&shell("rm -rf /"))));
        assert!(!call_allowed_by(
            &derived,
            SHELL,
            Some(&shell("rm -rf /etc"))
        ));
    }

    #[test]
    fn a_literal_star_grant_covers_nothing_it_did_not_grant() {
        let literal = rule(Spec::exact("rm -rf *"));
        assert!(!literal.covers(&rule(Spec::exact("rm -rf x"))));
        assert!(!literal.covers(&rule(Spec::prefix("rm -rf"))));
        assert!(literal.covers(&rule(Spec::exact("rm -rf *"))));
    }

    #[test]
    fn derives_spec_per_tool() {
        assert_eq!(
            derive_grant_spec(SHELL, Some(&shell("cargo test -p foo"))),
            Some(Spec::prefix("cargo test"))
        );
        assert_eq!(
            derive_grant_spec("write", Some(&object!({ "path": "src/main.rs" }))),
            Some(Spec::exact("src/main.rs"))
        );
        assert_eq!(
            derive_grant_spec("edit", Some(&object!({ "path": "src/lib.rs" }))),
            Some(Spec::exact("src/lib.rs"))
        );
        assert_eq!(
            derive_grant_spec("other__tool", Some(&object!({ "command": "cargo test" }))),
            None
        );
        assert_eq!(derive_grant_spec(SHELL, None), None);
    }

    #[test]
    fn only_the_developer_tools_have_a_primary_argument() {
        for tool_name in ["shell", "developer__shell"] {
            assert_eq!(
                derive_grant_spec(tool_name, Some(&shell("cargo test -p foo"))),
                Some(Spec::prefix("cargo test"))
            );
        }
        for tool_name in ["write", "developer__write", "edit", "developer__edit"] {
            assert_eq!(
                derive_grant_spec(tool_name, Some(&object!({ "path": "src/main.rs" }))),
                Some(Spec::exact("src/main.rs"))
            );
        }
        for tool_name in ["thirdparty__shell", "thirdparty__write", "subshell"] {
            assert_eq!(
                derive_grant_spec(tool_name, Some(&shell("cargo test -p foo"))),
                None
            );
        }
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

    #[test_case("cargo   test  -p   foo", "cargo test -p foo"; "collapses_runs")]
    #[test_case("  cargo test  ", "cargo test"; "trims_ends")]
    #[test_case("echo \"a   b\"", "echo \"a   b\""; "keeps_quoted_runs")]
    #[test_case("echo 'a   b'", "echo 'a   b'"; "keeps_single_quoted_runs")]
    fn normalizes_unquoted_whitespace(command: &str, expected: &str) {
        assert_eq!(normalize_whitespace(command), expected);
    }

    #[test_case(Spec::Prefix("cargo test".into()), "cargo test", true; "prefix_matches_bare_prefix")]
    #[test_case(Spec::Prefix("cargo test".into()), "cargo test --all", true; "prefix_matches_extension")]
    #[test_case(Spec::Prefix("cargo test".into()), "cargo testing", false; "prefix_is_token_bounded")]
    #[test_case(Spec::Prefix("cargo test".into()), "cargo build", false; "prefix_rejects_other_command")]
    #[test_case(Spec::Exact("ls -la".into()), "ls -la", true; "literal_matches")]
    #[test_case(Spec::Exact("ls -la".into()), "ls -la /tmp", false; "literal_rejects_extension")]
    #[test_case(Spec::Exact("rm -rf *".into()), "rm -rf /", false; "literal_star_is_not_a_glob")]
    fn matches_specs(spec: Spec, argument: &str, expected: bool) {
        assert_eq!(spec.matches(argument), expected);
    }

    #[test_case("cargo test --all", true; "widened_call")]
    #[test_case("cargo test && rm -rf /", false; "chained_call_needs_every_stage")]
    #[test_case("cargo test | rm -rf /", false; "piped_call_needs_every_stage")]
    #[test_case("cargo test $(rm -rf /)", false; "substitution_is_not_widened")]
    #[test_case("cargo test > /etc/passwd", false; "redirection_is_not_widened")]
    #[test_case("cargo test \"a && b", false; "unresolvable_quoting_is_not_widened")]
    #[test_case("echo 'cargo test'", false; "other_command_denied")]
    #[test_case("cargo test\nrm -rf /", false; "newline_chained_call_needs_every_stage")]
    #[test_case("cargo test\r\nrm -rf /", false; "carriage_return_chained_call_needs_every_stage")]
    #[test_case("cargo test  \n  rm -rf /", false; "padded_newline_chain_needs_every_stage")]
    #[test_case("cargo test\n", true; "trailing_newline_is_a_single_stage")]
    fn prefix_rule_allows_only_matching_calls(command: &str, expected: bool) {
        let granted = rule(Spec::prefix("cargo test"));
        assert_eq!(
            call_allowed_by(&granted, SHELL, Some(&shell(command))),
            expected
        );
    }

    #[test]
    fn every_stage_may_match_a_different_rule() {
        let rules = [rule(Spec::prefix("cargo test")), rule(Spec::prefix("head"))];
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
        let granted = GrantRule::new(SHELL, None);
        assert!(call_allowed_by(&granted, SHELL, Some(&shell("rm -rf /"))));
        assert!(call_allowed_by(&granted, SHELL, None));
        assert!(!call_allowed_by(&granted, "other__tool", None));
    }

    #[test]
    fn spec_rule_denies_a_tool_without_a_primary_argument() {
        let granted = GrantRule::new("other__tool", Some(Spec::exact("anything")));
        assert!(!call_allowed_by(
            &granted,
            "other__tool",
            Some(&object!({ "command": "anything" }))
        ));
    }

    #[test]
    fn an_empty_stage_set_allows_nothing() {
        let granted = [rule(Spec::prefix("cargo test"))];
        for command in [";;;", "&&", "|", " ", "\n\n"] {
            assert!(
                !call_allowed_by_any(&granted, SHELL, Some(&shell(command))),
                "{command:?} should not be allowed"
            );
        }
    }

    #[test]
    fn a_denial_catches_any_stage_of_a_chained_command() {
        let denied = [rule(Spec::prefix("rm -rf"))];
        for command in [
            "rm -rf /",
            "true && rm -rf /",
            "rm -rf / && true",
            "echo hi | rm -rf /tmp",
            "true\nrm -rf /",
            "true\r\nrm -rf /",
        ] {
            assert!(
                call_denied_by_any(&denied, SHELL, Some(&shell(command))),
                "{command} should be denied"
            );
        }
        assert!(!call_denied_by_any(
            &denied,
            SHELL,
            Some(&shell("cargo test"))
        ));
    }

    #[test]
    fn a_tool_wide_denial_catches_every_call() {
        let denied = [GrantRule::new(SHELL, None)];
        assert!(call_denied_by_any(
            &denied,
            SHELL,
            Some(&shell("cargo test"))
        ));
        assert!(!call_denied_by_any(&denied, "other__tool", None));
    }

    #[test_case("developer__shell", "developer__shell", None; "bare_tool_name")]
    #[test_case("developer__shell(cargo test *)", "developer__shell", Some(Spec::Prefix("cargo test".into())); "prefix_rule")]
    #[test_case("developer__shell(rm -rf \\*)", "developer__shell", Some(Spec::Exact("rm -rf *".into())); "escaped_literal_star")]
    #[test_case("developer__shell(echo (nested))", "developer__shell", Some(Spec::Exact("echo (nested)".into())); "nested_parentheses")]
    #[test_case("developer__shell(cargo", "developer__shell(cargo", None; "unterminated_entry_is_inert")]
    fn parses_entries(entry: &str, tool_name: &str, spec: Option<Spec>) {
        let parsed = GrantRule::parse(entry);
        assert_eq!(parsed, GrantRule::new(tool_name, spec));
        assert_eq!(parsed.to_string(), entry);
    }

    #[test_case("plain"; "plain")]
    #[test_case("rm -rf *"; "trailing_star")]
    #[test_case("rm -rf \\*"; "trailing_escaped_star")]
    #[test_case("glob*"; "star_without_space")]
    fn exact_specs_round_trip_through_their_serialized_form(text: &str) {
        let spec = Spec::exact(text);
        assert_eq!(Spec::parse(&spec.to_string()), spec);
    }

    #[test]
    fn prefix_specs_round_trip_through_their_serialized_form() {
        let spec = Spec::prefix("cargo test");
        assert_eq!(spec.to_string(), "cargo test *");
        assert_eq!(Spec::parse(&spec.to_string()), spec);
    }

    #[test_case(None, Some(Spec::Prefix("cargo test".into())), true; "tool_wide_covers_spec")]
    #[test_case(Some(Spec::Prefix("cargo test".into())), None, false; "spec_does_not_cover_tool_wide")]
    #[test_case(Some(Spec::Prefix("cargo test".into())), Some(Spec::Exact("cargo test".into())), true; "prefix_covers_bare_prefix")]
    #[test_case(Some(Spec::Prefix("cargo test".into())), Some(Spec::Exact("cargo test -p foo".into())), true; "prefix_covers_narrower_literal")]
    #[test_case(Some(Spec::Prefix("cargo test".into())), Some(Spec::Prefix("cargo test -p".into())), true; "prefix_covers_narrower_prefix")]
    #[test_case(Some(Spec::Prefix("cargo test -p".into())), Some(Spec::Prefix("cargo test".into())), false; "narrow_does_not_cover_wide")]
    #[test_case(Some(Spec::Exact("ls -la".into())), Some(Spec::Exact("ls -la".into())), true; "literal_covers_itself")]
    #[test_case(Some(Spec::Exact("ls -la".into())), Some(Spec::Exact("ls -la /tmp".into())), false; "literal_covers_nothing_else")]
    #[test_case(Some(Spec::Exact("cargo test".into())), Some(Spec::Prefix("cargo test".into())), false; "literal_does_not_cover_prefix")]
    fn covers_narrower_rules(spec: Option<Spec>, other: Option<Spec>, expected: bool) {
        let granted = GrantRule::new(SHELL, spec);
        let other = GrantRule::new(SHELL, other);
        assert_eq!(granted.covers(&other), expected);
    }

    #[test]
    fn rules_never_cover_another_tool() {
        let granted = GrantRule::new(SHELL, None);
        assert!(!granted.covers(&GrantRule::new("developer__write", None)));
    }
}
