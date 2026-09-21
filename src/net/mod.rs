// Outbound network safety. safe_fetch builds reqwest clients for
// user-supplied hosts/URLs (wizard device probes, LLM endpoint tests)
// with SSRF hardening: a forbidden-target address filter that still
// allows private LAN ranges, DNS-rebinding-proof IP pinning, no redirect
// following, and an http/https scheme restriction.

pub mod safe_fetch;
pub mod source_failure;
pub mod stream_failure;

/// The one way an adapter, a controller or a service builds its outbound
/// HTTP client: this timeout, this User-Agent (the derived per-install
/// identity, see `sources::derived_user_agent`), reqwest's defaults for
/// the rest. Adapters with a special need (a keyless authority that must
/// carry the operator's contact string, an SSRF-hardened user-supplied
/// host) start from `client_with` or `safe_fetch` instead of a builder of
/// their own.
pub fn client(timeout: std::time::Duration) -> reqwest::Client {
    client_with(timeout, &crate::sources::derived_user_agent())
}

/// `client` with an explicit User-Agent.
pub fn client_with(timeout: std::time::Duration, user_agent: &str) -> reqwest::Client {
    client_opts(timeout, None, user_agent)
}

/// The full shape: a request timeout, an optional separate connect
/// budget (so a dead host fails fast and a ladder reaches a live mirror
/// within one tick instead of burning the whole request budget on TCP
/// timeouts), and the User-Agent.
pub fn client_opts(
    timeout: std::time::Duration,
    connect_timeout: Option<std::time::Duration>,
    user_agent: &str,
) -> reqwest::Client {
    let mut b = reqwest::Client::builder()
        .timeout(timeout)
        .user_agent(user_agent);
    if let Some(c) = connect_timeout {
        b = b.connect_timeout(c);
    }
    b.build()
        // reqwest only fails to build on a TLS backend that cannot load its
        // roots; a fresh default client is the fallback every caller had.
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "http client build failed; using defaults");
            reqwest::Client::new()
        })
}

/// Safe typed transport evidence. Existing controller/probe/LLM wrappers
/// retain Display compatibility, but now include stable codes and status/OS
/// details. No configured URL, body or arbitrary upstream text is reflected.
pub fn reqwest_error_category(e: &reqwest::Error) -> crate::ports::source_error::SourceFailure {
    source_failure::from_reqwest(e, "HTTP request")
}

/// True when `ip` is loopback in ANY representation: IPv4 127/8, IPv6 ::1, or
/// an IPv4-mapped IPv6 loopback (::ffff:127.x.y.z). Companion of the LLM-probe
/// carve-out below; kept mapped-address-aware so a loopback cannot be judged
/// differently depending on which coat it wears.
fn is_loopback_any(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => v4.is_loopback(),
        std::net::IpAddr::V6(v6) => {
            v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
    }
}

/// LLM-scoped safe-fetch variant: identical to
/// [`safe_fetch::build_safe_client`] EXCEPT that loopback targets are allowed.
///
/// WHY A SEPARATE POLICY (and why this does not weaken `safe_fetch`): the LLM
/// auto-detect prober's entire job is finding an inference server the operator
/// runs NEXT TO LocalSky: Ollama on localhost:11434, llama.cpp on :8080, LM
/// Studio on :1234. On a native (or --network=host) install those are loopback
/// by definition, so the strict device policy, which rightly treats loopback
/// as never-a-weather-device, made the shipped "Auto" default unable to detect
/// anything anywhere. Loopback is not an SSRF pivot for this path: the probe
/// GETs fixed kind-specific paths (/api/tags, /v1/models) and reports only
/// reachable-or-not, never the response body. Weather-source, controller, and
/// soil-gateway probes keep the strict `build_safe_client`; only LLM endpoints
/// route here.
///
/// Everything else is the same hardening, byte for byte:
///   - http/https schemes only,
///   - link-local + cloud metadata (169.254.0.0/16, fe80::/10), unspecified,
///     multicast, and broadcast targets are still rejected,
///   - the host resolves ONCE and the connection is pinned to the vetted IP
///     (anti DNS-rebinding),
///   - redirects are disabled.
pub async fn build_llm_probe_client(
    url_str: &str,
    timeout: std::time::Duration,
) -> Result<(reqwest::Client, reqwest::Url), safe_fetch::SafeFetchError> {
    use safe_fetch::SafeFetchError;

    let url = reqwest::Url::parse(url_str).map_err(|_| SafeFetchError::InvalidUrl)?;
    match url.scheme() {
        "http" | "https" => {}
        _ => return Err(SafeFetchError::UnsupportedScheme),
    }
    let host = url
        .host_str()
        .ok_or(SafeFetchError::InvalidUrl)?
        .to_string();
    let port = url
        .port_or_known_default()
        .ok_or(SafeFetchError::InvalidUrl)?;

    // Resolve once. A bare-IP host resolves to itself; a name hits DNS.
    let candidates: Vec<std::net::SocketAddr> = tokio::net::lookup_host((host.as_str(), port))
        .await
        .map_err(SafeFetchError::DnsLookup)?
        .collect();
    if candidates.is_empty() {
        return Err(SafeFetchError::DnsFailed);
    }

    // Pick the first address that is not forbidden UNDER THE LLM POLICY:
    // everything `is_forbidden_target` rejects except loopback.
    let chosen = candidates
        .into_iter()
        .find(|addr| {
            let ip = addr.ip();
            !safe_fetch::is_forbidden_target(&ip) || is_loopback_any(&ip)
        })
        .ok_or(SafeFetchError::BlockedTarget)?;

    let client = reqwest::Client::builder()
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        // Pin DNS for this host:port to the vetted IP so reqwest connects to
        // exactly what we checked, never a re-resolved (rebinding) one.
        .resolve(&host, chosen)
        .build()
        .map_err(SafeFetchError::ClientBuild)?;

    Ok((client, url))
}

/// Constant-time byte-string equality (SC-08). Compares the full length
/// of both inputs in a fixed number of operations per byte so the time
/// taken does not leak how many leading bytes matched, defeating a
/// byte-at-a-time timing oracle against a shared secret. A length
/// mismatch still returns false but is folded into the same accumulator
/// so the comparison cost tracks the longer of the two inputs rather than
/// short-circuiting on the first length check. Used by the /ingest
/// receivers (Ecowitt passkey, webhook token) where the comparand is a
/// low-entropy operator-chosen secret an attacker may probe repeatedly.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    // XOR the length difference into the accumulator so unequal lengths
    // can never pass, then fold every byte. Indexing past the shorter
    // slice is avoided by walking the longer length and reading 0 for the
    // out-of-range side, keeping the work proportional to the longer input
    // without an early return.
    let mut diff: u8 = (a.len() as u64 ^ b.len() as u64)
        .to_le_bytes()
        .iter()
        .fold(0u8, |acc, &x| acc | x);
    let n = a.len().max(b.len());
    for i in 0..n {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push(p);
            }
        }
    }

    fn production(path: &std::path::Path) -> String {
        let src = std::fs::read_to_string(path).unwrap();
        if path.to_string_lossy().ends_with("_tests.rs") {
            return String::new();
        }
        crate::engine::clock::code_only(src.split("#[cfg(test)]").next().unwrap())
    }

    /// Every outbound HTTP client is built by `net::client` or `client_with`
    /// (or `safe_fetch`, which pins its own DNS); no adapter, controller
    /// or service keeps a builder of its own.
    #[test]
    fn no_client_builder_outside_net() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        walk(&root, &mut files);
        let mut offenders = Vec::new();
        for f in files {
            let rel = f
                .strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if rel.starts_with("net/") {
                continue;
            }
            let code = production(&f);
            for (n, line) in code.lines().enumerate() {
                if line.contains("Client::builder()") {
                    offenders.push(format!("{rel}:{}: {}", n + 1, line.trim()));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "build clients with net::client / net::client_with:
{}",
            offenders.join(
                "
"
            )
        );
    }

    /// A polling adapter runs on `sources::poll::run_polling`, or at least
    /// reports its edges through `ReachabilityLatch`, so both reachability
    /// transitions reach the bus.
    #[test]
    fn every_polling_adapter_reports_both_reachability_edges() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/sources");
        let mut files = Vec::new();
        walk(&root, &mut files);
        let mut offenders = Vec::new();
        for f in files {
            let name = f.file_name().unwrap().to_string_lossy().to_string();
            if name == "poll.rs" || name == "mod.rs" {
                continue;
            }
            let code = production(&f);
            let polls = code.contains("interval(") && code.contains("SourceEvent::");
            if !polls {
                continue;
            }
            if !(code.contains("run_polling(") || code.contains("ReachabilityLatch")) {
                offenders.push(name);
            }
        }
        assert!(
            offenders.is_empty(),
            "poll through sources::poll (run_polling or ReachabilityLatch): {offenders:?}"
        );
    }

    use super::constant_time_eq;

    #[test]
    fn equal_strings_match() {
        assert!(constant_time_eq(b"hunter2", b"hunter2"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn different_strings_do_not_match() {
        assert!(!constant_time_eq(b"hunter2", b"hunter3"));
        // A length mismatch never passes, even if one is a prefix.
        assert!(!constant_time_eq(b"hunter2", b"hunter"));
        assert!(!constant_time_eq(b"hunter", b"hunter2"));
        assert!(!constant_time_eq(b"secret", b""));
    }

    #[tokio::test]
    async fn llm_probe_client_allows_loopback() {
        // The whole point of the LLM policy: a localhost Ollama target builds a
        // client instead of dying on BlockedTarget before a byte is sent.
        let (_client, url) = super::build_llm_probe_client(
            "http://127.0.0.1:11434/api/tags",
            std::time::Duration::from_secs(1),
        )
        .await
        .expect("loopback must be allowed on the LLM probe path");
        assert_eq!(url.host_str(), Some("127.0.0.1"));
    }

    #[tokio::test]
    async fn llm_probe_client_still_blocks_metadata_and_schemes() {
        // Loopback is the ONLY relaxation: the metadata endpoint and non-http
        // schemes stay rejected exactly like the strict device policy.
        let err = super::build_llm_probe_client(
            "http://169.254.169.254/latest/meta-data/",
            std::time::Duration::from_secs(1),
        )
        .await
        .unwrap_err();
        assert!(matches!(
            err,
            super::safe_fetch::SafeFetchError::BlockedTarget
        ));
        let err =
            super::build_llm_probe_client("file:///etc/passwd", std::time::Duration::from_secs(1))
                .await
                .unwrap_err();
        assert!(matches!(
            err,
            super::safe_fetch::SafeFetchError::UnsupportedScheme
        ));
    }

    #[test]
    fn loopback_any_covers_v4_v6_and_mapped() {
        use super::is_loopback_any;
        assert!(is_loopback_any(&"127.0.0.1".parse().unwrap()));
        assert!(is_loopback_any(&"::1".parse().unwrap()));
        assert!(is_loopback_any(&"::ffff:127.0.0.1".parse().unwrap()));
        assert!(!is_loopback_any(&"10.0.0.50".parse().unwrap()));
        assert!(!is_loopback_any(&"169.254.169.254".parse().unwrap()));
    }

    #[tokio::test]
    async fn reqwest_error_category_is_typed_and_leaks_no_target() {
        // A timeout to an unroutable address yields a real reqwest::Error; the
        // category must be one of the fixed buckets and must NOT echo the
        // target host/URL or raw OS text (the leak the trim closes).
        let secret_host = "192.0.2.123"; // TEST-NET-1, unroutable.
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(50))
            .build()
            .unwrap();
        let err = client
            .get(format!("http://{secret_host}:81/probe"))
            .send()
            .await
            .expect_err("unroutable target must error");
        let cat = super::reqwest_error_category(&err);
        use crate::ports::source_error::SourceErrorCode;
        assert!(matches!(
            cat.code,
            SourceErrorCode::Timeout | SourceErrorCode::Connection | SourceErrorCode::Request
        ));
        let cat = cat.to_string();
        // The target host never appears in the category text.
        assert!(
            !cat.contains(secret_host),
            "category must not echo the target host"
        );
    }
}
