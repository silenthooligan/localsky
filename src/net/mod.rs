// Outbound network safety. safe_fetch builds reqwest clients for
// user-supplied hosts/URLs (wizard device probes, LLM endpoint tests)
// with SSRF hardening: a forbidden-target address filter that still
// allows private LAN ranges, DNS-rebinding-proof IP pinning, no redirect
// following, and an http/https scheme restriction.

pub mod safe_fetch;

/// Classify a `reqwest::Error` into a COARSE category string, never the raw
/// upstream message. The raw `reqwest::Error` Display embeds the target URL
/// and OS/TLS error text; reflecting it to an API caller (the wizard probe /
/// controller-test handlers do) turns an operator-supplied-host fetch into an
/// SSRF/exfil oracle and leaks the internal target. This maps the error to one
/// of a handful of stable buckets so callers (adapters' `Transport`/`Init`
/// wrappers, probe handlers) carry a category an operator can act on without
/// exposing the upstream's own bytes. Consistent with the Wave-1 body-trim
/// (status-only on bad HTTP status; this covers the connection-level errors).
pub fn reqwest_error_category(e: &reqwest::Error) -> &'static str {
    if e.is_timeout() {
        "request timed out"
    } else if e.is_connect() {
        "could not connect to host"
    } else if e.is_redirect() {
        "redirect not followed"
    } else if e.is_decode() {
        "response could not be decoded"
    } else if e.is_body() {
        "request/response body error"
    } else if e.is_request() {
        "request could not be sent"
    } else {
        "network error"
    }
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
    async fn reqwest_error_category_is_coarse_and_leaks_no_target() {
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
        // It is one of the stable buckets.
        const BUCKETS: &[&str] = &[
            "request timed out",
            "could not connect to host",
            "redirect not followed",
            "response could not be decoded",
            "request/response body error",
            "request could not be sent",
            "network error",
        ];
        assert!(BUCKETS.contains(&cat), "unexpected category: {cat}");
        // The target host never appears in the category text.
        assert!(
            !cat.contains(secret_host),
            "category must not echo the target host"
        );
    }
}
