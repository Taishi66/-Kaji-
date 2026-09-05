//! Récupération d'une page sous garde SSRF.
//!
//! Les redirections ne sont pas déléguées à reqwest : chaque saut est repassé
//! par la garde, sinon un 302 vers `http://169.254.169.254/` suffirait à
//! contourner la vérification faite sur l'URL initiale.

use super::error::WebError;
use super::extract::html_to_markdown;
use super::guard::{self, CONNECT_TIMEOUT};
use reqwest::header::{ACCEPT, CONTENT_TYPE, LOCATION};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use url::Url;

pub use super::guard::FetchPolicy;

pub const USER_AGENT: &str = concat!("kaji/", env!("CARGO_PKG_VERSION"));

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum FetchMode {
    /// Texte lisible extrait du HTML.
    #[default]
    Markdown,
    /// Corps servi tel quel, tronqué au plafond.
    Raw,
}

#[derive(Debug)]
pub struct FetchOutcome {
    pub final_url: String,
    pub status: u16,
    pub content_type: Option<String>,
    pub body: String,
    pub truncated: bool,
}

pub async fn fetch_guarded(url: &str, policy: &FetchPolicy) -> Result<FetchOutcome, WebError> {
    let mut current =
        Url::parse(url).map_err(|error| WebError::InvalidUrl(format!("{url} — {error}")))?;
    let mut hops = 0usize;

    loop {
        let target = guard::check_url(&current, policy)?;
        let addrs = guard::resolve_target(&target, policy).await?;
        let client = build_client(&target, &addrs, policy)?;

        let response = client
            .get(current.clone())
            .header(
                ACCEPT,
                "text/html,text/plain,application/json;q=0.9,*/*;q=0.8",
            )
            .send()
            .await
            .map_err(|error| WebError::Transport(error.to_string()))?;

        let status = response.status();
        if status.is_redirection() {
            let location = response
                .headers()
                .get(LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| WebError::RedirectWithoutLocation(current.to_string()))?;
            let next = current
                .join(location)
                .map_err(|error| WebError::InvalidUrl(format!("{location} — {error}")))?;

            hops += 1;
            if hops > policy.max_redirects {
                return Err(WebError::TooManyRedirects(policy.max_redirects));
            }
            current = next;
            continue;
        }

        if !status.is_success() {
            return Err(WebError::HttpStatus {
                url: current.to_string(),
                status: status.as_u16(),
            });
        }

        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let (bytes, truncated) = read_capped(response, policy.max_bytes).await?;

        return Ok(FetchOutcome {
            final_url: current.to_string(),
            status: status.as_u16(),
            content_type,
            body: to_capped_string(bytes, policy.max_bytes),
            truncated,
        });
    }
}

/// Le client d'un saut. `no_proxy` est délibéré : un proxy résoudrait le nom
/// lui-même et rendrait l'épinglage — donc la garde — inopérant.
fn build_client(
    target: &guard::Target,
    addrs: &[SocketAddr],
    policy: &FetchPolicy,
) -> Result<reqwest::Client, WebError> {
    let mut builder = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(policy.timeout)
        .connect_timeout(CONNECT_TIMEOUT)
        .no_proxy();

    if let Some(domain) = target.domain() {
        builder = builder.resolve_to_addrs(domain, addrs);
    }

    builder
        .build()
        .map_err(|error| WebError::Transport(error.to_string()))
}

async fn read_capped(
    mut response: reqwest::Response,
    max: usize,
) -> Result<(Vec<u8>, bool), WebError> {
    let mut buffer = Vec::new();
    let mut truncated = false;

    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| WebError::Transport(error.to_string()))?
    {
        if buffer.len() + chunk.len() > max {
            let room = max - buffer.len();
            buffer.extend_from_slice(&chunk[..room]);
            truncated = true;
            break;
        }
        buffer.extend_from_slice(&chunk);
    }

    Ok((buffer, truncated))
}

/// Une troncature au milieu d'un caractère produirait des `U+FFFD` de trois
/// octets, qui feraient repasser la chaîne au-dessus du plafond.
fn to_capped_string(bytes: Vec<u8>, max: usize) -> String {
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if text.len() > max {
        let mut cut = max;
        while cut > 0 && !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
    }
    text
}

pub async fn run_fetch(
    url: &str,
    mode: FetchMode,
    policy: &FetchPolicy,
) -> Result<String, WebError> {
    let outcome = fetch_guarded(url, policy).await?;

    let body = match mode {
        FetchMode::Raw => outcome.body,
        FetchMode::Markdown => {
            if looks_like_html(outcome.content_type.as_deref(), &outcome.body) {
                html_to_markdown(&outcome.body)
            } else {
                outcome.body
            }
        }
    };

    let mut rendered = format!("Source : {}\n\n{}", outcome.final_url, body);
    if outcome.truncated {
        rendered.push_str("\n\n[tronqué au plafond de 2 Mo]");
    }
    Ok(rendered)
}

fn looks_like_html(content_type: Option<&str>, body: &str) -> bool {
    if let Some(content_type) = content_type {
        let content_type = content_type.to_ascii_lowercase();
        if content_type.contains("html") || content_type.contains("xml") {
            return true;
        }
        if content_type.contains("text/") || content_type.contains("json") {
            return false;
        }
    }
    let head = body.trim_start().to_ascii_lowercase();
    head.starts_with("<!doctype html") || head.starts_with("<html")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mode_defaults_to_markdown() {
        assert_eq!(FetchMode::default(), FetchMode::Markdown);
        assert_eq!(
            serde_json::from_str::<FetchMode>("\"raw\"").unwrap(),
            FetchMode::Raw
        );
    }

    #[test]
    fn plain_text_is_never_run_through_the_html_extractor() {
        assert!(!looks_like_html(Some("text/plain"), "<p>x</p>"));
        assert!(looks_like_html(Some("text/html; charset=utf-8"), ""));
        assert!(looks_like_html(None, "<!DOCTYPE html><html>"));
        assert!(!looks_like_html(Some("application/json"), "{}"));
    }

    #[test]
    fn the_cap_never_lets_a_lossy_conversion_grow_past_it() {
        let bytes = vec![0xff; 64];
        assert!(to_capped_string(bytes, 64).len() <= 64);
    }
}
