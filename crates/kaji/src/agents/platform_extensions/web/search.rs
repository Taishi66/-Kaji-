//! Recherche web derrière un trait, trois implémentations.
//!
//! Les endpoints de Brave et Tavily sont des constantes du code : le modèle n'a
//! aucune prise dessus. Celui de SearXNG vient de la configuration, donc d'un
//! fichier qu'un agent outillé peut écrire en cours de session — il passe la
//! garde réseau comme n'importe quelle URL, à charge pour l'opérateur qui
//! héberge son instance sur un réseau interne de la déclarer dans
//! `KAJI_WEB_ALLOW_HOSTS` (cf. `guard.rs`).

use super::error::WebError;
use super::fetch::{FetchPolicy, USER_AGENT};
use super::guard;
use super::untrusted;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;

pub const MAX_COUNT: u8 = 10;
pub const DEFAULT_COUNT: u8 = 5;

pub const BACKEND_SETTING: &str = "KAJI_WEB_SEARCH_BACKEND";
pub const BRAVE_KEY_SETTING: &str = "BRAVE_API_KEY";
pub const TAVILY_KEY_SETTING: &str = "TAVILY_API_KEY";
pub const SEARXNG_URL_SETTING: &str = "SEARXNG_URL";

pub const BRAVE_ENDPOINT: &str = "https://api.search.brave.com/res/v1/web/search";
pub const TAVILY_ENDPOINT: &str = "https://api.tavily.com/search";

const BACKEND_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

#[async_trait]
pub trait SearchBackend: Send + Sync + std::fmt::Debug {
    fn name(&self) -> &'static str;
    async fn search(&self, query: &str, count: u8) -> Result<Vec<SearchResult>, WebError>;
}

/// Un seul client pour tous les backends : son pool de connexions n'a rien à
/// gagner à être reconstruit à chaque appel d'outil, et la découverte du proxy
/// système coûte plusieurs secondes au premier montage. `no_proxy` aligne la
/// recherche sur `web_fetch`, où un proxy rendrait la garde SSRF inopérante.
fn client() -> reqwest::Client {
    static CLIENT: once_cell::sync::Lazy<reqwest::Client> = once_cell::sync::Lazy::new(|| {
        reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .no_proxy()
            .timeout(BACKEND_TIMEOUT)
            .build()
            .expect("le client de recherche se construit sans TLS ni proxy à découvrir")
    });
    CLIENT.clone()
}

async fn json(backend: &'static str, request: reqwest::RequestBuilder) -> Result<Value, WebError> {
    let response = request
        .send()
        .await
        .map_err(|error| WebError::BackendTransport {
            backend,
            detail: error.to_string(),
        })?;

    let status = response.status();
    if !status.is_success() {
        return Err(WebError::BackendHttp {
            backend,
            status: status.as_u16(),
        });
    }

    response
        .json::<Value>()
        .await
        .map_err(|error| WebError::BackendPayload {
            backend,
            detail: error.to_string(),
        })
}

fn endpoint_with(
    backend: &'static str,
    endpoint: &str,
    params: &[(&str, &str)],
) -> Result<url::Url, WebError> {
    let mut url = url::Url::parse(endpoint).map_err(|error| WebError::BackendTransport {
        backend,
        detail: format!("endpoint invalide '{endpoint}' — {error}"),
    })?;
    {
        let mut pairs = url.query_pairs_mut();
        for (key, value) in params {
            pairs.append_pair(key, value);
        }
    }
    Ok(url)
}

/// Les trois APIs rendent la même chose sous trois noms : un tableau d'objets
/// dont on lit le titre, l'URL et un extrait.
fn read_results(items: Option<&Value>, snippet_key: &str, count: u8) -> Vec<SearchResult> {
    let Some(items) = items.and_then(Value::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let url = item.get("url").and_then(Value::as_str)?;
            Some(SearchResult {
                title: item
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or(url)
                    .to_string(),
                url: url.to_string(),
                snippet: item
                    .get(snippet_key)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            })
        })
        .take(count as usize)
        .collect()
}

#[derive(Debug)]
pub struct BraveBackend {
    endpoint: String,
    api_key: String,
    client: reqwest::Client,
}

impl BraveBackend {
    pub fn new(endpoint: String, api_key: String) -> Self {
        Self {
            endpoint,
            api_key,
            client: client(),
        }
    }
}

#[async_trait]
impl SearchBackend for BraveBackend {
    fn name(&self) -> &'static str {
        "brave"
    }

    async fn search(&self, query: &str, count: u8) -> Result<Vec<SearchResult>, WebError> {
        let url = endpoint_with(
            self.name(),
            &self.endpoint,
            &[("q", query), ("count", &count.to_string())],
        )?;
        let body = json(
            self.name(),
            self.client
                .get(url)
                .header("X-Subscription-Token", &self.api_key)
                .header("Accept", "application/json"),
        )
        .await?;

        Ok(read_results(
            body.get("web").and_then(|web| web.get("results")),
            "description",
            count,
        ))
    }
}

#[derive(Debug)]
pub struct TavilyBackend {
    endpoint: String,
    api_key: String,
    client: reqwest::Client,
}

impl TavilyBackend {
    pub fn new(endpoint: String, api_key: String) -> Self {
        Self {
            endpoint,
            api_key,
            client: client(),
        }
    }
}

#[async_trait]
impl SearchBackend for TavilyBackend {
    fn name(&self) -> &'static str {
        "tavily"
    }

    async fn search(&self, query: &str, count: u8) -> Result<Vec<SearchResult>, WebError> {
        let body = json(
            self.name(),
            self.client.post(&self.endpoint).json(&serde_json::json!({
                "api_key": self.api_key,
                "query": query,
                "max_results": count,
                "search_depth": "basic",
            })),
        )
        .await?;

        Ok(read_results(body.get("results"), "content", count))
    }
}

#[derive(Debug)]
pub struct SearxngBackend {
    endpoint: String,
    policy: FetchPolicy,
    client: reqwest::Client,
}

impl SearxngBackend {
    /// L'endpoint de recherche complet, pas la racine de l'instance :
    /// `backend_from_config` dérive l'un de l'autre.
    pub fn new(endpoint: String) -> Self {
        Self::with_policy(endpoint, FetchPolicy::from_env())
    }

    pub fn with_policy(endpoint: String, policy: FetchPolicy) -> Self {
        Self {
            endpoint,
            policy,
            client: client(),
        }
    }

    pub fn endpoint_from_instance(url: &str) -> String {
        format!("{}/search", url.trim_end_matches('/'))
    }
}

#[async_trait]
impl SearchBackend for SearxngBackend {
    fn name(&self) -> &'static str {
        "searxng"
    }

    async fn search(&self, query: &str, count: u8) -> Result<Vec<SearchResult>, WebError> {
        let url = endpoint_with(
            self.name(),
            &self.endpoint,
            &[("q", query), ("format", "json")],
        )?;

        let target = guard::check_url(&url, &self.policy).map_err(endpoint_refused)?;
        guard::resolve_target(&target, &self.policy)
            .await
            .map_err(endpoint_refused)?;

        let body = json(self.name(), self.client.get(url)).await?;

        Ok(read_results(body.get("results"), "content", count))
    }
}

/// La garde parle de `web_fetch` ; ici c'est la configuration de la recherche
/// qui est en cause, le refus doit le dire.
fn endpoint_refused(error: WebError) -> WebError {
    WebError::BackendEndpointRefused {
        backend: "searxng",
        detail: error
            .to_string()
            .trim_start_matches("web_fetch: ")
            .to_string(),
    }
}

pub fn clamp_count(requested: Option<u8>) -> u8 {
    requested.unwrap_or(DEFAULT_COUNT).clamp(1, MAX_COUNT)
}

pub fn backend_from_config() -> Result<Box<dyn SearchBackend>, WebError> {
    let config = crate::config::Config::global();

    let selected = config
        .get_param::<String>(BACKEND_SETTING)
        .ok()
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty())
        .ok_or(WebError::NoSearchBackend)?;

    match selected.as_str() {
        "brave" => Ok(Box::new(BraveBackend::new(
            BRAVE_ENDPOINT.to_string(),
            secret(config, "brave", BRAVE_KEY_SETTING)?,
        ))),
        "tavily" => Ok(Box::new(TavilyBackend::new(
            TAVILY_ENDPOINT.to_string(),
            secret(config, "tavily", TAVILY_KEY_SETTING)?,
        ))),
        "searxng" => {
            let instance = config
                .get_param::<String>(SEARXNG_URL_SETTING)
                .ok()
                .filter(|value| !value.trim().is_empty())
                .ok_or(WebError::BackendNotConfigured {
                    backend: "searxng",
                    setting: SEARXNG_URL_SETTING,
                })?;
            Ok(Box::new(SearxngBackend::new(
                SearxngBackend::endpoint_from_instance(instance.trim()),
            )))
        }
        other => Err(WebError::UnknownSearchBackend(other.to_string())),
    }
}

fn secret(
    config: &crate::config::Config,
    backend: &'static str,
    setting: &'static str,
) -> Result<String, WebError> {
    config
        .get_secret::<String>(setting)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .ok_or(WebError::BackendNotConfigured { backend, setting })
}

/// Titres et extraits sont rédigés par les sites indexés : ils sortent encadrés
/// comme le corps d'une page, pour la même raison.
pub fn format_results(query: &str, results: &[SearchResult]) -> String {
    if results.is_empty() {
        return format!("Aucun résultat pour « {query} ».");
    }

    let mut rendered = String::new();
    for (rank, result) in results.iter().enumerate() {
        rendered.push_str(&format!(
            "{}. {}\n   {}\n",
            rank + 1,
            result.title,
            result.url
        ));
        if !result.snippet.is_empty() {
            rendered.push_str(&format!("   {}\n", result.snippet));
        }
    }

    format!(
        "Résultats pour « {query} » :\n\n{}",
        untrusted::frame("résultats de recherche", rendered.trim_end())
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_count_is_clamped_on_both_ends() {
        assert_eq!(clamp_count(None), DEFAULT_COUNT);
        assert_eq!(clamp_count(Some(0)), 1);
        assert_eq!(clamp_count(Some(3)), 3);
        assert_eq!(clamp_count(Some(200)), MAX_COUNT);
    }

    #[test]
    fn a_result_without_url_is_dropped() {
        let items = serde_json::json!([
            { "title": "sans url", "content": "x" },
            { "title": "ok", "url": "https://x.test", "content": "y" },
        ]);
        let results = read_results(Some(&items), "content", 10);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].url, "https://x.test");
    }

    #[test]
    fn the_searxng_endpoint_is_derived_from_the_instance_url() {
        assert_eq!(
            SearxngBackend::endpoint_from_instance("https://searx.test/"),
            "https://searx.test/search"
        );
    }
}
