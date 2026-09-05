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
use std::time::Duration;

/// Politique de test : seule l'adresse exacte du serveur monté par le test est
/// ouverte, nommément. Tout le reste — le reste du bouclage compris — demeure
/// fermé, ce qui rend observable le refus d'une redirection vers un privé.
fn only(base: &str) -> FetchPolicy {
    FetchPolicy::allowing(base.trim_start_matches("http://"))
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
async fn a_url_carrying_credentials_is_refused() {
    let policy = FetchPolicy::strict();
    for url in ["http://user:pass@example.com/", "http://user@example.com/"] {
        assert!(
            matches!(refusal(url, &policy).await, WebError::BlockedUserinfo(_)),
            "{url} : l'identifiant dans l'URL est refusé"
        );
    }
}

/// Une liste non vide n'est pas un opt-out : elle ouvre ses entrées, et rien
/// d'autre. L'endpoint de métadonnées cloud reste fermé.
#[tokio::test]
async fn an_entry_opens_only_itself() {
    let policy = FetchPolicy::allowing("127.0.0.1:11434, 10.0.0.0/8:9000");

    for url in [
        "http://169.254.169.254/latest/meta-data/",
        "http://[fe80::1]/",
        "http://127.0.0.1/",
        "http://192.168.1.1/",
    ] {
        assert!(
            matches!(refusal(url, &policy).await, WebError::BlockedAddress { .. }),
            "{url} : refusé malgré une liste non vide"
        );
    }

    // Le port d'une entrée n'est pas ouvert aux autres hôtes.
    assert!(
        matches!(
            refusal("http://192.168.1.1:11434/", &policy).await,
            WebError::BlockedAddress { .. }
        ),
        "le port listé pour le bouclage n'ouvre pas un autre hôte"
    );
}

/// Un port hors liste blanche que personne n'a demandé est refusé avant toute
/// résolution, liste ou pas.
#[tokio::test]
async fn a_port_no_entry_names_is_refused_before_any_lookup() {
    let policy = FetchPolicy::allowing("127.0.0.1:11434");
    assert!(matches!(
        refusal("http://example.com:6379/", &policy).await,
        WebError::BlockedPort(6379)
    ));
}

#[tokio::test]
async fn an_unreadable_allowlist_opens_nothing() {
    let policy = FetchPolicy::allowing("pas une entrée, 10.0.0.0/99, :8080, ");
    for url in ["http://10.0.0.1/", "http://127.0.0.1/"] {
        assert!(
            matches!(refusal(url, &policy).await, WebError::BlockedAddress { .. }),
            "{url} : une liste illisible n'ouvre rien"
        );
    }
}

// ---------------------------------------------------------------------------
// Cas avec serveur local
// ---------------------------------------------------------------------------

struct Server {
    base: String,
    /// Toutes routes confondues : « rien d'autre n'a été servi » n'est une
    /// assertion que si chaque route compte.
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

    async fn huge(State(hits): State<Arc<AtomicUsize>>) -> impl IntoResponse {
        hits.fetch_add(1, Ordering::SeqCst);
        (
            [(header::CONTENT_TYPE, "text/plain")],
            "x".repeat(3 * 1024 * 1024),
        )
    }

    async fn to_private(State(hits): State<Arc<AtomicUsize>>) -> impl IntoResponse {
        hits.fetch_add(1, Ordering::SeqCst);
        (
            StatusCode::FOUND,
            [(header::LOCATION, "http://10.0.0.1/secret")],
        )
    }

    async fn to_file(State(hits): State<Arc<AtomicUsize>>) -> impl IntoResponse {
        hits.fetch_add(1, Ordering::SeqCst);
        (
            StatusCode::FOUND,
            [(header::LOCATION, "file:///etc/passwd")],
        )
    }

    /// Chaque saut traîne : la chaîne dépasse le plafond global sans qu'aucune
    /// requête ne dépasse le sien.
    async fn slow(
        State(hits): State<Arc<AtomicUsize>>,
        axum::extract::Path(n): axum::extract::Path<u32>,
    ) -> impl IntoResponse {
        hits.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        (
            StatusCode::FOUND,
            [(header::LOCATION, format!("/slow/{}", n + 1))],
        )
    }

    let router = axum::Router::new()
        .route("/page", get(page))
        .route("/huge", get(huge))
        .route("/to-private", get(to_private))
        .route("/to-file", get(to_file))
        .route("/slow/{n}", get(slow))
        .route(
            "/hop/{n}",
            get(
                |State(hits): State<Arc<AtomicUsize>>,
                 axum::extract::Path(n): axum::extract::Path<u32>| async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    (
                        StatusCode::FOUND,
                        [(header::LOCATION, format!("/hop/{}", n + 1))],
                    )
                },
            ),
        )
        .route(
            "/once",
            get(|State(hits): State<Arc<AtomicUsize>>| async move {
                hits.fetch_add(1, Ordering::SeqCst);
                (StatusCode::FOUND, [(header::LOCATION, "/page")])
            }),
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
    let error = refusal(&format!("{}/to-private", server.base), &only(&server.base)).await;
    match error {
        WebError::BlockedAddress { addr, .. } => {
            assert_eq!(addr, "10.0.0.1".parse::<IpAddr>().unwrap());
        }
        other => panic!("la redirection vers un privé doit être refusée : {other}"),
    }
    assert_eq!(
        server.hits.load(Ordering::SeqCst),
        1,
        "seule la redirection a été servie, rien d'autre"
    );
}

#[tokio::test]
async fn a_redirect_towards_a_non_http_scheme_is_refused() {
    let server = server().await;
    let error = refusal(&format!("{}/to-file", server.base), &only(&server.base)).await;
    assert!(
        matches!(error, WebError::BlockedScheme(_)),
        "la redirection vers file:// est refusée : {error}"
    );
}

/// Le plafond de temps annoncé par l'outil vaut pour l'appel entier : cinq
/// redirections traînantes ne le multiplient pas par six.
#[tokio::test]
async fn the_deadline_covers_the_whole_redirect_chain() {
    let server = server().await;
    let policy = FetchPolicy {
        timeout: Duration::from_millis(400),
        ..only(&server.base)
    };

    let started = std::time::Instant::now();
    let error = refusal(&format!("{}/slow/0", server.base), &policy).await;
    let elapsed = started.elapsed();

    assert!(
        matches!(error, WebError::DeadlineExceeded(_)),
        "le plafond global est nommé : {error}"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "la chaîne est coupée au plafond, pas six fois le plafond : {elapsed:?}"
    );
}

#[tokio::test]
async fn an_allowlisted_host_is_reachable_and_its_neighbours_are_not() {
    let server = server().await;
    let policy = only(&server.base);

    let outcome = fetch_guarded(&format!("{}/page", server.base), &policy)
        .await
        .expect("l'hôte listé est joignable");
    assert!(outcome.body.contains("Bonjour"));

    for url in [
        "http://169.254.169.254/latest/meta-data/",
        "http://10.0.0.1/",
        "http://127.0.0.1:8080/",
    ] {
        assert!(
            matches!(refusal(url, &policy).await, WebError::BlockedAddress { .. }),
            "{url} : hors liste, toujours refusé"
        );
    }
}

#[tokio::test]
async fn a_cidr_entry_covers_its_range_and_stops_there() {
    let server = server().await;
    let port = server
        .base
        .rsplit(':')
        .next()
        .expect("le port du serveur de test")
        .to_string();

    let inside = FetchPolicy::allowing(&format!("127.0.0.0/8:{port}"));
    fetch_guarded(&format!("{}/page", server.base), &inside)
        .await
        .expect("le CIDR couvre l'hôte du test");

    let elsewhere = FetchPolicy::allowing(&format!("10.0.0.0/8:{port}"));
    assert!(
        matches!(
            refusal(&format!("{}/page", server.base), &elsewhere).await,
            WebError::BlockedAddress { .. }
        ),
        "un CIDR voisin n'ouvre pas le bouclage"
    );
}

#[tokio::test]
async fn redirects_are_followed_and_capped() {
    let server = server().await;
    let policy = only(&server.base);

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
    let outcome = fetch_guarded(&format!("{}/huge", server.base), &only(&server.base))
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
