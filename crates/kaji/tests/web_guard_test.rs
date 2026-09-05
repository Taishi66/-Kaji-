//! La garde SSRF de `web_fetch` : ce que l'extension refuse de joindre.
//!
//! Aucun cas ne touche le réseau. Les refus sont prononcés avant toute
//! connexion (schéma, port, adresse résolue), et les cas « autorisés » visent un
//! serveur HTTP local monté par le test.

use kaji::agents::platform_extensions::web::error::WebError;
use kaji::agents::platform_extensions::web::fetch::{fetch_guarded, FetchPolicy};
use std::net::IpAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// Politique de test : la boucle de rejeu ne doit jamais sortir de la machine,
/// donc le bouclage est ouvert et le reste des réseaux internes reste fermé.
/// C'est ce qui rend observable le refus d'une redirection vers un privé.
fn loopback_only() -> FetchPolicy {
    FetchPolicy {
        allow_loopback: true,
        allow_private: false,
        ..FetchPolicy::strict()
    }
}

async fn refusal(url: &str, policy: &FetchPolicy) -> WebError {
    match fetch_guarded(url, policy).await {
        Ok(outcome) => panic!("{url} aurait dû être refusé, a répondu {outcome:?}"),
        Err(error) => error,
    }
}

#[tokio::test]
async fn a_non_http_scheme_is_refused() {
    let policy = FetchPolicy::strict();
    for url in ["file:///etc/passwd", "ftp://example.com/x", "gopher://x/1"] {
        assert!(
            matches!(refusal(url, &policy).await, WebError::BlockedScheme(_)),
            "{url} : schéma refusé"
        );
    }
}

#[tokio::test]
async fn an_unexpected_port_is_refused_before_any_dns_lookup() {
    let policy = FetchPolicy::strict();
    for url in [
        "http://example.com:22/",
        "https://example.com:6379/",
        "http://example.com:11211/",
    ] {
        assert!(
            matches!(refusal(url, &policy).await, WebError::BlockedPort(_)),
            "{url} : port refusé"
        );
    }
}

#[tokio::test]
async fn the_default_ports_are_accepted_by_the_port_check() {
    let policy = FetchPolicy::strict();
    for url in [
        "http://10.0.0.1/",
        "https://10.0.0.1:443/",
        "http://10.0.0.1:8080/",
        "https://10.0.0.1:8443/",
    ] {
        assert!(
            matches!(refusal(url, &policy).await, WebError::BlockedAddress { .. }),
            "{url} : le port passe, c'est l'adresse qui refuse"
        );
    }
}

#[tokio::test]
async fn private_and_local_addresses_are_refused() {
    let policy = FetchPolicy::strict();
    for url in [
        "http://127.0.0.1/",
        "http://127.1.2.3/",
        "http://10.1.2.3/",
        "http://172.16.0.1/",
        "http://192.168.1.1/",
        "http://169.254.169.254/latest/meta-data/",
        "http://100.64.0.1/",
        "http://0.0.0.0/",
        "http://255.255.255.255/",
        "http://224.0.0.1/",
        "http://localhost/",
    ] {
        assert!(
            matches!(refusal(url, &policy).await, WebError::BlockedAddress { .. }),
            "{url} : adresse refusée"
        );
    }
}

#[tokio::test]
async fn ipv6_local_addresses_are_refused() {
    let policy = FetchPolicy::strict();
    for url in [
        "http://[::1]/",
        "http://[::]/",
        "http://[fe80::1]/",
        "http://[fc00::1]/",
        "http://[fd12:3456::1]/",
        "http://[ff02::1]/",
    ] {
        assert!(
            matches!(refusal(url, &policy).await, WebError::BlockedAddress { .. }),
            "{url} : adresse v6 refusée"
        );
    }
}

#[tokio::test]
async fn ipv4_mapped_and_translated_ipv6_are_refused() {
    let policy = FetchPolicy::strict();
    for url in [
        "http://[::ffff:127.0.0.1]/",
        "http://[::ffff:10.0.0.1]/",
        "http://[::ffff:169.254.169.254]/",
        "http://[64:ff9b::127.0.0.1]/",
        "http://[::127.0.0.1]/",
    ] {
        assert!(
            matches!(refusal(url, &policy).await, WebError::BlockedAddress { .. }),
            "{url} : la forme v6 d'une v4 interne est refusée"
        );
    }
}

/// Les écritures exotiques d'une IPv4 sont normalisées par le parseur d'URL :
/// la garde voit 127.0.0.1, pas la chaîne d'origine.
#[tokio::test]
async fn obfuscated_ipv4_literals_are_refused() {
    let policy = FetchPolicy::strict();
    for url in ["http://2130706433/", "http://0x7f.0.0.1/", "http://127.1/"] {
        assert!(
            matches!(refusal(url, &policy).await, WebError::BlockedAddress { .. }),
            "{url} : écriture obfusquée refusée"
        );
    }
}

#[tokio::test]
async fn the_private_opt_out_lifts_the_address_refusal() {
    let policy = FetchPolicy::permissive();
    // Rien n'écoute sur ce port : la garde laisse passer, c'est le transport
    // qui échoue. C'est l'assertion — le refus n'est plus prononcé.
    let error = refusal("http://127.0.0.1:8443/", &policy).await;
    assert!(
        matches!(error, WebError::Transport(_)),
        "opt-out : plus de refus d'adresse, seulement l'échec réseau : {error}"
    );
}

// ---------------------------------------------------------------------------
// Cas avec serveur local
// ---------------------------------------------------------------------------

struct Server {
    base: String,
    hits: Arc<AtomicUsize>,
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn server() -> Server {
    use axum::extract::State;
    use axum::http::{header, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::get;

    let hits = Arc::new(AtomicUsize::new(0));

    async fn page(State(hits): State<Arc<AtomicUsize>>) -> impl IntoResponse {
        hits.fetch_add(1, Ordering::SeqCst);
        (
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            "<html><body><h1>Titre</h1><p>Bonjour <b>monde</b>.</p></body></html>",
        )
    }

    async fn huge() -> impl IntoResponse {
        (
            [(header::CONTENT_TYPE, "text/plain")],
            "x".repeat(3 * 1024 * 1024),
        )
    }

    async fn to_private() -> impl IntoResponse {
        (
            StatusCode::FOUND,
            [(header::LOCATION, "http://10.0.0.1/secret")],
        )
    }

    async fn to_file() -> impl IntoResponse {
        (
            StatusCode::FOUND,
            [(header::LOCATION, "file:///etc/passwd")],
        )
    }

    let router = axum::Router::new()
        .route("/page", get(page))
        .route("/huge", get(huge))
        .route("/to-private", get(to_private))
        .route("/to-file", get(to_file))
        .route(
            "/hop/{n}",
            get(
                |axum::extract::Path(n): axum::extract::Path<u32>| async move {
                    (
                        StatusCode::FOUND,
                        [(header::LOCATION, format!("/hop/{}", n + 1))],
                    )
                },
            ),
        )
        .route(
            "/once",
            get(|| async { (StatusCode::FOUND, [(header::LOCATION, "/page")]) }),
        )
        .with_state(Arc::clone(&hits));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let handle = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    Server { base, hits, handle }
}

#[tokio::test]
async fn a_redirect_towards_a_private_address_is_refused() {
    let server = server().await;
    let error = refusal(&format!("{}/to-private", server.base), &loopback_only()).await;
    match error {
        WebError::BlockedAddress { addr, .. } => {
            assert_eq!(addr, "10.0.0.1".parse::<IpAddr>().unwrap());
        }
        other => panic!("la redirection vers un privé doit être refusée : {other}"),
    }
    assert_eq!(
        server.hits.load(Ordering::SeqCst),
        0,
        "rien d'autre n'a été servi"
    );
}

#[tokio::test]
async fn a_redirect_towards_a_non_http_scheme_is_refused() {
    let server = server().await;
    let error = refusal(&format!("{}/to-file", server.base), &loopback_only()).await;
    assert!(
        matches!(error, WebError::BlockedScheme(_)),
        "la redirection vers file:// est refusée : {error}"
    );
}

#[tokio::test]
async fn redirects_are_followed_and_capped() {
    let server = server().await;
    let policy = loopback_only();

    let outcome = fetch_guarded(&format!("{}/once", server.base), &policy)
        .await
        .expect("une redirection suivie");
    assert!(outcome.body.contains("Bonjour"));
    assert!(outcome.final_url.ends_with("/page"));

    let error = refusal(&format!("{}/hop/0", server.base), &policy).await;
    assert!(
        matches!(error, WebError::TooManyRedirects(5)),
        "au-delà de 5 sauts, refus nommé : {error}"
    );
}

#[tokio::test]
async fn the_body_is_capped() {
    let server = server().await;
    let outcome = fetch_guarded(&format!("{}/huge", server.base), &loopback_only())
        .await
        .expect("un corps tronqué reste un succès");
    assert!(outcome.truncated, "le corps est signalé tronqué");
    assert!(
        outcome.body.len() <= 2 * 1024 * 1024,
        "corps plafonné à 2 Mo, reçu {}",
        outcome.body.len()
    );
}

#[tokio::test]
async fn the_user_agent_identifies_kaji() {
    assert_eq!(
        kaji::agents::platform_extensions::web::fetch::USER_AGENT,
        concat!("kaji/", env!("CARGO_PKG_VERSION"))
    );
}
