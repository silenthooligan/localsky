//! Browser presentation of the server-owned diagnostic record.
use std::fmt;
#[derive(Clone, Debug)]
pub struct RequestError {
    pub message: String,
    pub diagnostic: Option<serde_json::Value>,
}
impl fmt::Display for RequestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl RequestError {
    pub fn response(status: u16, body: &str) -> Self {
        let value = serde_json::from_str::<serde_json::Value>(body).ok();
        let diagnostic = value
            .as_ref()
            .and_then(|v| v.get("diagnostic"))
            .filter(|v| v.is_object())
            .cloned()
            .map(|mut record| {
                if let Some(request) = value.as_ref().and_then(|v| v.get("request")) {
                    record["request"] = request.clone();
                }
                for key in ["controller_id", "zone"] {
                    if let Some(context) = value.as_ref().and_then(|v| v.get(key)) {
                        record[key] = context.clone();
                    }
                }
                record
            });
        Self {
            message: crate::components::settings_ui::load_error_message(status, body),
            diagnostic,
        }
    }
    pub fn network(operation: &'static str) -> Self {
        let failure =
            crate::failure::Failure::new(crate::failure::FailureCode::BrowserNetwork, operation);
        Self {
            message: format!("{}: {}", failure.code.as_str(), failure.message),
            diagnostic: Some(
                serde_json::json!({"at_epoch": chrono::Utc::now().timestamp(), "origin": "browser", "failure": failure}),
            ),
        }
    }
    pub fn show(&self, toast: crate::components::ui::ToastHub, prefix: &str) {
        toast.error_details(
            format!("{prefix}: {}", self.message),
            self.diagnostic.clone(),
        );
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn presentation_keeps_server_record_and_request_identity() {
        let error = super::RequestError::response(
            502,
            r#"{"error":"controller unavailable","diagnostic":{"at_epoch":123,"failure":{"code":"LS_HTTP_SERVER","http_status":503}},"request":{"id":"abc","route":"/api/irrigation/action"}}"#,
        );
        let record = error.diagnostic.unwrap();
        assert_eq!(record["failure"]["http_status"], 503);
        assert_eq!(record["request"]["id"], "abc");
        assert_eq!(record["at_epoch"], 123);
    }
}
