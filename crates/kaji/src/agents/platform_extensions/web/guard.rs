//! Garde SSRF : ce que `web_fetch` accepte de joindre.
//!
//! Trois barrières franchies avant chaque connexion, redirections comprises :
//! le schéma, le port, puis toutes les adresses vers lesquelles l'hôte résout.
//! La résolution est faite ici et l'adresse validée est ensuite épinglée sur le
//! client HTTP, donc une seconde résolution ne peut pas basculer vers un réseau
//! interne entre la vérification et la connexion.
//!
//! Aucune adresse interne n'est joignable par défaut, et il n'existe pas
//! d'interrupteur qui les ouvre toutes : l'opérateur ouvre des exceptions
//! **nommées** par `KAJI_WEB_ALLOW_HOSTS`, une liste séparée par des virgules
//! dont chaque entrée s'écrit `hôte[:port]` ou `CIDR[:port]` —
//! `127.0.0.1:11434`, `localhost:11434`, `10.0.0.0/8`, `[::1]:8888`. Une entrée
//! n'ouvre qu'elle-même : autoriser son Ollama local ne rend pas
//! `169.254.169.254` joignable, et le port n'est levé que pour l'entrée qui le
//! porte. Une entrée sans port s'en tient à la liste blanche de ports.
//!
//! Un CIDR **nomme une exception**, il n'ouvre pas le monde : son préfixe ne
//! peut pas être plus court que celui de la plus large des plages internes
//! usuelles (`10.0.0.0/8` en v4, `fc00::/7` en v6). `0.0.0.0/0` — qui rendrait
//! toute la garde inopérante d'une seule entrée — est donc refusé comme entrée
//! illisible, avec sa raison au journal.
//!
//! Ce plancher se lit sur la **forme** autant que sur la famille : une IPv4
//! voyage aussi en costume v6 (v4-mappée, 6to4, NAT64…), et `::ffff:0:0/96`
//! passerait le plancher v6 tout en nommant l'espace v4 entier. Un CIDR qui
//! touche l'un de ces préfixes porteurs prend donc le plancher de la v4,
//! reporté à la position de l'adresse embarquée — `/104` sous `::ffff:0:0/96`,
//! `/24` sous `2002::/16`.
//!
//! La variable est lue par `std::env::var`, jamais par `Config::get_param` :
//! l'escalade n'est pas posable depuis le fichier de configuration, qu'un agent
//! outillé peut écrire.

use super::error::WebError;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::str::FromStr;
use std::time::Duration;
use url::{Host, Url};

pub const ALLOWED_PORTS: &[u16] = &[80, 443, 8080, 8443];
pub const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_REDIRECTS: usize = 5;
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub const ALLOW_HOSTS_ENV: &str = "KAJI_WEB_ALLOW_HOSTS";

/// Le préfixe le plus court qu'un CIDR de la liste ait le droit de porter :
/// celui de `10.0.0.0/8`, la plus large plage privée v4. Plus court que ça,
/// l'entrée ne nomme plus une exception, elle éteint la garde.
pub const MIN_NET_PREFIX_V4: u8 = 8;

/// Son équivalent v6 : celui de `fc00::/7`, la plage unique-local. Il ne vaut
/// que pour un réseau qui ne touche aucune forme porteuse d'IPv4 — celles-là
/// prennent le plancher de la v4, reporté à la position de l'adresse embarquée.
pub const MIN_NET_PREFIX_V6: u8 = 7;

/// Les préfixes v6 qui **transportent** une IPv4 dans leurs bits de poids
/// faible — exactement les formes que [`embedded_ipv4`] déballe : v4-mappée,
/// v4-traduite, NAT64, 6to4, compatible. Chacun porte sa position de départ et
/// le plancher qui en découle : celui de la v4 (`/8`) décalé de cette position.
///
/// Un CIDR qui touche l'un d'eux ne nomme pas un réseau v6, il nomme de la v4 :
/// `::ffff:0:0/96` passe le plancher v6 (96 ≥ 7) et désigne pourtant **tout**
/// l'espace v4 sous sa forme mappée. Le refus tombe au parsing parce que c'est
/// la dernière occasion de le voir — [`AllowEntry::matches_host`] déballe
/// ensuite la v4 sans jamais revoir le préfixe.
const V4_BEARING_PREFIXES: [(Ipv6Addr, u8); 5] = [
    // `::ffff:0:0/96` — v4-mappée, l'IPv4 occupe les 32 derniers bits.
    (Ipv6Addr::new(0, 0, 0, 0, 0, 0xffff, 0, 0), 96),
    // `::ffff:0:0:0/96` — v4-traduite.
    (Ipv6Addr::new(0, 0, 0, 0, 0xffff, 0, 0, 0), 96),
    // `64:ff9b::/96` — traduction NAT64.
    (Ipv6Addr::new(0x64, 0xff9b, 0, 0, 0, 0, 0, 0), 96),
    // `2002::/16` — encapsulation 6to4, l'IPv4 occupe les bits 16 à 48.
    (Ipv6Addr::new(0x2002, 0, 0, 0, 0, 0, 0, 0), 16),
    // `::/96` — forme compatible héritée.
    (Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 0), 96),
];

/// Le préfixe minimal exigé d'un CIDR. Un réseau v6 qui **touche** une forme
/// porteuse d'IPv4 — qu'il vive dedans ou qu'il la recouvre — hérite du
/// plancher de la v4 reporté à la position de l'adresse embarquée ; le plus
/// exigeant l'emporte.
fn min_prefix(addr: IpAddr, prefix: u8) -> u8 {
    let IpAddr::V6(addr) = addr else {
        return MIN_NET_PREFIX_V4;
    };
    V4_BEARING_PREFIXES
        .iter()
        .filter(|(bearer, bearer_prefix)| {
            in_net(
                IpAddr::V6(addr),
                IpAddr::V6(*bearer),
                prefix.min(*bearer_prefix),
            )
        })
        .map(|(_, bearer_prefix)| bearer_prefix + MIN_NET_PREFIX_V4)
        .max()
        .unwrap_or(MIN_NET_PREFIX_V6)
}

/// Ce qu'une entrée de la liste désigne : un nom d'hôte tel qu'il est écrit
/// dans l'URL, ou un réseau qui contient l'adresse résolue.
#[derive(Clone, Debug, PartialEq, Eq)]
enum HostPattern {
    Name(String),
    Net { addr: IpAddr, prefix: u8 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AllowEntry {
    host: HostPattern,
    port: Option<u16>,
}

impl AllowEntry {
    /// Sans port explicite, l'entrée s'en tient à la liste blanche : ouvrir un
    /// hôte n'ouvre pas tous ses ports.
    fn allows_port(&self, port: u16) -> bool {
        match self.port {
            Some(listed) => listed == port,
            None => ALLOWED_PORTS.contains(&port),
        }
    }

    fn covers(&self, host: &str, ip: IpAddr, port: u16) -> bool {
        self.allows_port(port) && self.matches_host(host, ip)
    }

    fn matches_host(&self, host: &str, ip: IpAddr) -> bool {
        match &self.host {
            HostPattern::Name(name) => name.eq_ignore_ascii_case(host),
            HostPattern::Net { addr, prefix } => {
                in_net(ip, *addr, *prefix)
                    || matches!(ip, IpAddr::V6(v6) if embedded_ipv4(v6)
                        .is_some_and(|v4| in_net(IpAddr::V4(v4), *addr, *prefix)))
            }
        }
    }
}

/// Pourquoi une entrée est écartée. Une chaîne plutôt qu'un `()` : le journal
/// doit dire *ce qui* cloche, sinon un `0.0.0.0/0` refusé se lit comme une
/// faute de frappe. Possédée plutôt qu'empruntée : un refus de plancher nomme
/// la limite qu'il oppose, et elle dépend de l'entrée.
type EntryError = String;

impl FromStr for AllowEntry {
    type Err = EntryError;

    fn from_str(raw: &str) -> Result<Self, EntryError> {
        let (host, port) = split_host_port(raw.trim())?;
        if host.is_empty() || host.contains(char::is_whitespace) {
            return Err("hôte vide ou espacé".to_string());
        }

        let host = match host.split_once('/') {
            Some((addr, prefix)) => {
                let addr: IpAddr = addr
                    .parse()
                    .map_err(|_| "CIDR sans adresse valide".to_string())?;
                let prefix: u8 = prefix
                    .parse()
                    .map_err(|_| "préfixe CIDR illisible".to_string())?;
                let width = if addr.is_ipv4() { 32 } else { 128 };
                if prefix > width {
                    return Err(format!(
                        "préfixe /{prefix} plus long que l'adresse — maximum /{width}"
                    ));
                }
                let floor = min_prefix(addr, prefix);
                if prefix < floor {
                    return Err(format!(
                        "préfixe /{prefix} trop court — minimum /{floor} : une entrée nomme \
                         une exception, pas l'internet entier"
                    ));
                }
                HostPattern::Net { addr, prefix }
            }
            None => match host.parse::<IpAddr>() {
                Ok(addr) => HostPattern::Net {
                    addr,
                    prefix: if addr.is_ipv4() { 32 } else { 128 },
                },
                Err(_) => HostPattern::Name(host.to_ascii_lowercase()),
            },
        };

        Ok(Self { host, port })
    }
}

/// `[::1]:8888`, `10.0.0.0/8:9000`, `localhost`, `fe80::1`. Un littéral IPv6
/// sans crochets n'a pas de port : ses deux-points sont les siens.
fn split_host_port(raw: &str) -> Result<(&str, Option<u16>), EntryError> {
    if let Some(rest) = raw.strip_prefix('[') {
        let (host, rest) = rest
            .split_once(']')
            .ok_or_else(|| "crochet IPv6 non refermé".to_string())?;
        let port = match rest {
            "" => None,
            _ => Some(
                rest.strip_prefix(':')
                    .ok_or_else(|| "suffixe inattendu après le crochet IPv6".to_string())?
                    .parse()
                    .map_err(|_| "port illisible".to_string())?,
            ),
        };
        return Ok((host, port));
    }

    match raw.split_once(':') {
        Some((host, port)) if !port.contains(':') => Ok((
            host,
            Some(port.parse().map_err(|_| "port illisible".to_string())?),
        )),
        Some(_) => Ok((raw, None)),
        None => Ok((raw, None)),
    }
}

fn in_net(ip: IpAddr, net: IpAddr, prefix: u8) -> bool {
    fn bits_match(candidate: &[u8], net: &[u8], prefix: u8) -> bool {
        let full = usize::from(prefix / 8);
        let rest = prefix % 8;
        if candidate[..full] != net[..full] {
            return false;
        }
        if rest == 0 {
            return true;
        }
        let mask = 0xffu8 << (8 - rest);
        candidate[full] & mask == net[full] & mask
    }

    match (ip, net) {
        (IpAddr::V4(candidate), IpAddr::V4(net)) => {
            bits_match(&candidate.octets(), &net.octets(), prefix)
        }
        (IpAddr::V6(candidate), IpAddr::V6(net)) => {
            bits_match(&candidate.octets(), &net.octets(), prefix)
        }
        _ => false,
    }
}

#[derive(Clone, Debug)]
pub struct FetchPolicy {
    pub allowed: Vec<AllowEntry>,
    pub max_redirects: usize,
    pub max_bytes: usize,
    pub timeout: Duration,
}

impl FetchPolicy {
    pub fn strict() -> Self {
        Self {
            allowed: Vec::new(),
            max_redirects: MAX_REDIRECTS,
            max_bytes: MAX_BODY_BYTES,
            timeout: REQUEST_TIMEOUT,
        }
    }

    /// Une entrée illisible est écartée, pas interprétée : une liste mal écrite
    /// n'ouvre rien de plus que la politique stricte.
    pub fn allowing(spec: &str) -> Self {
        let allowed = spec
            .split(',')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
            .filter_map(|entry| match entry.parse::<AllowEntry>() {
                Ok(parsed) => Some(parsed),
                Err(reason) => {
                    tracing::warn!("{ALLOW_HOSTS_ENV} : entrée ignorée — '{entry}' : {reason}");
                    None
                }
            })
            .collect();
        Self {
            allowed,
            ..Self::strict()
        }
    }

    pub fn from_env() -> Self {
        match std::env::var(ALLOW_HOSTS_ENV) {
            Ok(spec) => Self::allowing(&spec),
            Err(_) => Self::strict(),
        }
    }

    pub fn allows(&self, host: &str, ip: IpAddr, port: u16) -> bool {
        self.allowed
            .iter()
            .any(|entry| entry.covers(host, ip, port))
    }

    /// Le port peut-il être joint par quelqu'un ? Refuser ici évite une
    /// résolution DNS ; l'autorisation définitive tient compte de l'adresse.
    fn port_is_conceivable(&self, port: u16) -> bool {
        ALLOWED_PORTS.contains(&port) || self.allowed.iter().any(|entry| entry.port == Some(port))
    }
}

impl Default for FetchPolicy {
    fn default() -> Self {
        Self::from_env()
    }
}

/// Un saut validé : l'hôte tel qu'écrit, et son IP quand l'URL en portait une
/// littérale (auquel cas il n'y a rien à résoudre).
#[derive(Debug, Clone)]
pub struct Target {
    pub host: String,
    pub literal_ip: Option<IpAddr>,
    pub port: u16,
}

impl Target {
    /// Le nom de domaine à épingler sur le client HTTP, absent pour une IP
    /// littérale que la pile réseau ne résout pas.
    pub fn domain(&self) -> Option<&str> {
        self.literal_ip.is_none().then_some(self.host.as_str())
    }
}

pub fn check_url(url: &Url, policy: &FetchPolicy) -> Result<Target, WebError> {
    match url.scheme() {
        "http" | "https" => {}
        other => return Err(WebError::BlockedScheme(other.to_string())),
    }

    if !url.username().is_empty() || url.password().is_some() {
        return Err(WebError::BlockedUserinfo(
            url.host_str().unwrap_or_default().to_string(),
        ));
    }

    let host = url
        .host()
        .ok_or_else(|| WebError::InvalidUrl(format!("{url} n'a pas d'hôte")))?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| WebError::InvalidUrl(format!("{url} n'a pas de port")))?;

    if !policy.port_is_conceivable(port) {
        return Err(WebError::BlockedPort(port));
    }

    let (host, literal_ip) = match host {
        Host::Domain(domain) => (domain.to_string(), None),
        Host::Ipv4(addr) => (addr.to_string(), Some(IpAddr::V4(addr))),
        Host::Ipv6(addr) => (addr.to_string(), Some(IpAddr::V6(addr))),
    };

    Ok(Target {
        host,
        literal_ip,
        port,
    })
}

/// Résout puis valide. Une seule adresse interne dans le lot suffit à refuser :
/// un résolveur hostile qui mélange une adresse publique et 127.0.0.1 ne doit
/// pas pouvoir jouer sur l'ordre de la liste.
pub async fn resolve_target(
    target: &Target,
    policy: &FetchPolicy,
) -> Result<Vec<SocketAddr>, WebError> {
    let addrs: Vec<SocketAddr> = match target.literal_ip {
        Some(ip) => vec![SocketAddr::new(ip, target.port)],
        None => tokio::net::lookup_host((target.host.as_str(), target.port))
            .await
            .map_err(|error| WebError::UnresolvedHost {
                host: target.host.clone(),
                detail: error.to_string(),
            })?
            .collect(),
    };

    if addrs.is_empty() {
        return Err(WebError::UnresolvedHost {
            host: target.host.clone(),
            detail: "la résolution n'a rendu aucune adresse".to_string(),
        });
    }

    for addr in &addrs {
        let ip = addr.ip();
        if policy.allows(&target.host, ip, target.port) {
            continue;
        }
        if let Some((_, reason)) = address_kind(ip) {
            return Err(WebError::BlockedAddress {
                host: target.host.clone(),
                addr: ip,
                reason,
            });
        }
        // Adresse publique : le port sort de la liste blanche et l'entrée qui
        // l'y avait fait entrer ne couvre pas cet hôte.
        if !ALLOWED_PORTS.contains(&target.port) {
            return Err(WebError::BlockedPort(target.port));
        }
    }

    Ok(addrs)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressKind {
    Loopback,
    Internal,
}

/// `None` pour une adresse publique routable. Le reste porte sa raison, en
/// clair, pour que le refus dise pourquoi.
pub fn address_kind(ip: IpAddr) -> Option<(AddressKind, &'static str)> {
    match ip {
        IpAddr::V4(v4) => ipv4_kind(v4),
        IpAddr::V6(v6) => ipv6_kind(v6),
    }
}

fn ipv4_kind(ip: Ipv4Addr) -> Option<(AddressKind, &'static str)> {
    let octets = ip.octets();

    if ip.is_loopback() {
        return Some((AddressKind::Loopback, "bouclage"));
    }
    if ip.is_unspecified() || octets[0] == 0 {
        return Some((AddressKind::Internal, "réseau courant"));
    }
    if ip.is_private() {
        return Some((AddressKind::Internal, "réseau privé"));
    }
    if ip.is_link_local() {
        return Some((AddressKind::Internal, "lien-local"));
    }
    if octets[0] == 100 && (64..128).contains(&octets[1]) {
        return Some((AddressKind::Internal, "espace partagé CGNAT"));
    }
    if octets[0] == 192 && octets[1] == 0 && octets[2] == 0 {
        return Some((AddressKind::Internal, "assignation de protocole IETF"));
    }
    if ip.is_documentation() {
        return Some((AddressKind::Internal, "plage de documentation"));
    }
    if octets[0] == 198 && (octets[1] == 18 || octets[1] == 19) {
        return Some((AddressKind::Internal, "banc d'essai réseau"));
    }
    if octets[0] == 192 && octets[1] == 88 && octets[2] == 99 {
        return Some((AddressKind::Internal, "anycast de relais 6to4"));
    }
    if ip.is_multicast() {
        return Some((AddressKind::Internal, "multicast"));
    }
    if octets[0] >= 240 {
        return Some((AddressKind::Internal, "plage réservée"));
    }
    None
}

fn ipv6_kind(ip: Ipv6Addr) -> Option<(AddressKind, &'static str)> {
    if let Some(v4) = embedded_ipv4(ip) {
        return ipv4_kind(v4);
    }

    let segments = ip.segments();

    if ip.is_loopback() {
        return Some((AddressKind::Loopback, "bouclage"));
    }
    if ip.is_unspecified() {
        return Some((AddressKind::Internal, "adresse non spécifiée"));
    }
    if (segments[0] & 0xfe00) == 0xfc00 {
        return Some((AddressKind::Internal, "unique-local"));
    }
    if (segments[0] & 0xffc0) == 0xfe80 {
        return Some((AddressKind::Internal, "lien-local"));
    }
    if (segments[0] & 0xffc0) == 0xfec0 {
        return Some((AddressKind::Internal, "site-local"));
    }
    if segments[0] == 0x0064 && segments[1] == 0xff9b && segments[2] == 0x0001 {
        return Some((AddressKind::Internal, "NAT64 à usage local"));
    }
    if ip.is_multicast() {
        return Some((AddressKind::Internal, "multicast"));
    }
    if segments[0] == 0x2001 && segments[1] == 0x0db8 {
        return Some((AddressKind::Internal, "plage de documentation"));
    }
    if segments[0] == 0x0100 && segments[1..4].iter().all(|part| *part == 0) {
        return Some((AddressKind::Internal, "trou noir"));
    }
    if segments[0] == 0x2001 && (segments[1] & 0xff00) == 0x0000 {
        return Some((AddressKind::Internal, "assignation de protocole IETF"));
    }
    None
}

/// L'IPv4 qu'une IPv6 transporte : forme mappée `::ffff:a.b.c.d`, traduction
/// NAT64 `64:ff9b::/96`, forme traduite `::ffff:0:a.b.c.d`, encapsulation 6to4
/// `2002:aabb:ccdd::`, et la forme compatible `::a.b.c.d`. Sans ce déballage,
/// `::ffff:127.0.0.1` passerait pour une adresse v6 quelconque.
fn embedded_ipv4(ip: Ipv6Addr) -> Option<Ipv4Addr> {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return Some(v4);
    }

    let segments = ip.segments();
    let trailing = ((segments[6] as u32) << 16) | segments[7] as u32;

    if segments[0] == 0x2002 {
        return Some(Ipv4Addr::from(
            ((segments[1] as u32) << 16) | segments[2] as u32,
        ));
    }

    if segments[0] == 0x0064
        && segments[1] == 0xff9b
        && segments[2..6].iter().all(|part| *part == 0)
    {
        return Some(Ipv4Addr::from(trailing));
    }

    // IPv4-translated : `::ffff:0:a.b.c.d`, à ne pas confondre avec la mappée.
    if segments[..4].iter().all(|part| *part == 0) && segments[4] == 0xffff && segments[5] == 0x0000
    {
        return Some(Ipv4Addr::from(trailing));
    }

    // `::` et `::1` ne sont pas des IPv4 déguisées.
    if segments[..6].iter().all(|part| *part == 0) && trailing > 1 {
        return Some(Ipv4Addr::from(trailing));
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind(literal: &str) -> Option<&'static str> {
        address_kind(literal.parse::<IpAddr>().unwrap()).map(|(_, reason)| reason)
    }

    #[test]
    fn public_addresses_pass() {
        assert_eq!(kind("1.1.1.1"), None);
        assert_eq!(kind("93.184.216.34"), None);
        assert_eq!(kind("2606:4700:4700::1111"), None);
    }

    #[test]
    fn every_internal_v4_range_is_named() {
        assert_eq!(kind("127.0.0.1"), Some("bouclage"));
        assert_eq!(kind("0.0.0.0"), Some("réseau courant"));
        assert_eq!(kind("10.0.0.1"), Some("réseau privé"));
        assert_eq!(kind("172.31.255.1"), Some("réseau privé"));
        assert_eq!(kind("192.168.0.1"), Some("réseau privé"));
        assert_eq!(kind("169.254.169.254"), Some("lien-local"));
        assert_eq!(kind("100.64.0.1"), Some("espace partagé CGNAT"));
        assert_eq!(kind("192.0.0.1"), Some("assignation de protocole IETF"));
        assert_eq!(kind("192.0.2.1"), Some("plage de documentation"));
        assert_eq!(kind("198.18.0.1"), Some("banc d'essai réseau"));
        assert_eq!(kind("192.88.99.1"), Some("anycast de relais 6to4"));
        assert_eq!(kind("224.0.0.1"), Some("multicast"));
        assert_eq!(kind("255.255.255.255"), Some("plage réservée"));
    }

    #[test]
    fn every_internal_v6_range_is_named() {
        assert_eq!(kind("::1"), Some("bouclage"));
        assert_eq!(kind("::"), Some("adresse non spécifiée"));
        assert_eq!(kind("fc00::1"), Some("unique-local"));
        assert_eq!(kind("fd00::1"), Some("unique-local"));
        assert_eq!(kind("fe80::1"), Some("lien-local"));
        assert_eq!(kind("ff02::1"), Some("multicast"));
        assert_eq!(kind("2001:db8::1"), Some("plage de documentation"));
        assert_eq!(kind("100::1"), Some("trou noir"));
        assert_eq!(kind("fec0::1"), Some("site-local"));
        assert_eq!(kind("64:ff9b:1::1"), Some("NAT64 à usage local"));
    }

    #[test]
    fn ipv6_wrappers_are_unwrapped_to_their_ipv4() {
        assert_eq!(kind("::ffff:127.0.0.1"), Some("bouclage"));
        assert_eq!(kind("::ffff:10.0.0.1"), Some("réseau privé"));
        assert_eq!(kind("64:ff9b::169.254.169.254"), Some("lien-local"));
        assert_eq!(kind("::127.0.0.1"), Some("bouclage"));
        assert_eq!(kind("::ffff:0:7f00:1"), Some("bouclage"));
        assert_eq!(kind("2002:7f00:1::"), Some("bouclage"));
        assert_eq!(kind("2002:a9fe:a9fe::"), Some("lien-local"));
        assert_eq!(kind("::ffff:1.1.1.1"), None);
    }

    fn allows(policy: &FetchPolicy, host: &str, ip: &str, port: u16) -> bool {
        policy.allows(host, ip.parse().unwrap(), port)
    }

    #[test]
    fn an_entry_opens_its_host_and_port_and_nothing_else() {
        let policy = FetchPolicy::allowing("127.0.0.1:11434");
        assert!(allows(&policy, "127.0.0.1", "127.0.0.1", 11434));
        assert!(!allows(&policy, "127.0.0.1", "127.0.0.1", 80));
        assert!(!allows(&policy, "127.0.0.2", "127.0.0.2", 11434));
        assert!(!allows(&policy, "metadata.test", "169.254.169.254", 11434));
    }

    #[test]
    fn an_entry_without_a_port_keeps_the_port_whitelist() {
        let policy = FetchPolicy::allowing("10.0.0.0/8");
        assert!(allows(&policy, "srv.lan", "10.1.2.3", 8080));
        assert!(!allows(&policy, "srv.lan", "10.1.2.3", 11434));
        assert!(!allows(&policy, "srv.lan", "192.168.0.1", 8080));
    }

    #[test]
    fn a_name_entry_matches_the_host_as_written() {
        let policy = FetchPolicy::allowing("localhost:11434, [::1]:8888");
        assert!(allows(&policy, "localhost", "127.0.0.1", 11434));
        assert!(allows(&policy, "LOCALHOST", "::1", 11434));
        assert!(!allows(&policy, "127.0.0.1", "127.0.0.1", 11434));
        assert!(allows(&policy, "::1", "::1", 8888));
    }

    #[test]
    fn an_unreadable_entry_is_dropped_not_widened() {
        let policy = FetchPolicy::allowing("pas une entrée, 10.0.0.0/99, [::1, , 127.0.0.1:99999");
        assert!(policy.allowed.is_empty(), "{:?}", policy.allowed);
    }

    /// Sans plancher de préfixe, une seule entrée éteint la garde entière : le
    /// refus doit tomber au parsing, pas au premier `web_fetch` vers un réseau
    /// interne.
    #[test]
    fn a_cidr_wider_than_the_widest_internal_range_is_refused() {
        for spec in [
            "0.0.0.0/0",
            "0.0.0.0/7",
            "128.0.0.0/1",
            "::/0",
            "::/6",
            "0.0.0.0/0:11434",
        ] {
            assert!(
                FetchPolicy::allowing(spec).allowed.is_empty(),
                "« {spec} » ouvre plus qu'une exception nommée"
            );
        }
    }

    #[test]
    fn the_usual_internal_ranges_stay_expressible() {
        for spec in ["10.0.0.0/8", "192.168.0.0/16", "127.0.0.0/8", "fc00::/7"] {
            assert_eq!(
                FetchPolicy::allowing(spec).allowed.len(),
                1,
                "« {spec} » devrait rester une entrée valide"
            );
        }
    }

    /// Le plancher v6 générique est aveugle aux formes traduites : `/96` le
    /// passe largement et nomme pourtant tout l'espace v4 en costume. Chaque
    /// préfixe porteur d'IPv4 impose donc le plancher de la v4, reporté à la
    /// position de l'adresse embarquée.
    #[test]
    fn a_v6_cidr_that_carries_the_whole_v4_space_is_refused() {
        for spec in [
            "::ffff:0:0/96",   // v4-mappée
            "::ffff:0:0:0/96", // v4-traduite
            "64:ff9b::/96",    // NAT64
            "2002::/16",       // 6to4
            "::/96",           // forme compatible
            "::ffff:0:0/103",  // toujours deux fois trop large
            "2002::/23",       // idem, côté 6to4
            "::/64",           // recouvre les formes à 96 bits
        ] {
            assert!(
                FetchPolicy::allowing(spec).allowed.is_empty(),
                "« {spec} » nomme de la v4 en costume v6, pas une exception"
            );
        }
    }

    #[test]
    fn a_translated_range_as_narrow_as_its_v4_floor_stays_expressible() {
        for spec in [
            "::ffff:10.0.0.0/104",
            "::ffff:0:10.0.0.0/104",
            "64:ff9b::10.0.0.0/104",
            "2002:0a00::/24",
            "::1/128",
            "fe80::/10",
        ] {
            assert_eq!(
                FetchPolicy::allowing(spec).allowed.len(),
                1,
                "« {spec} » nomme une exception aussi étroite qu'un /8 en v4"
            );
        }
    }

    #[test]
    fn a_rejected_entry_names_what_is_wrong_with_it() {
        assert_eq!(
            "0.0.0.0/0".parse::<AllowEntry>().unwrap_err(),
            "préfixe /0 trop court — minimum /8 : une entrée nomme une exception, \
             pas l'internet entier"
        );
        assert_eq!(
            "::ffff:0:0/96".parse::<AllowEntry>().unwrap_err(),
            "préfixe /96 trop court — minimum /104 : une entrée nomme une exception, \
             pas l'internet entier",
            "le refus nomme le plancher de la forme, pas celui de la famille"
        );
        assert_eq!(
            "10.0.0.0/99".parse::<AllowEntry>().unwrap_err(),
            "préfixe /99 plus long que l'adresse — maximum /32"
        );
    }

    #[test]
    fn a_v4_entry_also_covers_the_v6_form_of_that_address() {
        let policy = FetchPolicy::allowing("127.0.0.0/8:11434");
        assert!(allows(&policy, "x.test", "::ffff:127.0.0.1", 11434));
    }

    #[test]
    fn the_port_check_precedes_resolution() {
        let url = Url::parse("http://example.com:6379/").unwrap();
        assert!(matches!(
            check_url(&url, &FetchPolicy::strict()),
            Err(WebError::BlockedPort(6379))
        ));
        assert!(matches!(
            check_url(&url, &FetchPolicy::allowing("127.0.0.1:11434")),
            Err(WebError::BlockedPort(6379))
        ));
    }

    #[test]
    fn credentials_in_the_url_are_refused() {
        let url = Url::parse("http://user:pass@example.com/").unwrap();
        assert!(matches!(
            check_url(&url, &FetchPolicy::strict()),
            Err(WebError::BlockedUserinfo(_))
        ));
    }

    #[test]
    fn an_ipv6_literal_carries_its_address() {
        let url = Url::parse("http://[::1]:8080/x").unwrap();
        let target = check_url(&url, &FetchPolicy::strict()).unwrap();
        assert_eq!(target.literal_ip, Some("::1".parse::<IpAddr>().unwrap()));
        assert_eq!(target.domain(), None);
    }
}
