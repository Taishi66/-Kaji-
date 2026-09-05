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
        "web_search: aucun backend de recherche configuré — définir KAJI_WEB_SEARCH_BACKEND \
         (brave, tavily ou searxng) puis la clé ou l'URL correspondante"
    )]
    NoSearchBackend,

    #[error("web_search: backend '{0}' inconnu — valeurs acceptées : brave, tavily, searxng")]
    UnknownSearchBackend(String),

    #[error("web_search: backend {backend} non configuré — renseigner {setting}")]
    BackendNotConfigured {
        backend: &'static str,
        setting: &'static str,
    },

    #[error("web_search: le backend {backend} a répondu {status}")]
    BackendHttp { backend: &'static str, status: u16 },

    #[error("web_search: réponse illisible du backend {backend} — {detail}")]
    BackendPayload {
        backend: &'static str,
        detail: String,
    },

    #[error("web_search: le backend {backend} est injoignable — {detail}")]
    BackendTransport {
        backend: &'static str,
        detail: String,
    },

    #[error(
        "web_search: l'endpoint configuré pour {backend} est refusé par la garde réseau — {detail}"
    )]
    BackendEndpointRefused {
        backend: &'static str,
        detail: String,
    },

    #[error("web_fetch: URL invalide — {0}")]
    InvalidUrl(String),

    #[error("web_fetch: schéma '{0}' refusé — seuls http et https sont autorisés")]
    BlockedScheme(String),

    #[error("web_fetch: port {0} refusé — ports autorisés : 80, 443, 8080, 8443")]
    BlockedPort(u16),

    #[error("web_fetch: identifiants dans l'URL de {0} — refusé")]
    BlockedUserinfo(String),

    #[error("web_fetch: {host} résout vers {addr} ({reason}) — hôte non joignable")]
    BlockedAddress {
        host: String,
        addr: IpAddr,
        reason: &'static str,
    },

    #[error("web_fetch: {host} ne résout vers aucune adresse — {detail}")]
    UnresolvedHost { host: String, detail: String },

    #[error("web_fetch: plus de {0} redirections")]
    TooManyRedirects(usize),

    #[error("web_fetch: redirection sans en-tête Location depuis {0}")]
    RedirectWithoutLocation(String),

    #[error("web_fetch: {url} a répondu {status}")]
    HttpStatus { url: String, status: u16 },

    #[error("web_fetch: délai de {0:?} dépassé, redirections comprises")]
    DeadlineExceeded(Duration),

    #[error("web_fetch: {0}")]
    Transport(String),
}
