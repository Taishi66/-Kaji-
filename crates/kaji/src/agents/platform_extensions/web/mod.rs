pub mod error;
pub mod extract;
pub mod fetch;
pub mod guard;
pub mod search;

use crate::agents::extension::PlatformExtensionContext;
use crate::agents::mcp_client::{Error, McpClientTrait};
use crate::agents::tool_execution::ToolCallContext;
use anyhow::Result;
use async_trait::async_trait;
use error::WebError;
use fetch::{run_fetch, FetchMode, FetchPolicy};
use indoc::indoc;
use rmcp::model::{
    CallToolResult, ContentBlock, Implementation, InitializeResult, JsonObject, ListToolsResult,
    ServerCapabilities, Tool, ToolAnnotations,
};
use schemars::{schema_for, JsonSchema};
use search::{backend_from_config, clamp_count, format_results, MAX_COUNT};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

pub static EXTENSION_NAME: &str = "web";
pub const WEB_SEARCH_TOOL: &str = "web_search";
pub const WEB_FETCH_TOOL: &str = "web_fetch";

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct WebSearchParams {
    /// Ce qu'il faut chercher, en langage naturel ou en mots-clés.
    query: String,
    /// Nombre de résultats voulus, 10 au maximum.
    #[serde(default)]
    count: Option<u8>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct WebFetchParams {
    /// L'URL http(s) à récupérer.
    url: String,
    /// `markdown` extrait le texte lisible, `raw` sert le corps tel quel.
    #[serde(default)]
    mode: Option<FetchMode>,
}

pub struct WebClient {
    info: InitializeResult,
}

impl WebClient {
    pub fn new(_context: PlatformExtensionContext) -> Result<Self> {
        let info = InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(
                Implementation::new(EXTENSION_NAME.to_string(), "1.0.0".to_string())
                    .with_title("Web"),
            )
            .with_instructions(
                indoc! {r#"
                Search the web and read pages.

                - web_search: needs a configured backend (KAJI_WEB_SEARCH_BACKEND = brave,
                  tavily or searxng, plus its API key or instance URL). Without it the tool
                  says so instead of guessing.
                - web_fetch: reads one http(s) URL. Private, loopback and link-local
                  addresses are refused, redirects included; set KAJI_WEB_ALLOW_PRIVATE=1 to
                  reach an internal network on purpose.

                Cite the URL of anything you report from a fetched page.
            "#}
                .to_string(),
            );

        Ok(Self { info })
    }

    fn tools() -> Vec<Tool> {
        let search_schema = serde_json::to_value(schema_for!(WebSearchParams))
            .expect("le schéma de web_search est sérialisable");
        let fetch_schema = serde_json::to_value(schema_for!(WebFetchParams))
            .expect("le schéma de web_fetch est sérialisable");

        // Ni l'un ni l'autre n'est annoté en lecture seule : les deux sortent de
        // la machine, ce qui est exactement ce qu'un mode à approbation doit
        // faire voir à l'utilisateur.
        let annotations = |title: &str| {
            ToolAnnotations::from_raw(
                Some(title.to_string()),
                Some(false),
                Some(false),
                Some(false),
                Some(true),
            )
        };

        vec![
            Tool::new(
                WEB_SEARCH_TOOL.to_string(),
                format!(
                    "Search the web through the configured backend. Returns at most {MAX_COUNT} \
                     results as title, URL and snippet."
                ),
                search_schema
                    .as_object()
                    .expect("un schéma JSON est un objet")
                    .clone(),
            )
            .annotate(annotations("Web search")),
            Tool::new(
                WEB_FETCH_TOOL.to_string(),
                indoc! {r#"
                    Fetch one http(s) URL and return its content. `mode: markdown` (default)
                    extracts readable text from HTML; `mode: raw` returns the body as served.
                    Capped at 2 MB, 30 s, 5 redirects.
                "#}
                .to_string(),
                fetch_schema
                    .as_object()
                    .expect("un schéma JSON est un objet")
                    .clone(),
            )
            .annotate(annotations("Web fetch")),
        ]
    }

    async fn handle_search(arguments: Option<JsonObject>) -> Result<String, WebError> {
        let params: WebSearchParams = parse(arguments)?;
        let count = clamp_count(params.count);
        let backend = backend_from_config()?;
        let results = backend.search(params.query.trim(), count).await?;
        Ok(format_results(params.query.trim(), &results))
    }

    async fn handle_fetch(arguments: Option<JsonObject>) -> Result<String, WebError> {
        let params: WebFetchParams = parse(arguments)?;
        run_fetch(
            params.url.trim(),
            params.mode.unwrap_or_default(),
            &FetchPolicy::from_env(),
        )
        .await
    }
}

fn parse<T: for<'de> Deserialize<'de>>(arguments: Option<JsonObject>) -> Result<T, WebError> {
    let arguments =
        arguments.ok_or_else(|| WebError::InvalidUrl("arguments absents".to_string()))?;
    serde_json::from_value(serde_json::Value::Object(arguments))
        .map_err(|error| WebError::InvalidUrl(format!("arguments illisibles — {error}")))
}

#[async_trait]
impl McpClientTrait for WebClient {
    async fn list_tools(
        &self,
        _session_id: &str,
        _next_cursor: Option<String>,
        _cancellation_token: CancellationToken,
    ) -> Result<ListToolsResult, Error> {
        Ok(ListToolsResult {
            tools: Self::tools(),
            next_cursor: None,
            meta: None,
            ..Default::default()
        })
    }

    async fn call_tool(
        &self,
        _ctx: &ToolCallContext,
        name: &str,
        arguments: Option<JsonObject>,
        _cancellation_token: CancellationToken,
    ) -> Result<CallToolResult, Error> {
        let outcome = match name {
            WEB_SEARCH_TOOL => Self::handle_search(arguments).await,
            WEB_FETCH_TOOL => Self::handle_fetch(arguments).await,
            other => {
                return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                    "Unknown tool: {other}"
                ))]))
            }
        };

        Ok(match outcome {
            Ok(text) => CallToolResult::success(vec![ContentBlock::text(text)]),
            Err(error) => CallToolResult::error(vec![ContentBlock::text(error.to_string())]),
        })
    }

    fn get_info(&self) -> Option<&InitializeResult> {
        Some(&self.info)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_tools_are_declared_with_their_schema() {
        let tools = WebClient::tools();
        let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_ref()).collect();
        assert_eq!(names, vec![WEB_SEARCH_TOOL, WEB_FETCH_TOOL]);

        for tool in &tools {
            let annotations = tool.annotations.as_ref().expect("annoté");
            assert_eq!(
                annotations.read_only_hint,
                Some(false),
                "{} sort de la machine, donc jamais auto-approuvé comme une lecture",
                tool.name
            );
            assert_eq!(annotations.open_world_hint, Some(true));
        }
    }

    #[tokio::test]
    async fn a_fetch_without_arguments_is_a_named_error() {
        let error = WebClient::handle_fetch(None).await.expect_err("refusé");
        assert!(error.to_string().contains("arguments absents"));
    }
}
