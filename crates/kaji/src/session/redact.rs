use regex::{Captures, Regex};
use std::sync::OnceLock;

const MASK_PREFIX_LEN: usize = 4;
const MASK_SUFFIX_LEN: usize = 3;
const FULL_MASK: &str = "[REDACTED]";

/// Mask style applied by a redaction rule to its match.
enum MaskStyle {
    /// Keep the captured key/header prefix, mask only the value.
    KeepKey,
    /// Mask the value only (no key prefix in the match).
    ValueOnly,
    /// Replace the whole match (e.g. credentials embedded in a URL).
    Full,
}

struct Rule {
    regex: Regex,
    style: MaskStyle,
}

static RULES: OnceLock<Vec<Rule>> = OnceLock::new();

/// Secret redaction rules for bug-report bundles. Each rule captures the secret
/// value in the `v` group; `KeepKey` rules also capture a `k` prefix (key name
/// or header) that is preserved so the redacted output stays readable.
fn rules() -> &'static Vec<Rule> {
    RULES.get_or_init(|| {
        vec![
            // config.yaml / JSON value forms: `api_key: sk-...`, `"token": "..."`.
            Rule {
                regex: Regex::new(
                    r#"(?i)(?P<k>\b(?:api[_-]?key|apikey|access[_-]?token|refresh[_-]?token|client[_-]?secret|private[_-]?key|auth[_-]?token|token|secret|password|passwd)\b["']?\s*[:=]\s*["']?)(?P<v>[^"'\r\n,;}\s]+)"#,
                )
                .expect("static rule 1"),
                style: MaskStyle::KeepKey,
            },
            // Environment-style key dumps: `OPENAI_API_KEY=sk-...`.
            Rule {
                regex: Regex::new(
                    r#"(?i)(?P<k>\b[A-Z][A-Z0-9_]*(?:_API_KEY|_TOKEN|_SECRET|_PASSWORD|_CLIENT_SECRET)\b\s*[:=]\s*["']?)(?P<v>[^"'\r\n,;}\s]+)"#,
                )
                .expect("static rule 2"),
                style: MaskStyle::KeepKey,
            },
            // Auth headers: `Authorization: Bearer <token>`.
            Rule {
                regex: Regex::new(
                    r#"(?i)(?P<k>\b(?:authorization|proxy-authorization|x-api-key|api-key)\b\s*:\s*(?:bearer\s+)?)(?P<v>[A-Za-z0-9._~+/=-]+)"#,
                )
                .expect("static rule 3"),
                style: MaskStyle::KeepKey,
            },
            // Known provider token shapes (OpenAI/Anthropic, GitHub, AWS, JWT)
            // that appear without an adjacent key.
            Rule {
                regex: Regex::new(
                    r#"(?P<v>\b(?:sk-[A-Za-z0-9_\-]{12,}|ghp_[A-Za-z0-9]{30,}|gho_[A-Za-z0-9]{30,}|AKIA[0-9A-Z]{16}|eyJ[A-Za-z0-9_\-]{20,}\.[A-Za-z0-9._\-]{20,}))"#,
                )
                .expect("static rule 4"),
                style: MaskStyle::ValueOnly,
            },
            // Credentials embedded in URLs: `https://user:pass@host`.
            Rule {
                regex: Regex::new(r#"(?P<v>https?://[^/\s:@]+:[^/\s@]+@)"#).expect("static rule 5"),
                style: MaskStyle::Full,
            },
        ]
    })
}

/// Mask a secret value, keeping a short prefix/suffix for debuggability
/// (matching the convention of the ACP provider field view).
fn masked(value: &str) -> String {
    let prefix: String = value.chars().take(MASK_PREFIX_LEN).collect();
    let suffix: String = value
        .chars()
        .rev()
        .take(MASK_SUFFIX_LEN)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    if prefix.is_empty()
        || suffix.is_empty()
        || value.chars().count() <= MASK_PREFIX_LEN + MASK_SUFFIX_LEN
    {
        return FULL_MASK.to_string();
    }
    format!("{prefix}...{suffix}")
}

fn apply_rule(rule: &Rule, input: &str) -> (String, usize) {
    let mut count = 0usize;
    let output = rule.regex.replace_all(input, |caps: &Captures| {
        count += 1;
        let value = caps.name("v").map(|m| m.as_str()).unwrap_or_default();
        match rule.style {
            MaskStyle::KeepKey => {
                let key = caps.name("k").map(|m| m.as_str()).unwrap_or_default();
                format!("{key}{}", masked(value))
            }
            MaskStyle::ValueOnly => masked(value),
            MaskStyle::Full => FULL_MASK.to_string(),
        }
    });
    (output.into_owned(), count)
}

/// Redact secrets from arbitrary text (config YAML, logs, prompts, session
/// exports). Returns the redacted text and the number of values masked.
pub fn redact_text(input: &str) -> (String, usize) {
    let mut output = input.to_string();
    let mut total = 0usize;
    for rule in rules() {
        let (next, count) = apply_rule(rule, &output);
        output = next;
        total += count;
    }
    (output, total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_keys_tokens_and_credentials() {
        let input = concat!(
            "OPENAI_API_KEY=sk-proj-abcdefghijklmnop\n",
            "api_key: sk-123456789012345678\n",
            "Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.signature\n",
            "https://user:pass@example.com/endpoint\n",
            "ghp_abcdefghijklmnopqrstuvwxyz1234567890\n",
        );
        let (out, count) = redact_text(input);
        assert!(count >= 5, "expected at least 5 redactions, got {count}");
        assert!(!out.contains("sk-proj-abcdefghijklmnop"));
        assert!(!out.contains("sk-123456789012345678"));
        assert!(!out.contains("eyJhbGciOiJIUzI1NiJ9"));
        assert!(!out.contains("user:pass@"));
        assert!(!out.contains("ghp_abcdefghijklmnopqrstuvwxyz1234567890"));
        assert!(out.contains("OPENAI_API_KEY="));
        assert!(out.contains("Authorization: Bearer "));
        assert!(out.contains("api_key: "));
    }

    #[test]
    fn leaves_benign_text_alone() {
        let input = "model: gpt-4o-mini\ninput_tokens: 1500\nthis is a normal error message\nconfig_path: /tmp/kaji\n";
        let (out, count) = redact_text(input);
        assert_eq!(count, 0, "expected no redactions, got: {out}");
    }
}
