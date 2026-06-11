// POST /api/zones/photo, multipart image upload for the zone form's
// Photo URL field. The browser uploads a chosen or dropped image; the
// server validates it, writes the bytes into the configured photos
// directory under a sanitized, timestamped filename, and returns the
// relative URL (/site/photos/<filename>) that the zone form stores as
// deployment.zones.<slug>.photo_url.
//
// The photos directory itself is served as static files via
// tower_http::services::ServeDir mounted at /site/photos in main.rs,
// so the URL this endpoint returns is directly fetchable by the
// browser without any extra wiring.

use axum::{
    extract::{Multipart, State},
    http::StatusCode,
    response::Json,
    routing::post,
    Router,
};
use serde::Serialize;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Clone)]
pub struct PhotosState {
    pub dir: Arc<PathBuf>,
}

#[derive(Serialize)]
pub struct UploadResponse {
    pub url: String,
    pub filename: String,
}

pub fn router(dir: PathBuf) -> Router {
    let state = PhotosState { dir: Arc::new(dir) };
    Router::new()
        .route("/photo", post(upload_photo))
        .with_state(state)
}

/// Allow common web image formats. Browsers can render all of these
/// without extra work; SVG is excluded because it can carry script.
const ALLOWED_EXT: &[&str] = &["jpg", "jpeg", "png", "gif", "webp"];

/// Reject uploads larger than this. The form is for thumbnails, not
/// archival originals; users wanting bigger files can paste a URL.
const MAX_BYTES: usize = 10 * 1024 * 1024;

async fn upload_photo(
    State(state): State<PhotosState>,
    mut mp: Multipart,
) -> Result<Json<UploadResponse>, (StatusCode, String)> {
    while let Some(field) = mp
        .next_field()
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("multipart: {e}")))?
    {
        if field.name() != Some("file") {
            continue;
        }
        let original = field.file_name().unwrap_or("upload").to_string();
        let content_type = field.content_type().unwrap_or("").to_string();

        // Validate file extension against the allowlist.
        let ext = std::path::Path::new(&original)
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if !ALLOWED_EXT.contains(&ext.as_str()) {
            return Err((
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                format!(
                    "unsupported file type '.{ext}' (allowed: {})",
                    ALLOWED_EXT.join(", ")
                ),
            ));
        }
        // Validate browser-sent content type loosely; the extension is
        // the load-bearing check, this just rejects obvious mismatches.
        if !content_type.starts_with("image/") {
            return Err((
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                format!("content type '{content_type}' is not image/*"),
            ));
        }

        let data = field
            .bytes()
            .await
            .map_err(|e| (StatusCode::BAD_REQUEST, format!("read body: {e}")))?;
        if data.len() > MAX_BYTES {
            return Err((
                StatusCode::PAYLOAD_TOO_LARGE,
                format!(
                    "upload too large: {} bytes (max {} bytes)",
                    data.len(),
                    MAX_BYTES
                ),
            ));
        }

        // Build a safe filename. Take the original stem (alphanumeric +
        // dash + underscore only, capped at 40 chars) and append the
        // current epoch in milliseconds so concurrent uploads of files
        // with the same name don't collide.
        let stem: String = std::path::Path::new(&original)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("photo")
            .chars()
            .filter(|c| c.is_alphanumeric() || matches!(c, '-' | '_'))
            .take(40)
            .collect();
        let stem = if stem.is_empty() {
            "photo".to_string()
        } else {
            stem
        };
        let ts = chrono::Utc::now().timestamp_millis();
        let safe_name = format!("{stem}-{ts}.{ext}");

        tokio::fs::create_dir_all(state.dir.as_ref())
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("mkdir: {e}")))?;

        let target = state.dir.join(&safe_name);
        tokio::fs::write(&target, &data)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("write: {e}")))?;

        return Ok(Json(UploadResponse {
            url: format!("/site/photos/{safe_name}"),
            filename: safe_name,
        }));
    }
    Err((
        StatusCode::BAD_REQUEST,
        "no 'file' field in multipart body".into(),
    ))
}
