//! Preserve typed transport evidence without reflecting credentials or bodies.
use super::safe_fetch::SafeFetchError;
use crate::ports::source_error::{SourceErrorCode as Code, SourceFailure};

pub use crate::diagnostics::from_anyhow;

pub fn from_grib(
    error: &gribberish::error::GribberishError,
    operation: &'static str,
) -> SourceFailure {
    use gribberish::error::GribberishError::*;
    let field = match error {
        DataRepresentationTemplateError(_) => "GRIB.data_representation_template",
        GridTemplateError(_) => "GRIB.grid_template",
        JpegError(_) => "GRIB.jpeg2000_data",
        MessageError(_) => "GRIB.message",
        IndexError(_) => "GRIB.index",
        TimeUnitError(_) => "GRIB.time_unit",
    };
    SourceFailure::new(Code::GribDecode, operation).with_field(field)
}

pub fn from_reqwest(error: &reqwest::Error, operation: &'static str) -> SourceFailure {
    let mut failure = if let Some(status) = error.status() {
        SourceFailure::http(status.as_u16(), None, operation)
    } else {
        let code = if error.is_timeout() {
            Code::Timeout
        } else if error.is_connect() {
            Code::Connection
        } else if error.is_redirect() {
            Code::HttpRedirect
        } else if error.is_decode() {
            Code::Decode
        } else if error.is_body() {
            Code::Body
        } else {
            Code::Request
        };
        SourceFailure::new(code, operation)
    };
    append_causes(&mut failure, error);
    failure
}

pub(crate) fn append_causes(
    failure: &mut SourceFailure,
    error: &(dyn std::error::Error + 'static),
) {
    let mut cause = Some(error);
    while let Some(inner) = cause {
        if let Some(io) = inner.downcast_ref::<std::io::Error>() {
            failure.io_kind = Some(format!("{:?}", io.kind()));
            failure.os_code = io.raw_os_error();
        }
        if let Some(json) = inner.downcast_ref::<serde_json::Error>() {
            failure.causes.push(from_json(json, failure.operation));
        }
        if let Some(tls) = inner.downcast_ref::<rustls::Error>() {
            failure.causes.push(from_tls(tls, failure.operation));
        }
        // std::io::Error::source forwards to the wrapped error's source,
        // which can skip the rustls error itself. Inspect get_ref directly.
        cause = inner
            .downcast_ref::<std::io::Error>()
            .and_then(|io| io.get_ref())
            .map(|error| error as &(dyn std::error::Error + 'static))
            .or_else(|| inner.source());
    }
}

fn from_tls(error: &rustls::Error, operation: &'static str) -> SourceFailure {
    use rustls::{CertificateError as Cert, Error as Tls};
    let (code, reason) = match error {
        Tls::InvalidCertificate(cert) => (
            Code::TlsCertificate,
            match cert {
                Cert::UnknownIssuer => "unknown_issuer",
                Cert::Expired | Cert::ExpiredContext { .. } => "expired",
                Cert::NotValidYet | Cert::NotValidYetContext { .. } => "not_valid_yet",
                Cert::NotValidForName | Cert::NotValidForNameContext { .. } => "hostname_mismatch",
                Cert::Revoked => "revoked",
                Cert::BadEncoding => "bad_encoding",
                Cert::BadSignature => "bad_signature",
                Cert::UnhandledCriticalExtension => "unhandled_critical_extension",
                Cert::UnknownRevocationStatus => "unknown_revocation_status",
                Cert::ExpiredRevocationList | Cert::ExpiredRevocationListContext { .. } => {
                    "expired_revocation_list"
                }
                Cert::InvalidPurpose | Cert::InvalidPurposeContext { .. } => "invalid_purpose",
                Cert::InvalidOcspResponse => "invalid_ocsp_response",
                Cert::UnsupportedSignatureAlgorithmContext { .. }
                | Cert::UnsupportedSignatureAlgorithmForPublicKeyContext { .. } => {
                    "unsupported_signature_algorithm"
                }
                Cert::ApplicationVerificationFailure => "application_verification_failed",
                _ => "certificate_verifier_unclassified",
            },
        ),
        Tls::NoCertificatesPresented => (Code::TlsCertificate, "no_certificate"),
        Tls::PeerIncompatible(_) => (Code::TlsProtocol, "incompatible_peer"),
        Tls::PeerMisbehaved(_) => (Code::TlsProtocol, "peer_protocol_violation"),
        Tls::InvalidMessage(_) => (Code::TlsProtocol, "invalid_tls_message"),
        Tls::AlertReceived(_) => (Code::TlsProtocol, "peer_fatal_alert"),
        Tls::NoApplicationProtocol => (Code::TlsProtocol, "no_application_protocol"),
        Tls::FailedToGetCurrentTime => (Code::TlsProtocol, "system_clock_unavailable"),
        Tls::FailedToGetRandomBytes => (Code::TlsProtocol, "system_entropy_unavailable"),
        _ => (Code::TlsProtocol, "tls_engine_unclassified"),
    };
    let mut failure = SourceFailure::new(code, operation);
    failure.tls_reason = Some(reason);
    failure
}

pub fn from_json(error: &serde_json::Error, operation: &'static str) -> SourceFailure {
    use serde_json::error::Category;
    let code = match error.classify() {
        Category::Syntax => Code::JsonSyntax,
        Category::Data => Code::JsonShape,
        Category::Eof => Code::JsonTruncated,
        Category::Io => Code::Body,
    };
    let mut failure = SourceFailure::new(code, operation);
    failure.line = Some(error.line());
    failure.column = Some(error.column());
    failure
}

pub fn from_safe(error: &SafeFetchError, operation: &'static str) -> SourceFailure {
    let code = match error {
        SafeFetchError::InvalidUrl => Code::InvalidUrl,
        SafeFetchError::UnsupportedScheme => Code::UnsupportedScheme,
        SafeFetchError::DnsFailed => Code::Dns,
        SafeFetchError::DnsLookup(_) => Code::Dns,
        SafeFetchError::BlockedTarget => Code::BlockedTarget,
        SafeFetchError::ClientBuild(_) => Code::ClientBuild,
        SafeFetchError::BodyTooLarge => Code::BodyTooLarge,
        SafeFetchError::BodyRead(error) => return from_reqwest(error, operation),
        SafeFetchError::Json(error) => return from_json(error, operation),
    };
    let mut failure = SourceFailure::new(code, operation);
    if let SafeFetchError::DnsLookup(error) = error {
        failure.io_kind = Some(format!("{:?}", error.kind()));
        failure.os_code = error.raw_os_error();
    }
    if matches!(error, SafeFetchError::BodyTooLarge) {
        failure.limit_bytes = Some(super::safe_fetch::MAX_RESPONSE_BODY_BYTES);
    }
    if let SafeFetchError::ClientBuild(error) = error {
        append_causes(&mut failure, error);
    }
    failure
}

pub fn response_format(response: &reqwest::Response) -> &'static str {
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(str::trim);
    match content_type {
        Some(value) if value.eq_ignore_ascii_case("application/json") => "JSON",
        Some(value) if value.eq_ignore_ascii_case("text/html") => "HTML",
        Some(value) if value.eq_ignore_ascii_case("text/plain") => "plain text",
        None => "unspecified",
        _ => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapping_does_not_destroy_code_or_leak_context() {
        let original = SourceFailure::http(503, Some("HTML"), "GET /api/states");
        let error = anyhow::Error::new(original).context("private-token-in-context");
        let failure = from_anyhow(&error, "poll");
        assert_eq!(failure.code, Code::HttpServer);
        assert_eq!(failure.http_status, Some(503));
        assert_eq!(failure.operation, "GET /api/states");
        assert!(!failure.to_string().contains("private-token"));
    }

    #[test]
    fn parse_evidence_is_specific_without_response_values() {
        let error = serde_json::from_str::<Vec<u32>>(r#"["private-token"]"#).unwrap_err();
        let failure = from_json(&error, "decode");
        assert_eq!(failure.code, Code::JsonShape);
        assert_eq!(failure.line, Some(1));
        assert!(failure.column.unwrap() > 1);
        assert!(!failure.to_string().contains("private-token"));
        let truncated = serde_json::from_str::<serde_json::Value>("{").unwrap_err();
        assert_eq!(from_json(&truncated, "decode").code, Code::JsonTruncated);
    }

    #[test]
    fn untyped_failure_is_an_explicit_instrumentation_defect() {
        let failure = from_anyhow(&anyhow::anyhow!("private-url-and-token"), "station poll");
        assert_eq!(failure.code, Code::DiagnosticMissing);
        assert!(failure.next_step.contains("bug report"));
        assert!(!failure.to_string().contains("private-url"));
    }

    #[test]
    fn reqwest_status_survives_without_the_url() {
        let response = reqwest::Response::from(
            axum::http::Response::builder()
                .status(401)
                .body(reqwest::Body::from("private-body"))
                .unwrap(),
        );
        let error = response.error_for_status().unwrap_err();
        let failure = from_reqwest(&error, "poll");
        assert_eq!(failure.code, Code::HttpUnauthorized);
        assert_eq!(failure.http_status, Some(401));
        assert!(!failure.to_string().contains("private-body"));
    }

    #[test]
    fn tls_certificate_evidence_survives_an_io_wrapper() {
        let error = std::io::Error::other(rustls::Error::InvalidCertificate(
            rustls::CertificateError::UnknownIssuer,
        ));
        let mut failure = SourceFailure::new(Code::Connection, "HA GET /api/states");
        append_causes(&mut failure, &error);
        assert_eq!(failure.causes[0].code, Code::TlsCertificate);
        assert_eq!(failure.causes[0].tls_reason, Some("unknown_issuer"));
        assert!(failure.to_string().contains("LS_TLS_CERTIFICATE"));
    }
}
