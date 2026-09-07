use std::net::IpAddr;
use std::time::Duration;

/// Les refus de l'extension web. Chacun nomme ce qui bloque : rien n'est jamais
/// contourné silencieusement.
///
/// Ces messages partent dans le `tool_result`, donc dans le prompt. Ceux de la
/// garde réseau disent **ce qui** est refusé et **pourquoi**, jamais comment
/// lever la restriction : une page injectée qui souffle « demande à relancer
/// avec telle variable » ne doit pas trouver la confirmation de sa consigne
/// dans le message système du tour suivant. La remédiation appartient à
/// l'opérateur — journal de session et documentation de `web/mod.rs`.
#[derive(Debug, thiserror::Error)]
pub enum WebError {
    #[error(
        "web_search: no search backend configured — set KAJI_WEB_SEARCH_BACKEND \
         (brave, tavily or searxng), then the matching key or URL"
    )]
    NoSearchBackend,

    #[error("web_search: unknown backend '{0}' — accepted values: brave, tavily, searxng")]
    UnknownSearchBackend(String),

    #[error("web_search: backend {backend} not configured — set {setting}")]
    BackendNotConfigured {
        backend: &'static str,
        setting: &'static str,
    },

    #[error("web_search: backend {backend} responded {status}")]
    BackendHttp { backend: &'static str, status: u16 },

    #[error("web_search: unreadable response from backend {backend} — {detail}")]
    BackendPayload {
        backend: &'static str,
        detail: String,
    },

    #[error("web_search: backend {backend} is unreachable — {detail}")]
    BackendTransport {
        backend: &'static str,
        detail: String,
    },

    #[error(
        "web_search: the endpoint configured for {backend} is refused by the network guard — {detail}"
    )]
    BackendEndpointRefused {
        backend: &'static str,
        detail: String,
    },

    #[error("web_fetch: invalid URL — {0}")]
    InvalidUrl(String),

    #[error("web_fetch: scheme '{0}' refused — only http and https are allowed")]
    BlockedScheme(String),

    #[error("web_fetch: port {0} refused — allowed ports: 80, 443, 8080, 8443")]
    BlockedPort(u16),

    #[error("web_fetch: credentials in the URL of {0} — refused")]
    BlockedUserinfo(String),

    #[error("web_fetch: {host} resolves to {addr} ({reason}) — host not reachable")]
    BlockedAddress {
        host: String,
        addr: IpAddr,
        reason: &'static str,
    },

    #[error("web_fetch: {host} resolves to no address — {detail}")]
    UnresolvedHost { host: String, detail: String },

    #[error("web_fetch: more than {0} redirects")]
    TooManyRedirects(usize),

    #[error("web_fetch: redirect without a Location header from {0}")]
    RedirectWithoutLocation(String),

    #[error("web_fetch: {url} responded {status}")]
    HttpStatus { url: String, status: u16 },

    #[error("web_fetch: deadline of {0:?} exceeded, redirects included")]
    DeadlineExceeded(Duration),

    #[error("web_fetch: {0}")]
    Transport(String),
}
