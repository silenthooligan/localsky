// The browser's one client for the config document. Every settings page
// used to carry its own fetch_config / save_config pair (about thirty
// copies), and they disagreed on the details that matter: whether a
// non-2xx JSON error body was mistaken for the config, whether the PUT
// response's restart reasons were read, how an error was worded. These
// two functions are the whole surface.

/// What a successful save reports back: the restart reasons the server
/// attached when the change needs a boot-wired connection rebuilt (a new
/// listener, a new poll loop). Empty means the change hot-reloaded.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SaveOutcome {
    pub restart_reasons: Vec<String>,
}

impl SaveOutcome {
    pub fn restart_required(&self) -> bool {
        !self.restart_reasons.is_empty()
    }

    pub fn save_confirmation(&self) -> &'static str {
        if self.restart_required() {
            crate::voice::SAVED_NEEDS_RESTART
        } else {
            crate::voice::SAVED_LIVE
        }
    }

    /// Keep the save confirmation consistent with the server's source-wiring
    /// outcome. Saving config does not necessarily activate a connection.
    pub fn confirmation(&self, action: &str) -> String {
        if self.restart_required() {
            format!("{action} Restart LocalSky to finish applying this change.")
        } else {
            action.to_string()
        }
    }
}

/// Read `restart_required` / `restart_reasons` off a PUT or PATCH response
/// body. A missing or old field reads as "no restart", the safe default.
pub fn outcome_from_body(body: &serde_json::Value) -> SaveOutcome {
    let required = body
        .get("restart_required")
        .and_then(|r| r.as_bool())
        .unwrap_or(false);
    if !required {
        return SaveOutcome::default();
    }
    SaveOutcome {
        restart_reasons: body
            .get("restart_reasons")
            .and_then(|r| r.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
    }
}

/// GET /api/config as JSON. A non-2xx answer is an error even when its
/// body is valid JSON: an auth or error body must never poison the config
/// signal (the failure-path refetch would then "restore" garbage).
#[cfg(feature = "hydrate")]
pub async fn get_config() -> Result<serde_json::Value, String> {
    use gloo_net::http::Request;
    let resp = Request::get("/api/config").send().await.map_err(|_| {
        crate::components::request_error::RequestError::network("configuration request").to_string()
    })?;
    if !resp.ok() {
        let body = resp.text().await.unwrap_or_default();
        return Err(crate::components::settings_ui::load_error_message(
            resp.status(),
            &body,
        ));
    }
    resp.json::<serde_json::Value>()
        .await
        .map_err(|e| e.to_string())
}

/// PUT the whole config document.
#[cfg(feature = "hydrate")]
pub async fn put_config(cfg: &serde_json::Value) -> Result<SaveOutcome, String> {
    use gloo_net::http::Request;
    let req = Request::put("/api/config").json(cfg).map_err(|_| {
        crate::components::request_error::RequestError::network("configuration request").to_string()
    })?;
    finish_save(req).await
}

#[cfg(feature = "hydrate")]
async fn finish_save(req: gloo_net::http::Request) -> Result<SaveOutcome, String> {
    let resp = req.send().await.map_err(|_| {
        crate::components::request_error::RequestError::network("configuration request").to_string()
    })?;
    if !resp.ok() {
        let body = resp.text().await.unwrap_or_default();
        return Err(crate::components::settings_ui::save_error_message(
            resp.status(),
            &body,
        ));
    }
    Ok(resp
        .json::<serde_json::Value>()
        .await
        .map(|b| outcome_from_body(&b))
        .unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// The config document and the wizard draft have one client each;
    /// no page carries its own fetch/save pair or talks to the endpoint
    /// directly.
    #[test]
    fn no_component_talks_to_the_config_or_draft_endpoint_on_its_own() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/components");
        let mut files = Vec::new();
        walk(&root, &mut files);
        let mut offenders = Vec::new();
        for f in files {
            let name = f.file_name().unwrap().to_string_lossy().to_string();
            let owner = f.ends_with("components/config_client.rs") || f.ends_with("setup/draft.rs");
            if owner {
                continue;
            }
            let src = std::fs::read_to_string(&f).unwrap();
            let code = crate::engine::clock::code_only(src.split("#[cfg(test)]").next().unwrap());
            for needle in [
                "fn fetch_config(",
                "fn save_config(",
                "fn load_config(",
                "fn fetch_draft(",
                "fn save_draft(",
                "fn fetch_draft_value(",
                "Request::get(\"/api/config\")",
                "Request::put(\"/api/config\")",
                "Request::get(\"/api/wizard/draft\")",
                "Request::put(\"/api/wizard/draft\")",
            ] {
                if code.contains(needle) {
                    offenders.push(format!("{name}: {needle}"));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "use config_client / setup::draft:
{}",
            offenders.join(
                "
"
            )
        );
    }

    #[test]
    fn restart_reasons_are_read_only_when_required() {
        let none = outcome_from_body(&serde_json::json!({ "ok": true }));
        assert!(!none.restart_required());
        let off = outcome_from_body(&serde_json::json!({
            "restart_required": false, "restart_reasons": ["ignored"]
        }));
        assert!(off.restart_reasons.is_empty());
        let on = outcome_from_body(&serde_json::json!({
            "restart_required": true, "restart_reasons": ["tempest listener", 7, "poll loop"]
        }));
        assert_eq!(on.restart_reasons, vec!["tempest listener", "poll loop"]);
        assert!(on.restart_required());
    }
}
