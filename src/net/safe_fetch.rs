// SSRF-hardened reqwest client builder for user-supplied hosts/URLs.
//
// LocalSky's setup wizard probes hardware the operator names: an
// OpenSprinkler controller, an Ecowitt gateway, an OpenAI-compatible LLM
// endpoint. Those legitimately live on the home LAN (10/8, 172.16/12,
// 192.168/16, fc00::/7), so a blanket RFC1918 deny would break the
// product. What is NEVER a real irrigation device is the loopback range,
// link-local + cloud metadata (169.254.0.0/16, fe80::/10), the
// unspecified address, or multicast. Those are the SSRF targets an
// attacker reaches for (the 169.254.169.254 metadata endpoint above all),
// so we reject exactly those and allow normal private LAN ranges.
//
// On top of the address filter this builder:
//   - restricts the scheme to http/https (no file://, gopher://, etc.),
//   - resolves the host ONCE and pins the connection to the resolved IP
//     via ClientBuilder::resolve, which defeats DNS-rebinding (a name
//     that resolves to a public IP for the SSRF check then flips to
//     169.254.169.254 on the real connection cannot, because reqwest
//     reuses our pinned address instead of re-resolving), and
//   - disables redirect following (redirect::Policy::none()), so a
//     compliant-looking first hop cannot bounce the request to a
//     forbidden target.
//
// Callers keep their own timeouts: build_safe_client takes the timeout so
// the existing per-call budgets (8s soil probe, 10s controller, 30s LLM)
// are preserved exactly.
//
// The timeout bounds DURATION, not SIZE: reqwest's .json()/.text()/.bytes()
// buffer the whole body unbounded, so a target that streams garbage faster
// than the timeout can still exhaust memory. read_body_capped /
// read_json_capped are the companion body readers that close that hole;
// use them instead of reqwest's own readers on any response fetched from
// an operator-named target.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use reqwest::{Client, Url};

/// Hard ceiling on a response body read through the safe-fetch pathway.
/// The per-request timeout bounds how LONG a target may talk, not how MUCH:
/// a malicious or broken device that streams a multi-GB body fast enough
/// beats the timeout and OOMs the process if the body is buffered
/// unbounded. 8 MiB is generous for every payload these clients consume
/// (weather/controller/LLM JSON runs KiB to low MiB) while keeping the
/// worst-case buffer harmless.
pub const MAX_RESPONSE_BODY_BYTES: usize = 8 * 1024 * 1024;

/// Why a safe-fetch client could not be built for a user-supplied target,
/// or a response body could not be safely read from it.
/// Deliberately coarse: a caller surfaces a category, never the raw
/// upstream response, so this is not an information-leak oracle.
#[derive(Debug)]
pub enum SafeFetchError {
    /// URL did not parse, or carried no host.
    InvalidUrl,
    /// Scheme was not http or https.
    UnsupportedScheme,
    /// DNS resolution returned no addresses (host unknown / no records).
    DnsFailed,
    /// Every resolved address is a forbidden target (loopback,
    /// link-local/metadata, unspecified, or multicast).
    BlockedTarget,
    /// reqwest client construction failed (TLS root load, etc.).
    ClientBuild(String),
    /// Response body exceeded [`MAX_RESPONSE_BODY_BYTES`]; read aborted.
    BodyTooLarge,
    /// Body could not be read or decoded. Carries the COARSE category from
    /// `net::reqwest_error_category`, never raw reqwest text (whose Display
    /// embeds the target URL).
    BodyRead(&'static str),
}

impl std::fmt::Display for SafeFetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SafeFetchError::InvalidUrl => write!(f, "invalid url"),
            SafeFetchError::UnsupportedScheme => write!(f, "unsupported scheme (http/https only)"),
            SafeFetchError::DnsFailed => write!(f, "host did not resolve"),
            SafeFetchError::BlockedTarget => {
                write!(f, "target address is not a permitted device endpoint")
            }
            SafeFetchError::ClientBuild(e) => write!(f, "client build failed: {e}"),
            SafeFetchError::BodyTooLarge => write!(f, "response body exceeded the size cap"),
            SafeFetchError::BodyRead(c) => write!(f, "body read failed: {c}"),
        }
    }
}

impl std::error::Error for SafeFetchError {}

/// True for addresses that are NEVER a legitimate user device and are the
/// classic SSRF pivots: loopback, link-local (incl. the cloud metadata
/// 169.254.169.254 and IPv6 fe80::/10), the unspecified address, and
/// multicast. Normal private LAN ranges (10/8, 172.16/12, 192.168/16,
/// fc00::/7) are intentionally NOT here: real home hardware lives there.
pub fn is_forbidden_target(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_link_local() // 169.254.0.0/16, includes 169.254.169.254
                || v4.is_unspecified() // 0.0.0.0
                || v4.is_multicast()
                || v4.is_broadcast() // 255.255.255.255
        }
        IpAddr::V6(v6) => {
            v6.is_loopback() // ::1
                || v6.is_unspecified() // ::
                || v6.is_multicast()
                || is_v6_link_local(v6) // fe80::/10
                // An IPv4-mapped IPv6 address (::ffff:a.b.c.d) must be
                // judged by its embedded v4, or a forbidden v4 sneaks
                // through wearing a v6 coat.
                || v6.to_ipv4_mapped().is_some_and(|v4| is_forbidden_target(&IpAddr::V4(v4)))
        }
    }
}

/// fe80::/10 link-local. `Ipv6Addr::is_unicast_link_local` is unstable, so
/// match the top 10 bits (0xfe80) directly.
fn is_v6_link_local(v6: &std::net::Ipv6Addr) -> bool {
    (v6.segments()[0] & 0xffc0) == 0xfe80
}

/// Build a reqwest Client locked to a single safe, already-resolved IP for
/// the URL's host. Returns the client plus the parsed Url to send. The
/// host is resolved here and the connection pinned to that IP, so reqwest
/// never re-resolves (DNS-rebinding defense). Redirects are disabled and
/// only http/https are accepted.
pub async fn build_safe_client(
    url_str: &str,
    timeout: Duration,
) -> Result<(Client, Url), SafeFetchError> {
    let url = Url::parse(url_str).map_err(|_| SafeFetchError::InvalidUrl)?;
    match url.scheme() {
        "http" | "https" => {}
        _ => return Err(SafeFetchError::UnsupportedScheme),
    }
    let host = url
        .host_str()
        .ok_or(SafeFetchError::InvalidUrl)?
        .to_string();
    // Default port for the scheme when the URL omits one; needed for both
    // resolution and the resolve() pin.
    let port = url
        .port_or_known_default()
        .ok_or(SafeFetchError::InvalidUrl)?;

    // Resolve once. A bare-IP host resolves to itself; a name hits DNS.
    let candidates: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), port))
        .await
        .map_err(|_| SafeFetchError::DnsFailed)?
        .collect();
    if candidates.is_empty() {
        return Err(SafeFetchError::DnsFailed);
    }

    // Pick the first address that is not a forbidden target. If a name
    // resolves to a mix, the safe one is used and the connection is pinned
    // to it; if every address is forbidden, reject.
    let chosen = candidates
        .into_iter()
        .find(|addr| !is_forbidden_target(&addr.ip()))
        .ok_or(SafeFetchError::BlockedTarget)?;

    let client = Client::builder()
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        // Pin DNS for this host:port to the vetted IP so reqwest connects
        // to exactly what we checked, never a re-resolved (rebinding) one.
        .resolve(&host, chosen)
        .build()
        .map_err(|e| SafeFetchError::ClientBuild(e.to_string()))?;

    Ok((client, url))
}

/// Read a response body to completion under a hard byte ceiling
/// ([`MAX_RESPONSE_BODY_BYTES`]). Use this instead of reqwest's
/// `.json()`/`.text()`/`.bytes()` on any response from an operator-named
/// target (everything build_safe_client serves): those readers buffer with
/// no bound, so the only size cap a hostile body ever meets is this one.
/// A Content-Length already over the cap fails fast without reading; a
/// chunked or lying response is caught by the streaming accounting, which
/// aborts (dropping the connection) the moment the cap would be crossed.
pub async fn read_body_capped(resp: reqwest::Response) -> Result<Vec<u8>, SafeFetchError> {
    read_body_capped_limit(resp, MAX_RESPONSE_BODY_BYTES).await
}

/// [`read_body_capped`] with an explicit cap, kept separate so tests can
/// exercise the limit without shoveling 8 MiB through loopback.
async fn read_body_capped_limit(
    mut resp: reqwest::Response,
    cap: usize,
) -> Result<Vec<u8>, SafeFetchError> {
    if let Some(len) = resp.content_length() {
        if len > cap as u64 {
            return Err(SafeFetchError::BodyTooLarge);
        }
    }
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| SafeFetchError::BodyRead(crate::net::reqwest_error_category(&e)))?
    {
        if buf.len() + chunk.len() > cap {
            return Err(SafeFetchError::BodyTooLarge);
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

/// [`read_body_capped`] + JSON parse, the shape nearly every safe-fetch
/// call site wants. A parse failure maps to the same coarse bucket
/// reqwest's own decode errors use, so callers keep surfacing a category,
/// never upstream bytes.
pub async fn read_json_capped<T: serde::de::DeserializeOwned>(
    resp: reqwest::Response,
) -> Result<T, SafeFetchError> {
    let body = read_body_capped(resp).await?;
    serde_json::from_slice(&body)
        .map_err(|_| SafeFetchError::BodyRead("response could not be decoded"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn forbids_loopback_and_metadata_and_unspecified() {
        assert!(is_forbidden_target(&ip("127.0.0.1")));
        assert!(is_forbidden_target(&ip("127.10.20.30")));
        assert!(is_forbidden_target(&ip("::1")));
        // Cloud metadata + the rest of link-local.
        assert!(is_forbidden_target(&ip("169.254.169.254")));
        assert!(is_forbidden_target(&ip("169.254.0.1")));
        assert!(is_forbidden_target(&ip("fe80::1")));
        // Unspecified.
        assert!(is_forbidden_target(&ip("0.0.0.0")));
        assert!(is_forbidden_target(&ip("::")));
        // Multicast + broadcast.
        assert!(is_forbidden_target(&ip("224.0.0.1")));
        assert!(is_forbidden_target(&ip("255.255.255.255")));
        assert!(is_forbidden_target(&ip("ff02::1")));
        // IPv4-mapped loopback must be caught via the embedded v4.
        assert!(is_forbidden_target(&ip("::ffff:127.0.0.1")));
        assert!(is_forbidden_target(&ip("::ffff:169.254.169.254")));
    }

    #[test]
    fn allows_private_lan_ranges() {
        // Real home hardware lives on RFC1918 + ULA; these must pass so
        // the product keeps working for self-hosters.
        assert!(!is_forbidden_target(&ip("10.0.0.50")));
        assert!(!is_forbidden_target(&ip("172.16.5.20")));
        assert!(!is_forbidden_target(&ip("172.16.5.9")));
        assert!(!is_forbidden_target(&ip("172.31.255.254")));
        assert!(!is_forbidden_target(&ip("fc00::1234")));
        assert!(!is_forbidden_target(&ip("fd12:3456::1")));
        // A normal public address is fine too (e.g. a cloud LLM).
        assert!(!is_forbidden_target(&ip("8.8.8.8")));
        assert!(!is_forbidden_target(&ip("2606:4700:4700::1111")));
        // IPv4-mapped private address stays allowed.
        assert!(!is_forbidden_target(&ip("::ffff:10.0.0.50")));
    }

    #[tokio::test]
    async fn rejects_non_http_scheme() {
        let err = build_safe_client("file:///etc/passwd", Duration::from_secs(1))
            .await
            .unwrap_err();
        assert!(matches!(err, SafeFetchError::UnsupportedScheme));
    }

    #[tokio::test]
    async fn rejects_loopback_url() {
        let err = build_safe_client("http://127.0.0.1:80/x", Duration::from_secs(1))
            .await
            .unwrap_err();
        assert!(matches!(err, SafeFetchError::BlockedTarget));
    }

    #[tokio::test]
    async fn rejects_metadata_url() {
        let err = build_safe_client(
            "http://169.254.169.254/latest/meta-data/",
            Duration::from_secs(1),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, SafeFetchError::BlockedTarget));
    }

    #[tokio::test]
    async fn builds_for_private_lan_ip() {
        // A literal RFC1918 IP resolves to itself and must build a client.
        let (_client, url) =
            build_safe_client("http://10.0.0.50/get_livedata_info", Duration::from_secs(1))
                .await
                .expect("private LAN target must be allowed");
        assert_eq!(url.host_str(), Some("10.0.0.50"));
    }

    /// A reqwest::Response over an in-memory body (no network); the
    /// exact-size body makes content_length() report the true length.
    fn canned_response(body: Vec<u8>) -> reqwest::Response {
        reqwest::Response::from(
            axum::http::Response::builder()
                .body(reqwest::Body::from(body))
                .unwrap(),
        )
    }

    #[tokio::test]
    async fn capped_read_passes_a_small_body_and_parses_json() {
        let body = read_body_capped(canned_response(b"hello".to_vec()))
            .await
            .expect("small body must pass");
        assert_eq!(body, b"hello");

        let v: serde_json::Value = read_json_capped(canned_response(b"{\"ok\":true}".to_vec()))
            .await
            .expect("small JSON body must parse");
        assert_eq!(v["ok"], serde_json::Value::Bool(true));
    }

    #[tokio::test]
    async fn capped_read_rejects_an_over_cap_body() {
        // 256 bytes against a 64-byte cap: refused (fast-fail on the known
        // length, or the streaming accounting; either way BodyTooLarge).
        let err = read_body_capped_limit(canned_response(vec![b'x'; 256]), 64)
            .await
            .unwrap_err();
        assert!(matches!(err, SafeFetchError::BodyTooLarge));
    }

    #[tokio::test]
    async fn capped_read_aborts_a_stream_with_no_declared_length() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        // A close-delimited body advertises NO length, so the fast-fail
        // cannot fire and the chunk accounting must abort once the cap is
        // crossed, however much more the peer intends to send. Loopback is
        // fine here: the reader takes any Response, only build_safe_client
        // forbids loopback targets.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            // Drain the request head before answering.
            let mut head = [0u8; 1024];
            let _ = sock.read(&mut head).await;
            let _ = sock
                .write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n")
                .await;
            // Try to send 16x the cap; the reader hangs up first.
            let block = [b'x'; 1024];
            for _ in 0..64 {
                if sock.write_all(&block).await.is_err() {
                    break;
                }
            }
        });
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let resp = client
            .get(format!("http://{addr}/stream"))
            .send()
            .await
            .expect("local test server must answer");
        let err = read_body_capped_limit(resp, 4 * 1024).await.unwrap_err();
        assert!(matches!(err, SafeFetchError::BodyTooLarge));
    }
}
