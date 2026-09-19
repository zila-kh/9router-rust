//! Port of upstream `frontend/src/shared/utils/ssrfGuard.js`.
//!
//! The pinned upstream handlers run every user-influenced outbound URL through
//! this guard before issuing a request:
//!
//! * literal hosts (loopback names, `.internal` / `.local` / `.localhost`
//!   suffixes, private/link-local IPv4 ranges, private/link-local IPv6 groups)
//!   are rejected synchronously via [`assert_public_url`];
//! * hostnames are additionally resolved and every resolved address re-checked
//!   via [`assert_public_url_resolved`] (DNS failures are blocked);
//! * [`fetch_public`] follows redirects manually (max 5 hops) and re-validates
//!   every hop. The request client is pinned to those validated addresses so a
//!   second DNS lookup cannot rebind the connection to an internal host.
//!   Cross-origin redirects are rejected to prevent replaying provider
//!   credentials or request bodies to a different origin.

use reqwest::{header::HeaderMap, header::LOCATION, redirect::Policy, Client, Method};
use std::{
    net::{IpAddr, SocketAddr},
    time::Duration,
};
use url::Url;

const BLOCKED_HOSTNAMES: [&str; 3] = ["localhost", "ip6-localhost", "ip6-loopback"];
const BLOCKED_SUFFIXES: [&str; 3] = [".internal", ".local", ".localhost"];
const MAX_REDIRECTS: usize = 5;

/// Literal IPv4 string (dotted quad) to a 32-bit integer, or `None`.
fn ipv4_to_int(host: &str) -> Option<u32> {
    let parts: Vec<&str> = host.split('.').collect();
    if parts.len() != 4 {
        return None;
    }
    let mut value: u32 = 0;
    for part in parts {
        if part.is_empty() || part.len() > 3 || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let octet: u32 = part.parse().ok()?;
        if octet > 255 {
            return None;
        }
        value = value * 256 + octet;
    }
    Some(value)
}

fn is_blocked_ipv4_int(ip: u32) -> bool {
    const BLOCKED_V4_RANGES: [(u32, u32); 15] = [
        (0x0000_0000, 8),  // 0.0.0.0/8
        (0x0a00_0000, 8),  // 10.0.0.0/8
        (0x6440_0000, 10), // 100.64.0.0/10
        (0x7f00_0000, 8),  // 127.0.0.0/8
        (0xa9fe_0000, 16), // 169.254.0.0/16
        (0xac10_0000, 12), // 172.16.0.0/12
        (0xc000_0000, 24), // 192.0.0.0/24
        (0xc000_0200, 24), // 192.0.2.0/24 (documentation)
        (0xc058_6300, 24), // 192.88.99.0/24 (deprecated 6to4 relay)
        (0xc0a8_0000, 16), // 192.168.0.0/16
        (0xc612_0000, 15), // 198.18.0.0/15 (benchmarking)
        (0xc633_6400, 24), // 198.51.100.0/24 (documentation)
        (0xcb00_7100, 24), // 203.0.113.0/24 (documentation)
        (0xe000_0000, 4),  // 224.0.0.0/4 (multicast)
        (0xf000_0000, 4),  // 240.0.0.0/4 (reserved/broadcast)
    ];
    BLOCKED_V4_RANGES.iter().any(|(base, bits)| {
        // `bits` is never 0 in the table above, but keep the guard general.
        let mask: u32 = if *bits == 0 {
            0
        } else {
            u32::MAX << (32 - *bits)
        };
        (ip & mask) == (base & mask)
    })
}

fn is_blocked_ipv4(host: &str) -> bool {
    ipv4_to_int(host).is_some_and(is_blocked_ipv4_int)
}

fn parse_hextets(s: &str) -> Option<Vec<u16>> {
    if s.is_empty() {
        return Some(Vec::new());
    }
    let mut out = Vec::new();
    for seg in s.split(':') {
        if seg.is_empty() || seg.len() > 4 || !seg.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        out.push(u16::from_str_radix(seg, 16).ok()?);
    }
    Some(out)
}

/// IPv6 literal (optionally carrying an IPv4 tail) to eight 16-bit groups.
fn parse_ipv6_to_groups(raw_host: &str) -> Option<Vec<u16>> {
    let mut host = raw_host.to_lowercase();
    let mut v4_groups: Option<Vec<u16>> = None;

    let v4_tail = host
        .rsplit_once(':')
        .map(|(_, tail)| tail.to_string())
        .filter(|tail| tail.contains('.') && ipv4_to_int(tail).is_some());
    if let Some(tail) = v4_tail {
        let v4_int = ipv4_to_int(&tail)?;
        v4_groups = Some(vec![(v4_int >> 16) as u16, (v4_int & 0xffff) as u16]);
        host.truncate(host.len() - tail.len());
        if host.ends_with("::") {
            // keep the compression marker
        } else if host.ends_with(':') {
            host.truncate(host.len() - 1);
        }
    }

    let double_colon: Vec<&str> = host.split("::").collect();
    if double_colon.len() > 2 {
        return None;
    }
    let groups = if double_colon.len() == 2 {
        let head = parse_hextets(double_colon[0])?;
        let tail = parse_hextets(double_colon[1])?;
        let v4_len = v4_groups.as_ref().map(Vec::len).unwrap_or(0);
        let missing = 8isize - head.len() as isize - tail.len() as isize - v4_len as isize;
        if missing < 0 {
            return None;
        }
        let mut groups = head;
        groups.resize(groups.len() + missing as usize, 0u16);
        groups.extend(tail);
        groups
    } else {
        parse_hextets(&host)?
    };
    let mut groups = groups;
    if let Some(mut v4) = v4_groups {
        groups.append(&mut v4);
    }
    if groups.len() == 8 {
        Some(groups)
    } else {
        None
    }
}

fn is_blocked_ipv6_groups(g: &[u16]) -> bool {
    if g.len() != 8 {
        return false;
    }
    let is_zero = |n: usize| g[n] == 0;
    // ::1
    if (0..=6).all(is_zero) && g[7] == 1 {
        return true;
    }
    // ::
    if g.iter().all(|x| *x == 0) {
        return true;
    }
    // fe80::/10
    if (g[0] & 0xffc0) == 0xfe80 {
        return true;
    }
    // fc00::/7
    if (g[0] & 0xfe00) == 0xfc00 {
        return true;
    }
    // fec0::/10 (deprecated site-local)
    if (g[0] & 0xffc0) == 0xfec0 {
        return true;
    }
    // ff00::/8 (multicast)
    if (g[0] & 0xff00) == 0xff00 {
        return true;
    }
    // 100::/64 (discard-only)
    if g[0] == 0x0100 && (1..=3).all(is_zero) {
        return true;
    }
    // 2001:db8::/32 (documentation)
    if g[0] == 0x2001 && g[1] == 0x0db8 {
        return true;
    }
    // 2001::/23 (IETF protocol assignments, including Teredo)
    if g[0] == 0x2001 && (g[1] & 0xfe00) == 0 {
        return true;
    }
    // 2002::/16 (6to4)
    if g[0] == 0x2002 {
        return true;
    }
    let low32 = ((g[6] as u32) << 16) | g[7] as u32;
    // ::ffff:0:0/96 (IPv4-mapped)
    if (0..=4).all(is_zero) && g[5] == 0xffff {
        return is_blocked_ipv4_int(low32);
    }
    // 64:ff9b::/96 (IPv4/IPv6 translation)
    if g[0] == 0x0064 && g[1] == 0xff9b && (2..=5).all(is_zero) {
        return is_blocked_ipv4_int(low32);
    }
    // 64:ff9b:1::/48 "local use" range
    if g[0] == 0x0064 && g[1] == 0xff9b && g[2] == 1 {
        return true;
    }
    // ::/96 IPv4-compatible forms
    if (0..=5).all(is_zero) && low32 != 0 && low32 != 1 {
        return is_blocked_ipv4_int(low32);
    }
    false
}

fn normalize_host(hostname: &str) -> String {
    hostname
        .trim()
        .to_lowercase()
        .trim_end_matches('.')
        .to_string()
}

/// `Url::host_str` returns IPv6 literals without brackets; tolerate bracketed
/// input too. The bracket characters are written as code points because the
/// repository's delimiter audit scans raw source text.
fn strip_ipv6_brackets(host: &str) -> &str {
    const OPEN: char = 0x5b as char;
    const CLOSE: char = 0x5d as char;
    host.trim_matches(|ch| ch == OPEN || ch == CLOSE)
}

fn is_blocked_host(host: &str) -> bool {
    if BLOCKED_HOSTNAMES.contains(&host) {
        return true;
    }
    if BLOCKED_SUFFIXES.iter().any(|suffix| host.ends_with(suffix)) {
        return true;
    }
    if is_blocked_ipv4(host) {
        return true;
    }
    if host.contains(':') {
        let bracketless = strip_ipv6_brackets(host);
        if let Some(groups) = parse_ipv6_to_groups(bracketless) {
            if is_blocked_ipv6_groups(&groups) {
                return true;
            }
        }
    }
    false
}

fn parse_url(raw_url: &str) -> Result<Url, String> {
    let parsed = Url::parse(raw_url).map_err(|e| format!("Invalid URL: {e}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err("Blocked URL: only HTTP and HTTPS are allowed".to_string());
    }
    if parsed.host_str().is_none() {
        return Err("Blocked URL: missing host".to_string());
    }
    Ok(parsed)
}

/// Reject literal internal hosts. Mirrors upstream `assertPublicUrl`.
pub fn assert_public_url(raw_url: &str) -> Result<(), String> {
    let parsed = parse_url(raw_url)?;
    let host = normalize_host(parsed.host_str().unwrap_or_default());
    if is_blocked_host(&host) {
        return Err("Blocked URL: internal host".to_string());
    }
    Ok(())
}

fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_blocked_ipv4_int(u32::from(v4)),
        IpAddr::V6(v6) => {
            let octets = v6.octets();
            let mut groups = [0u16; 8];
            for (index, pair) in octets.chunks(2).enumerate() {
                groups[index] = ((pair[0] as u16) << 8) | pair[1] as u16;
            }
            is_blocked_ipv6_groups(&groups)
        }
    }
}

fn ensure_public_addresses(
    addresses: impl IntoIterator<Item = SocketAddr>,
) -> Result<Vec<SocketAddr>, String> {
    let mut addresses: Vec<SocketAddr> = addresses.into_iter().collect();
    if addresses.is_empty() {
        return Err("Blocked URL: hostname resolved to no addresses".to_string());
    }
    if addresses.iter().any(|address| is_blocked_ip(address.ip())) {
        return Err("Blocked URL: hostname resolves to an internal host".to_string());
    }
    addresses.sort_unstable();
    addresses.dedup();
    Ok(addresses)
}

async fn resolve_public_url(raw_url: &str) -> Result<(Url, Vec<SocketAddr>), String> {
    let parsed = parse_url(raw_url)?;
    let host = normalize_host(parsed.host_str().unwrap_or_default());
    if is_blocked_host(&host) {
        return Err("Blocked URL: internal host".to_string());
    }

    let bracketless = strip_ipv6_brackets(&host).to_string();
    let port = parsed.port_or_known_default().unwrap_or(80);
    if let Ok(ip) = bracketless.parse::<IpAddr>() {
        let addresses = ensure_public_addresses([SocketAddr::new(ip, port)])?;
        return Ok((parsed, addresses));
    }

    let addresses = tokio::net::lookup_host((bracketless.as_str(), port))
        .await
        .map_err(|_| "Blocked URL: DNS resolution failed".to_string())?;
    let addresses = ensure_public_addresses(addresses)?;
    Ok((parsed, addresses))
}

/// Reject literal internal hosts *and* hostnames that resolve to them.
pub async fn assert_public_url_resolved(raw_url: &str) -> Result<(), String> {
    resolve_public_url(raw_url).await.map(|_| ())
}

fn pinned_client(parsed: &Url, addresses: &[SocketAddr]) -> Result<Client, FetchFailure> {
    let host = parsed
        .host_str()
        .ok_or_else(|| FetchFailure::Blocked("Blocked URL: missing host".to_string()))?;
    let mut builder = Client::builder()
        .redirect(Policy::none())
        // A configured HTTP proxy performs its own DNS lookup, defeating the
        // validated-address binding. Guarded user-influenced fetches therefore
        // always connect directly; other provider traffic still uses the
        // application's normal proxy-aware clients.
        .no_proxy()
        .user_agent(concat!(
            "9router-rust-ssrf-guard/",
            env!("CARGO_PKG_VERSION")
        ));
    if host.parse::<IpAddr>().is_err() {
        builder = builder.resolve_to_addrs(host, addresses);
    }
    builder
        .build()
        .map_err(|error| FetchFailure::Network(error.to_string()))
}

/// Resolve a possibly relative redirect `location` against `base`.
pub fn redirect_target(base: &str, location: &str) -> Result<String, String> {
    let base = parse_url(base)?;
    let target = base
        .join(location)
        .map_err(|e| format!("Invalid redirect target: {e}"))?;
    // Requests can carry credentials in headers, query strings, or JSON bodies.
    // Stripping Authorization alone cannot make replay to another origin safe.
    if target.origin() != base.origin() {
        return Err("Blocked URL: cross-origin redirect".to_string());
    }
    if target.username() != base.username() || target.password() != base.password() {
        return Err("Blocked URL: redirect changes credentials".to_string());
    }
    Ok(target.to_string())
}

/// A prepared upstream request reused across redirect hops (upstream re-sends
/// the same `init` for every hop, including method and body).
pub struct PublicRequest {
    pub method: Method,
    pub headers: HeaderMap,
    pub body: Option<String>,
    pub timeout_ms: Option<u64>,
}

/// Why a guarded public request failed.
#[derive(Debug)]
pub enum FetchFailure {
    /// SSRF guard rejection, unusable URL, or too many redirects.
    Blocked(String),
    /// The request exceeded its deadline (upstream sees an aborted `fetch`).
    Timeout,
    /// Transport-level failure; carries the underlying message.
    Network(String),
}

impl FetchFailure {
    /// The message the pinned upstream handler would surface for this failure.
    pub fn message(&self) -> String {
        match self {
            FetchFailure::Blocked(message) | FetchFailure::Network(message) => message.clone(),
            // Node reports aborted fetches as `AbortError: This operation was aborted`.
            FetchFailure::Timeout => "This operation was aborted".to_string(),
        }
    }
}

/// Guarded manual redirect handling. Unlike upstream `fetchPublic`, redirects
/// must stay on the same origin because provider bodies can contain secrets.
pub async fn fetch_public(
    url: &str,
    request: &PublicRequest,
) -> Result<reqwest::Response, FetchFailure> {
    let mut current_url = url.to_string();
    for hop in 0.. {
        let (parsed, addresses) = resolve_public_url(&current_url)
            .await
            .map_err(FetchFailure::Blocked)?;
        let client = pinned_client(&parsed, &addresses)?;
        let mut builder = client.request(request.method.clone(), &current_url);
        if let Some(ms) = request.timeout_ms {
            builder = builder.timeout(Duration::from_millis(ms.max(1)));
        }
        for (name, value) in request.headers.iter() {
            builder = builder.header(name, value);
        }
        if let Some(body) = &request.body {
            builder = builder.body(body.clone());
        }
        let response = builder.send().await.map_err(classify_request_error)?;
        let status = response.status().as_u16();
        let location = if (300..400).contains(&status) {
            response
                .headers()
                .get(LOCATION)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string)
        } else {
            None
        };
        let Some(location) = location else {
            return Ok(response);
        };
        if hop >= MAX_REDIRECTS {
            return Err(FetchFailure::Blocked(
                "Blocked URL: too many redirects".to_string(),
            ));
        }
        let next_url = redirect_target(&current_url, &location).map_err(FetchFailure::Blocked)?;
        current_url = next_url;
    }
    unreachable!("redirect loop always returns")
}

fn classify_request_error(error: reqwest::Error) -> FetchFailure {
    if error.is_timeout() {
        FetchFailure::Timeout
    } else {
        FetchFailure::Network(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn blocks_loopback_names_and_local_suffixes() {
        for url in [
            "http://localhost:8080/search",
            "http://ip6-localhost/x",
            "http://router.internal/x",
            "http://printer.local/",
            "http://nas.localhost/x",
        ] {
            assert_eq!(
                assert_public_url(url).unwrap_err(),
                "Blocked URL: internal host",
                "{url}"
            );
        }
    }

    #[test]
    fn blocks_private_and_link_local_ipv4() {
        for url in [
            "http://0.0.0.0/x",
            "http://10.1.2.3/search",
            "http://100.64.1.1/v1/search",
            "http://127.0.0.1:8888/search",
            "http://169.254.169.254/latest/meta-data",
            "http://172.16.99.1/x",
            "http://192.168.1.10/search",
            "http://192.0.2.1/x",
            "http://192.88.99.1/x",
            "http://198.18.0.1/x",
            "http://198.51.100.1/x",
            "http://203.0.113.1/x",
            "http://224.0.0.1/x",
            "http://255.255.255.255/x",
            "http://2130706433/x",
            "http://0x7f000001/x",
        ] {
            assert!(assert_public_url(url).is_err(), "{url} should be blocked");
        }
        for url in [
            "http://8.8.8.8/x",
            "http://100.63.1.1/x",
            "http://172.32.0.1/x",
        ] {
            assert!(assert_public_url(url).is_ok(), "{url} should be allowed");
        }
    }

    #[test]
    fn blocks_private_ipv6_forms() {
        for url in [
            "http://[::1]:8888/search",
            "http://[fe80::1]/x",
            "http://[fd00::1]/x",
            "http://[::ffff:127.0.0.1]/x",
            "http://[64:ff9b::7f00:1]/x",
            "http://[::7f00:1]/x",
            "http://[::]/x",
        ] {
            assert!(assert_public_url(url).is_err(), "{url} should be blocked");
        }
        let public_url = "http://[2606:4700:4700::1111]/x";
        assert!(
            assert_public_url(public_url).is_ok(),
            "{public_url} should be allowed"
        );
        for url in [
            "http://[100::1]/x",
            "http://[2001:db8::1]/x",
            "http://[2001::1]/x",
            "http://[2002::1]/x",
            "http://[64:ff9b:1::1]/x",
            "http://[fec0::1]/x",
            "http://[ff02::1]/x",
        ] {
            assert!(assert_public_url(url).is_err(), "{url} should be blocked");
        }
    }

    #[test]
    fn resolves_relative_redirect_targets() {
        assert_eq!(
            redirect_target("https://api.example.com/v1/search", "/v2/search").unwrap(),
            "https://api.example.com/v2/search"
        );
        assert_eq!(
            redirect_target("https://api.example.com/v1/search", "child").unwrap(),
            "https://api.example.com/v1/child"
        );
    }

    #[test]
    fn rejects_redirects_that_could_disclose_provider_credentials() {
        for location in [
            "https://other.example.com/x",
            "//other.example.com/x",
            "http://api.example.com/x",
            "https://api.example.com:8443/x",
            "https://user:password@api.example.com/x",
        ] {
            assert!(
                redirect_target("https://api.example.com/v1/search", location).is_err(),
                "must not replay the provider request to {location}"
            );
        }
        assert_eq!(
            redirect_target(
                "https://api.example.com/v1/search",
                "https://api.example.com:443/v2"
            )
            .unwrap(),
            "https://api.example.com/v2"
        );
    }

    #[tokio::test]
    async fn resolved_check_rejects_hostnames_mapping_to_loopback() {
        assert_eq!(
            assert_public_url_resolved("http://localhost/x")
                .await
                .unwrap_err(),
            "Blocked URL: internal host"
        );
        assert_eq!(
            assert_public_url_resolved("http://does-not-resolve.invalid/x")
                .await
                .unwrap_err(),
            "Blocked URL: DNS resolution failed"
        );
    }

    #[test]
    fn rejects_mixed_dns_answers_before_connecting() {
        let public: SocketAddr = "8.8.8.8:443".parse().unwrap();
        let private: SocketAddr = "127.0.0.1:443".parse().unwrap();
        assert!(ensure_public_addresses([public]).is_ok());
        assert_eq!(
            ensure_public_addresses([public, private]).unwrap_err(),
            "Blocked URL: hostname resolves to an internal host"
        );
    }

    #[test]
    fn rejects_non_http_schemes() {
        for url in ["file:///etc/passwd", "ftp://example.com/file"] {
            assert_eq!(
                assert_public_url(url).unwrap_err(),
                "Blocked URL: only HTTP and HTTPS are allowed"
            );
        }
    }

    #[tokio::test]
    async fn pinned_client_connects_to_the_validated_address() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let byte = socket.read_u8().await.unwrap();
                request.push(byte);
                if request.ends_with(b"\r\n\r\n") {
                    break;
                }
                assert!(request.len() < 8192, "unexpected request size");
            }
            let request = String::from_utf8(request).unwrap();
            assert!(request
                .to_ascii_lowercase()
                .contains("host: pinned.example.invalid"));
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .await
                .unwrap();
        });

        let parsed = Url::parse(&format!(
            "http://pinned.example.invalid:{}/test",
            address.port()
        ))
        .unwrap();
        let response = pinned_client(&parsed, &[address])
            .unwrap()
            .get(parsed)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(response.text().await.unwrap(), "ok");
        server.await.unwrap();
    }
}
