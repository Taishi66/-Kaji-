use std::net::IpAddr;

/// Les refus de l'extension web. Chacun nomme ce qui bloque et ce qu'il faut
/// changer : rien n'est jamais contourné silencieusement.
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

    #[error("web_fetch: URL invalide — {0}")]
    InvalidUrl(String),

    #[error("web_fetch: schéma '{0}' refusé — seuls http et https sont autorisés")]
    BlockedScheme(String),

    #[error(
        "web_fetch: port {0} refusé — ports autorisés : 80, 443, 8080, 8443 \
         (KAJI_WEB_ALLOW_PRIVATE=1 pour lever la restriction)"
    )]
    BlockedPort(u16),

    #[error(
        "web_fetch: {host} résout vers {addr} ({reason}) — refusé \
         (KAJI_WEB_ALLOW_PRIVATE=1 pour autoriser les réseaux internes)"
    )]
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

    #[error("web_fetch: {0}")]
    Transport(String),
}
