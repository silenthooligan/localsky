// The wizard's shared draft client. A step edits its own fetched baseline;
// saves are serialized and apply only that step's changes to the latest draft.

#[derive(Clone, Default)]
pub struct SaveStatus {
    pub pending: u32,
    pub error: Option<String>,
    #[cfg(feature = "hydrate")]
    failures: std::collections::BTreeMap<u64, String>,
}

thread_local! {
    static STATUS: leptos::prelude::ArcRwSignal<SaveStatus> =
        leptos::prelude::ArcRwSignal::new(SaveStatus::default());
}

pub fn status() -> leptos::prelude::ArcRwSignal<SaveStatus> {
    STATUS.with(Clone::clone)
}

#[cfg(any(feature = "hydrate", test))]
const SESSION_KEY: &str = "_localsky_edit_session";

#[cfg(feature = "hydrate")]
#[derive(Default)]
struct DraftClient {
    next_session: u64,
    baselines: std::collections::HashMap<u64, serde_json::Value>,
}

#[cfg(feature = "hydrate")]
thread_local! {
    static CLIENT: std::rc::Rc<futures::lock::Mutex<DraftClient>> =
        std::rc::Rc::new(futures::lock::Mutex::new(DraftClient::default()));
}

#[cfg(feature = "hydrate")]
async fn fetch_remote() -> Option<serde_json::Value> {
    let resp = gloo_net::http::Request::get("/api/wizard/draft")
        .send()
        .await
        .ok()?;
    if !resp.ok() {
        return None;
    }
    resp.json::<serde_json::Value>().await.ok()
}

/// Fetch after earlier saves finish, so advancing steps cannot load an older
/// version of this browser's draft. The editing-session key stays client-only.
#[cfg(feature = "hydrate")]
pub async fn fetch() -> Option<serde_json::Value> {
    let client = CLIENT.with(Clone::clone);
    let mut client = client.lock().await;
    let mut draft = fetch_remote().await?;
    client.next_session += 1;
    let session = client.next_session;
    client.baselines.insert(session, draft.clone());
    draft
        .as_object_mut()?
        .insert(SESSION_KEY.into(), session.into());
    Some(draft)
}

/// Save the fields edited by this step, preserving unrelated changes and the
/// server's current concurrency token. Overlapping edits still report a conflict.
#[cfg(feature = "hydrate")]
pub async fn save(draft: &serde_json::Value) -> Result<(), String> {
    use leptos::prelude::Update;
    let status = status();
    status.update(|state| state.pending += 1);
    let session = draft
        .get(SESSION_KEY)
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let result = save_inner(draft).await;
    status.update(|state| {
        state.pending -= 1;
        match &result {
            Ok(()) => {
                state.failures.remove(&session);
            }
            Err(error) => {
                state.failures.insert(session, error.clone());
            }
        }
        // A successful unrelated save must not hide another step's failure.
        state.error = state.failures.values().next().cloned();
    });
    result
}

#[cfg(feature = "hydrate")]
async fn save_inner(draft: &serde_json::Value) -> Result<(), String> {
    let client = CLIENT.with(Clone::clone);
    let mut client = client.lock().await;
    let session = draft.get(SESSION_KEY).and_then(serde_json::Value::as_u64);
    let mut edited = draft.clone();
    if let Some(object) = edited.as_object_mut() {
        object.remove(SESSION_KEY);
    }
    let candidate = if let Some(session) = session {
        let baseline = client
            .baselines
            .get(&session)
            .ok_or_else(|| "This setup draft needs to be reloaded before saving.".to_string())?;
        let mut current = fetch_remote()
            .await
            .ok_or_else(|| "Could not load your saved setup. Please try again.".to_string())?;
        merge_changes(baseline, &edited, &mut current, "")?;
        current
    } else {
        // Compatibility for a caller creating a document without fetching it.
        edited.clone()
    };
    let resp = gloo_net::http::Request::put("/api/wizard/draft")
        .json(&candidate)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        let body = resp.text().await.unwrap_or_default();
        return Err(crate::components::settings_ui::save_error_message(
            resp.status(),
            &body,
        ));
    }
    if let Some(session) = session {
        client.baselines.insert(session, edited);
    }
    Ok(())
}

#[cfg(any(feature = "hydrate", test))]
fn merge_changes(
    baseline: &serde_json::Value,
    edited: &serde_json::Value,
    current: &mut serde_json::Value,
    path: &str,
) -> Result<(), String> {
    if baseline == edited {
        return Ok(());
    }
    if let (Some(base), Some(next), Some(live)) = (
        baseline.as_object(),
        edited.as_object(),
        current.as_object_mut(),
    ) {
        let keys: std::collections::BTreeSet<_> = base.keys().chain(next.keys()).collect();
        for key in keys {
            if path.is_empty() && matches!(key.as_str(), "last_updated_epoch" | SESSION_KEY) {
                continue;
            }
            if base.get(key) == next.get(key) {
                continue;
            }
            let field = format!("{path}/{key}");
            match (base.get(key), next.get(key), live.get_mut(key)) {
                (Some(before), Some(after), Some(value)) => merge_changes(before, after, value, &field)?,
                (None, Some(after), None) => { live.insert(key.clone(), after.clone()); }
                (None, Some(after), Some(value)) if *value == *after => {}
                (Some(before), None, Some(value)) if *value == *before => { live.remove(key); }
                (Some(_), None, None) => {}
                _ => return Err(format!("Your setup changed in another editor ({field}). Reload this step before saving.")),
            }
        }
        return Ok(());
    }
    if *current == *baseline || *current == *edited {
        *current = edited.clone();
        Ok(())
    } else {
        Err(format!(
            "Your setup changed in another editor ({path}). Reload this step before saving."
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::merge_changes;
    use serde_json::json;

    #[test]
    fn repeated_location_edits_keep_the_server_token_and_other_steps() {
        let first = json!({"last_updated_epoch": 1, "config": {
            "deployment": {"location": {"lat": 0.0, "lon": 0.0}, "timezone": null},
            "zones": {}}});
        let mut local = first.clone();
        local["config"]["deployment"]["location"]["lat"] = json!(29.65);
        let mut saved = first.clone();
        merge_changes(&first, &local, &mut saved, "").unwrap();
        saved["last_updated_epoch"] = json!(2);
        saved["config"]["zones"] = json!({"lawn": {"display_name": "Lawn"}});
        let baseline = local.clone();
        local["config"]["deployment"]["location"]["lon"] = json!(-82.32);
        local["config"]["deployment"]["timezone"] = json!("America/New_York");
        merge_changes(&baseline, &local, &mut saved, "").unwrap();
        assert_eq!(saved["last_updated_epoch"], 2);
        assert_eq!(saved["config"]["zones"]["lawn"]["display_name"], "Lawn");
        assert_eq!(saved["config"]["deployment"], local["config"]["deployment"]);
    }

    #[test]
    fn overlapping_edits_conflict_without_mutating_that_field() {
        let baseline = json!({"config": {"sources": ["one"]}});
        let edited = json!({"config": {"sources": ["one", "mine"]}});
        let mut current = json!({"config": {"sources": ["one", "theirs"]}});
        let before = current.clone();
        assert!(merge_changes(&baseline, &edited, &mut current, "").is_err());
        assert_eq!(current, before);
    }
}
