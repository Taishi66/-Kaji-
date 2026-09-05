//! Garde SSRF : ce que `web_fetch` accepte de joindre.
//!
//! Trois barrières franchies avant chaque connexion, redirections comprises :
//! le schéma, le port, puis toutes les adresses vers lesquelles l'hôte résout.
//! La résolution est faite ici et l'adresse validée est ensuite épinglée sur le
//! client HTTP, donc une seconde résolution ne peut pas basculer vers un réseau
//! interne entre la vérification et la connexion.

use super::error::WebError;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;
use url::{Host, Url};

pub const ALLOWED_PORTS: &[u16] = &[80, 443, 8080, 8443];
pub const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_REDIRECTS: usize = 5;
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub const ALLOW_PRIVATE_ENV: &str = "KAJI_WEB_ALLOW_PRIVATE";

#[derive(Clone, Debug)]
pub struct FetchPolicy {
    pub allow_loopback: bool,
    pub allow_private: bool,
    pub max_redirects: usize,
    pub max_bytes: usize,
    pub timeout: Duration,
}

impl FetchPolicy {
    pub fn strict() -> Self {
        Self {
            allow_loopback: false,
            allow_private: false,
            max_redirects: MAX_REDIRECTS,
            max_bytes: MAX_BODY_BYTES,
            timeout: REQUEST_TIMEOUT,
        }
    }

    /// L'opt-out complet : réseaux internes et ports libres, pour les usages
    /// self-hosted assumés.
    pub fn permissive() -> Self {
        Self {
            allow_loopback: true,
            allow_private: true,
            ..Self::strict()
        }
    }

    pub fn from_env() -> Self {
        match std::env::var(ALLOW_PRIVATE_ENV) {
            Ok(value) if is_truthy(&value) => Self::permissive(),
            _ => Self::strict(),
        }
    }

    /// La liste blanche de ports ne tient que tant que les réseaux internes
    /// sont fermés : un service self-hosted qu'on a explicitement autorisé
    /// écoute rarement sur 80 ou 443.
    pub fn restricts_ports(&self) -> bool {
        !self.allow_loopback && !self.allow_private
    }
}

impl Default for FetchPolicy {
    fn default() -> Self {
        Self::from_env()
    }
}

fn is_truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
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

    let host = url
        .host()
        .ok_or_else(|| WebError::InvalidUrl(format!("{url} n'a pas d'hôte")))?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| WebError::InvalidUrl(format!("{url} n'a pas de port")))?;

    if policy.restricts_ports() && !ALLOWED_PORTS.contains(&port) {
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
        if let Some(reason) = refuse_address(addr.ip(), policy) {
            return Err(WebError::BlockedAddress {
                host: target.host.clone(),
                addr: addr.ip(),
                reason,
            });
        }
    }

    Ok(addrs)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressKind {
    Loopback,
    Internal,
}

pub fn refuse_address(ip: IpAddr, policy: &FetchPolicy) -> Option<&'static str> {
    let (kind, reason) = address_kind(ip)?;
    match kind {
        AddressKind::Loopback if policy.allow_loopback => None,
        _ if policy.allow_private => None,
        _ => Some(reason),
    }
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
/// NAT64 `64:ff9b::/96`, et la forme compatible `::a.b.c.d`. Sans ce
/// déballage, `::ffff:127.0.0.1` passerait pour une adresse v6 quelconque.
fn embedded_ipv4(ip: Ipv6Addr) -> Option<Ipv4Addr> {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return Some(v4);
    }

    let segments = ip.segments();
    let trailing = ((segments[6] as u32) << 16) | segments[7] as u32;

    if segments[0] == 0x0064
        && segments[1] == 0xff9b
        && segments[2..6].iter().all(|part| *part == 0)
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
    }

    #[test]
    fn ipv6_wrappers_are_unwrapped_to_their_ipv4() {
        assert_eq!(kind("::ffff:127.0.0.1"), Some("bouclage"));
        assert_eq!(kind("::ffff:10.0.0.1"), Some("réseau privé"));
        assert_eq!(kind("64:ff9b::169.254.169.254"), Some("lien-local"));
        assert_eq!(kind("::127.0.0.1"), Some("bouclage"));
        assert_eq!(kind("::ffff:1.1.1.1"), None);
    }

    #[test]
    fn the_loopback_opt_in_is_narrower_than_the_private_one() {
        let policy = FetchPolicy {
            allow_loopback: true,
            allow_private: false,
            ..FetchPolicy::strict()
        };
        assert_eq!(refuse_address("127.0.0.1".parse().unwrap(), &policy), None);
        assert_eq!(
            refuse_address("10.0.0.1".parse().unwrap(), &policy),
            Some("réseau privé")
        );
    }

    #[test]
    fn the_port_check_precedes_resolution() {
        let url = Url::parse("http://example.com:6379/").unwrap();
        assert!(matches!(
            check_url(&url, &FetchPolicy::strict()),
            Err(WebError::BlockedPort(6379))
        ));
    }

    #[test]
    fn opening_an_internal_network_also_opens_the_ports() {
        assert!(FetchPolicy::strict().restricts_ports());
        assert!(!FetchPolicy::permissive().restricts_ports());
        assert!(!FetchPolicy {
            allow_loopback: true,
            allow_private: false,
            ..FetchPolicy::strict()
        }
        .restricts_ports());
    }

    #[test]
    fn an_ipv6_literal_carries_its_address() {
        let url = Url::parse("http://[::1]:8080/x").unwrap();
        let target = check_url(&url, &FetchPolicy::strict()).unwrap();
        assert_eq!(target.literal_ip, Some("::1".parse::<IpAddr>().unwrap()));
        assert_eq!(target.domain(), None);
    }
}
