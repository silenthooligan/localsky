//! One request correlation and error envelope at the HTTP boundary.
use crate::failure::{Failure, FailureCode, FailureRecord};
use axum::{
    body::to_bytes,
    extract::{MatchedPath, Request},
    http::{header, HeaderValue},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use std::sync::atomic::{AtomicU64, Ordering};
use tracing::Instrument;
static SEQUENCE: AtomicU64 = AtomicU64::new(1);

pub async fn envelope(req: Request, next: Next) -> Response {
    if !req.uri().path().starts_with("/api/") {
        return next.run(req).await;
    }
    let request_id = format!(
        "{:x}-{:x}-{:x}",
        chrono::Utc::now().timestamp_millis(),
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    let method = req.method().to_string();
    // Matched route patterns contain no query strings, credentials or path values.
    let route = req
        .extensions()
        .get::<MatchedPath>()
        .map(|p| p.as_str().to_owned())
        .unwrap_or_else(|| "unmatched API route".into());
    let span = tracing::info_span!("api_request", %request_id, %method, %route);
    let mut response = next.run(req).instrument(span).await;
    response.headers_mut().insert(
        "x-localsky-request-id",
        HeaderValue::from_str(&request_id).expect("generated ASCII request ID"),
    );
    let status = response.status();
    if !(status.is_client_error() || status.is_server_error()) {
        return response;
    }
    let (mut parts, body) = response.into_parts();
    let bytes = to_bytes(body, 64 * 1024).await;
    let mut value = match bytes {
        Ok(bytes) => serde_json::from_slice::<serde_json::Value>(&bytes)
            .ok()
            .filter(|v| v.is_object())
            .unwrap_or_else(
                || serde_json::json!({"error": String::from_utf8_lossy(&bytes).trim()}),
            ),
        Err(_) => {
            serde_json::json!({"error": "Error response exceeded the 64 KiB diagnostic limit"})
        }
    };
    if value.get("diagnostic").is_none_or(|v| v.is_null()) {
        let mut failure = Failure::new(
            if status.is_server_error() {
                FailureCode::ApiServer
            } else {
                FailureCode::ApiRejected
            },
            "LocalSky HTTP API",
        );
        failure.http_status = Some(status.as_u16());
        value["diagnostic"] =
            serde_json::to_value(FailureRecord::now(failure)).expect("serializable diagnostic");
    }
    value["request"] = serde_json::json!({"id": request_id, "method": method, "route": route});
    tracing::warn!(%request_id, %method, %route, status = status.as_u16(), code = value.pointer("/diagnostic/failure/code").and_then(|v| v.as_str()).unwrap_or("unknown"), "API request failed");
    parts.headers.remove(header::CONTENT_LENGTH);
    parts.headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    let (_, body) = Json(value).into_response().into_parts();
    Response::from_parts(parts, body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::StatusCode, routing::post, Router};
    use tower::ServiceExt;
    #[tokio::test]
    async fn malformed_request_has_code_route_and_matching_correlation_header() {
        let app = Router::new()
            .route(
                "/api/test",
                post(|Json(_): Json<serde_json::Value>| async { StatusCode::OK }),
            )
            .layer(axum::middleware::from_fn(envelope));
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/test?token=private")
                    .header("content-type", "application/json")
                    .body(Body::from("{"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let id = response.headers()["x-localsky-request-id"]
            .to_str()
            .unwrap()
            .to_owned();
        let value: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 65536).await.unwrap()).unwrap();
        assert_eq!(value["request"]["id"], id);
        assert_eq!(value["request"]["route"], "/api/test");
        assert_eq!(value["diagnostic"]["failure"]["code"], "LS_API_REJECTED");
        assert!(!value.to_string().contains("token=private"));
    }
    #[tokio::test]
    async fn boundary_retains_upstream_cause_and_auth_headers() {
        let app = Router::new().route("/api/test", post(|| async {
            (StatusCode::FAILED_DEPENDENCY, [(header::WWW_AUTHENTICATE, "test")], Json(serde_json::json!({"error":"controller rejected credential", "diagnostic": FailureRecord::now(Failure::http(403, None, "controller status"))})))
        })).layer(axum::middleware::from_fn(envelope));
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/test")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FAILED_DEPENDENCY);
        assert_eq!(response.headers()[header::WWW_AUTHENTICATE], "test");
        let value: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 65536).await.unwrap()).unwrap();
        assert_eq!(value["diagnostic"]["failure"]["http_status"], 403);
        assert_eq!(
            value["diagnostic"]["failure"]["operation"],
            "controller status"
        );
    }
}
