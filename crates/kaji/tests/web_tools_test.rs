//! Les deux outils de l'extension `web` : recherche derrière `SearchBackend`
//! (trois implémentations, serveur local en guise d'API) et extraction lisible.

use kaji::agents::platform_extensions::web::error::WebError;
use kaji::agents::platform_extensions::web::extract::html_to_markdown;
use kaji::agents::platform_extensions::web::fetch::{run_fetch, FetchMode, FetchPolicy};
use kaji::agents::platform_extensions::web::search::{
    backend_from_config, format_results, BraveBackend, SearchBackend, SearxngBackend,
    TavilyBackend, MAX_COUNT,
};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

struct Api {
    base: String,
    queries: Arc<Mutex<Vec<String>>>,
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for Api {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// Les trois APIs de recherche servies par un seul serveur local : chacune
/// répond dans son propre format, et enregistre ce qu'elle a reçu.
async fn api() -> Api {
    use axum::extract::{Query, State};
    use axum::http::HeaderMap;
    use axum::routing::{get, post};
    use axum::Json;
    use std::collections::HashMap;

    type Log = Arc<Mutex<Vec<String>>>;
    let queries: Log = Arc::new(Mutex::new(Vec::new()));

    async fn brave(
        State(log): State<Log>,
        headers: HeaderMap,
        Query(params): Query<HashMap<String, String>>,
    ) -> Json<serde_json::Value> {
        let token = headers
            .get("x-subscription-token")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        log.lock().unwrap().push(format!(
            "brave q={} count={} token={token}",
            params.get("q").cloned().unwrap_or_default(),
            params.get("count").cloned().unwrap_or_default()
        ));
        Json(json!({
            "web": { "results": [
                { "title": "Rust", "url": "https://rust-lang.org", "description": "Le langage" },
                { "title": "Kaji", "url": "https://kaji.dev", "description": "L'agent" }
            ]}
        }))
    }

    async fn tavily(
        State(log): State<Log>,
        Json(body): Json<serde_json::Value>,
    ) -> Json<serde_json::Value> {
        log.lock().unwrap().push(format!(
            "tavily q={} max={} key={}",
            body["query"].as_str().unwrap_or_default(),
            body["max_results"].as_u64().unwrap_or_default(),
            body["api_key"].as_str().unwrap_or_default()
        ));
        Json(json!({
            "results": [
                { "title": "Rust", "url": "https://rust-lang.org", "content": "Le langage" }
            ]
        }))
    }

    async fn searxng(
        State(log): State<Log>,
        Query(params): Query<HashMap<String, String>>,
    ) -> Json<serde_json::Value> {
        log.lock().unwrap().push(format!(
            "searxng q={} format={}",
            params.get("q").cloned().unwrap_or_default(),
            params.get("format").cloned().unwrap_or_default()
        ));
        Json(json!({
            "results": [
                { "title": "Rust", "url": "https://rust-lang.org", "content": "Le langage" },
                { "title": "Kaji", "url": "https://kaji.dev", "content": "L'agent" },
                { "title": "Trop", "url": "https://x.invalid", "content": "En trop" }
            ]
        }))
    }

    async fn refused() -> (axum::http::StatusCode, &'static str) {
        (axum::http::StatusCode::UNAUTHORIZED, "nope")
    }

    let router = axum::Router::new()
        .route("/res/v1/web/search", get(brave))
        .route("/search", post(tavily))
        .route("/searxng", get(searxng))
        .route("/refused", get(refused))
        .with_state(Arc::clone(&queries));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let handle = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    Api {
        base,
        queries,
        handle,
    }
}

#[tokio::test]
async fn the_brave_backend_reads_its_results() {
    let api = api().await;
    let backend = BraveBackend::new(format!("{}/res/v1/web/search", api.base), "cle".into());
    let results = backend.search("rust", 2).await.expect("brave répond");

    assert_eq!(backend.name(), "brave");
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].title, "Rust");
    assert_eq!(results[0].url, "https://rust-lang.org");
    assert_eq!(results[0].snippet, "Le langage");
    assert_eq!(
        api.queries.lock().unwrap()[0],
        "brave q=rust count=2 token=cle"
    );
}

#[tokio::test]
async fn the_tavily_backend_reads_its_results() {
    let api = api().await;
    let backend = TavilyBackend::new(format!("{}/search", api.base), "jeton".into());
    let results = backend.search("rust", 3).await.expect("tavily répond");

    assert_eq!(backend.name(), "tavily");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].url, "https://rust-lang.org");
    assert_eq!(
        api.queries.lock().unwrap()[0],
        "tavily q=rust max=3 key=jeton"
    );
}

#[tokio::test]
async fn the_searxng_backend_reads_its_results_and_honours_the_count() {
    let api = api().await;
    let backend = SearxngBackend::new(format!("{}/searxng", api.base));
    let results = backend.search("rust", 2).await.expect("searxng répond");

    assert_eq!(backend.name(), "searxng");
    assert_eq!(results.len(), 2, "la liste est coupée au nombre demandé");
    assert_eq!(api.queries.lock().unwrap()[0], "searxng q=rust format=json");
}

#[tokio::test]
async fn a_backend_http_failure_is_a_named_error() {
    let api = api().await;
    let backend = SearxngBackend::new(format!("{}/refused", api.base));
    let error = backend.search("rust", 2).await.expect_err("401 remonte");
    assert!(
        matches!(
            error,
            WebError::BackendHttp {
                backend: "searxng",
                status: 401
            }
        ),
        "erreur nommée : {error}"
    );
}

#[test]
fn the_count_is_clamped_to_ten() {
    assert_eq!(MAX_COUNT, 10);
}

#[test]
fn results_are_formatted_for_the_model() {
    use kaji::agents::platform_extensions::web::search::SearchResult;
    let rendered = format_results(
        "rust",
        &[SearchResult {
            title: "Rust".into(),
            url: "https://rust-lang.org".into(),
            snippet: "Le langage".into(),
        }],
    );
    assert!(rendered.contains("rust"));
    assert!(rendered.contains("Rust"));
    assert!(rendered.contains("https://rust-lang.org"));
    assert!(rendered.contains("Le langage"));
}

// ---------------------------------------------------------------------------
// Sélection du backend par la configuration
// ---------------------------------------------------------------------------

#[test]
fn an_absent_backend_setting_is_a_named_actionable_error() {
    let _guard = env_lock::lock_env([
        ("KAJI_WEB_SEARCH_BACKEND", None::<&str>),
        ("BRAVE_API_KEY", None),
        ("TAVILY_API_KEY", None),
        ("SEARXNG_URL", None),
    ]);
    let error = backend_from_config().expect_err("aucun backend configuré");
    assert!(matches!(error, WebError::NoSearchBackend));
    let message = error.to_string();
    assert!(
        message.contains("KAJI_WEB_SEARCH_BACKEND")
            && message.contains("brave")
            && message.contains("tavily")
            && message.contains("searxng"),
        "le message dit quoi faire : {message}"
    );
}

#[test]
fn an_unknown_backend_name_is_refused() {
    let _guard = env_lock::lock_env([("KAJI_WEB_SEARCH_BACKEND", Some("google"))]);
    let error = backend_from_config().expect_err("nom inconnu");
    assert!(matches!(error, WebError::UnknownSearchBackend(ref name) if name == "google"));
}

#[test]
fn a_selected_backend_without_its_key_is_refused_by_name() {
    let _guard = env_lock::lock_env([
        ("KAJI_WEB_SEARCH_BACKEND", Some("brave")),
        ("BRAVE_API_KEY", None),
    ]);
    let error = backend_from_config().expect_err("clé absente");
    assert!(
        matches!(
            error,
            WebError::BackendNotConfigured {
                backend: "brave",
                setting: "BRAVE_API_KEY"
            }
        ),
        "erreur nommée : {error}"
    );
}

#[test]
fn searxng_needs_its_instance_url() {
    let _guard = env_lock::lock_env([
        ("KAJI_WEB_SEARCH_BACKEND", Some("searxng")),
        ("SEARXNG_URL", None),
    ]);
    let error = backend_from_config().expect_err("URL absente");
    assert!(matches!(
        error,
        WebError::BackendNotConfigured {
            backend: "searxng",
            setting: "SEARXNG_URL"
        }
    ));
}

#[test]
fn a_configured_backend_is_built() {
    let _guard = env_lock::lock_env([
        ("KAJI_WEB_SEARCH_BACKEND", Some("tavily")),
        ("TAVILY_API_KEY", Some("jeton")),
    ]);
    let backend = backend_from_config().expect("backend construit");
    assert_eq!(backend.name(), "tavily");
}

// ---------------------------------------------------------------------------
// Extraction
// ---------------------------------------------------------------------------

#[test]
fn the_markdown_extraction_keeps_the_readable_text() {
    let html = r#"
        <html><head><title>Ignoré</title>
          <style>body { color: red }</style>
          <script>var x = "<p>piège</p>";</script>
        </head>
        <body>
          <h1>Titre</h1>
          <p>Bonjour <b>monde</b> &amp; compagnie.</p>
          <ul><li>un</li><li>deux</li></ul>
          <a href="https://kaji.dev">le site</a>
          <!-- commentaire -->
        </body></html>
    "#;
    let markdown = html_to_markdown(html);

    assert!(
        markdown.contains("# Titre"),
        "titre en markdown : {markdown}"
    );
    assert!(markdown.contains("Bonjour monde & compagnie."));
    assert!(markdown.contains("- un"));
    assert!(markdown.contains("- deux"));
    assert!(markdown.contains("[le site](https://kaji.dev)"));
    assert!(!markdown.contains("color: red"), "le style est jeté");
    assert!(!markdown.contains("piège"), "le script est jeté");
    assert!(!markdown.contains("commentaire"), "le commentaire est jeté");
    assert!(!markdown.contains('<'), "plus de balise : {markdown}");
}

#[test]
fn the_numeric_entities_are_decoded() {
    assert_eq!(html_to_markdown("<p>&#65;&#x42;&nbsp;C</p>").trim(), "AB C");
}

#[test]
fn the_blocks_are_separated() {
    let markdown = html_to_markdown("<p>un</p><p>deux</p><div>trois</div>");
    assert_eq!(
        markdown.trim().lines().filter(|l| !l.is_empty()).count(),
        3,
        "un bloc par ligne : {markdown}"
    );
}

// ---------------------------------------------------------------------------
// Les deux modes de fetch
// ---------------------------------------------------------------------------

struct Page {
    base: String,
    handle: tokio::task::JoinHandle<()>,
    hits: Arc<AtomicUsize>,
}

impl Drop for Page {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn page() -> Page {
    use axum::extract::State;
    use axum::http::header;
    use axum::response::IntoResponse;
    use axum::routing::get;

    let hits = Arc::new(AtomicUsize::new(0));

    async fn html(State(hits): State<Arc<AtomicUsize>>) -> impl IntoResponse {
        hits.fetch_add(1, Ordering::SeqCst);
        (
            [(header::CONTENT_TYPE, "text/html")],
            "<html><body><h1>Titre</h1><p>corps</p></body></html>",
        )
    }

    let router = axum::Router::new()
        .route("/p", get(html))
        .with_state(Arc::clone(&hits));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let handle = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    Page { base, handle, hits }
}

#[tokio::test]
async fn the_markdown_mode_returns_readable_text_and_raw_returns_the_source() {
    let page = page().await;
    let policy = FetchPolicy::permissive();
    let url = format!("{}/p", page.base);

    let markdown = run_fetch(&url, FetchMode::Markdown, &policy)
        .await
        .expect("mode markdown");
    assert!(markdown.contains("# Titre"));
    assert!(!markdown.contains("<h1>"));

    let raw = run_fetch(&url, FetchMode::Raw, &policy)
        .await
        .expect("mode raw");
    assert!(raw.contains("<h1>Titre</h1>"), "le brut garde la source");

    assert_eq!(page.hits.load(Ordering::SeqCst), 2);
}
