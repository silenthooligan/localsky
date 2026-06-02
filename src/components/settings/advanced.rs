// SettingsAdvanced. Nerd mode toggle + kiosk mode toggle + source
// freshness + rollback snapshot list + demo mode display.

use leptos::prelude::*;
use leptos::tachys::view::any_view::IntoAny;

use crate::app::NerdMode;
use crate::components::ui::{Panel, Toggle};

#[component]
pub fn SettingsAdvanced() -> impl IntoView {
    // Nerd mode is a global app-level signal provided in app.rs (see the
    // NerdMode newtype). The Toggle below mutates it directly; the
    // app-level Effect handles localStorage persistence + the
    // data-nerd attribute on <html>. If for any reason the context
    // isn't installed, fall back to a local-only signal so the page
    // still renders.
    let nerd_mode = use_context::<NerdMode>()
        .map(|n| n.0)
        .unwrap_or_else(|| RwSignal::new(false));
    let readonly = RwSignal::new(false);
    let update_check = RwSignal::new(false);
    let update_status: RwSignal<UpdateStatus> = RwSignal::new(UpdateStatus::Idle);
    let demo_mode_active = RwSignal::new(false);
    let snapshots = RwSignal::new(Vec::<SnapshotRow>::new());

    #[cfg(feature = "hydrate")]
    {
        Effect::new(move |_| {
            if let Some(win) = web_sys::window() {
                if let Ok(Some(storage)) = win.local_storage() {
                    if let Ok(Some(v)) = storage.get_item("readonly") {
                        readonly.set(v == "1" || v == "true");
                    }
                    if let Ok(Some(v)) = storage.get_item("update_check") {
                        update_check.set(v == "1" || v == "true");
                    }
                }
            }
        });
        // Persist + apply readonly on change. We also flip the
        // data-readonly attribute live so the user can see the effect
        // immediately without reloading.
        Effect::new(move |_| {
            let v = readonly.get();
            if let Some(win) = web_sys::window() {
                if let Ok(Some(storage)) = win.local_storage() {
                    let _ = storage.set_item("readonly", if v { "1" } else { "0" });
                }
                if let Some(doc) = win.document() {
                    if let Some(html) = doc.document_element() {
                        if v {
                            let _ = html.set_attribute("data-readonly", "true");
                        } else {
                            let _ = html.remove_attribute("data-readonly");
                        }
                    }
                }
            }
        });
        // Persist update_check toggle.
        Effect::new(move |_| {
            let v = update_check.get();
            if let Some(win) = web_sys::window() {
                if let Ok(Some(storage)) = win.local_storage() {
                    let _ = storage.set_item("update_check", if v { "1" } else { "0" });
                }
            }
            if v {
                wasm_bindgen_futures::spawn_local(async move {
                    update_status.set(UpdateStatus::Checking);
                    match fetch_update_status().await {
                        Ok(s) => update_status.set(s),
                        Err(e) => update_status.set(UpdateStatus::Error(e)),
                    }
                });
            } else {
                update_status.set(UpdateStatus::Idle);
            }
        });
        // Fetch current config + snapshot list on mount.
        Effect::new(move |_| {
            wasm_bindgen_futures::spawn_local(async move {
                if let Ok(demo) = fetch_demo_mode().await {
                    demo_mode_active.set(demo);
                }
            });
        });
    }

    view! {
        <main id="main-content" class="settings-page">
            <header class="settings-page__header">
                <a class="settings-page__back" href="/settings">"← Settings"</a>
                <h1 class="settings-page__title">"Advanced"</h1>
                <p class="settings-page__subtitle">
                    "Debug visibility and rollback. None of these change the "
                    "engine's behavior; they just expose what's already "
                    "happening."
                </p>
            </header>

            <Panel title="Nerd mode".to_string()>
                <Toggle
                    checked=nerd_mode
                    label="Show raw engine math everywhere".to_string()
                    helptext="When on, every irrigation panel surfaces ET0, ETc, bucket depth, Kc, MAD, available water, and root depth. Per-device, persisted to localStorage.".to_string()
                />
            </Panel>

            <Panel title="Kiosk mode".to_string()>
                <Toggle
                    checked=readonly
                    label="Hide destructive controls on this device".to_string()
                    helptext="When on, this device cannot trigger irrigation actions (run zone, stop all, threshold edits, pause toggles). Status and history stay fully visible. Useful for shared iPads, public dashboards, and family devices. Per-device, persisted to localStorage.".to_string()
                />
            </Panel>

            <Panel title="Source freshness".to_string()>
                <SourceStatusList/>
            </Panel>

            <Panel title="Update check".to_string()>
                <Toggle
                    checked=update_check
                    label="Check GitHub for new LocalSky releases".to_string()
                    helptext="Off by default. When on, this device fetches https://api.github.com/repos/silenthooligan/localsky/releases/latest at most once per 24 hours and shows the latest tag below. Disclosure: that request reveals this device's IP to GitHub. Per-device, persisted to localStorage.".to_string()
                />
                <UpdateStatusLine status=update_status/>
            </Panel>

            <Panel title="Demo mode".to_string()>
                <p class="settings-page__subtitle" style="margin: 0">
                    {move || if demo_mode_active.get() {
                        "Active. All controller actions are recorded but not fired; weather data is simulated."
                    } else {
                        "Inactive. To enable, set LOCALSKY_DEMO=1 in the container env or features.demo_mode = true in /data/localsky.toml."
                    }}
                </p>
            </Panel>

            <Panel title="Configuration history".to_string()>
                <p class="settings-page__subtitle" style="margin: 0 0 1rem">
                    "Every PUT /api/config snapshots the previous config "
                    "before writing. Roll back to any of the last 20 "
                    "versions via "
                    <code>"POST /api/config/rollback?to=&lt;version&gt;"</code>
                    "."
                </p>
                <Show
                    when=move || !snapshots.get().is_empty()
                    fallback=|| view! {
                        <p class="settings-page__subtitle" style="margin: 0">
                            "No snapshots yet. The first save records version 1."
                        </p>
                    }
                >
                    <ul class="setup-source-list">
                        {move || snapshots.get().into_iter().map(|s| view! {
                            <li>
                                <strong>{format!("v{}", s.version)}</strong>
                                " applied "
                                {format_epoch(s.applied_at_epoch)}
                                {s.note.map(|n| format!(" — {n}"))}
                            </li>
                        }).collect_view()}
                    </ul>
                </Show>
            </Panel>

            <Panel title="Raw TOML editor".to_string()>
                <p class="settings-page__subtitle" style="margin: 0 0 0.75rem">
                    "Direct edit of "
                    <code>"/data/localsky.toml"</code>
                    ". Validates on save (TOML parse + schema invariants). "
                    "Skips the wizard entirely; useful when adding sources / "
                    "controllers / zones from a template you already have. "
                    "Secrets are visible here, unlike "<code>"GET /api/config"</code>"."
                </p>
                <RawTomlEditor/>
            </Panel>
        </main>
    }
}

#[derive(Clone)]
struct SnapshotRow {
    version: u32,
    applied_at_epoch: i64,
    note: Option<String>,
}

fn format_epoch(epoch: i64) -> String {
    use chrono::{Local, TimeZone};
    match Local.timestamp_opt(epoch, 0).single() {
        Some(dt) => dt.format("%Y-%m-%d %H:%M").to_string(),
        None => format!("epoch {epoch}"),
    }
}

#[derive(Clone, Default)]
struct SourceStatusRow {
    label: &'static str,
    last_epoch: i64,
    reachable: bool,
    /// When >0, an "expected interval" hint used to classify staleness.
    /// E.g., the forecast refresher polls every 30 min, so >60 min since
    /// the last successful fetch is "stale". The HA refresher cycles
    /// every 10s, so >60s without a packet is "stale".
    stale_after_s: i64,
}

#[component]
fn SourceStatusList() -> impl IntoView {
    let rows: RwSignal<Vec<SourceStatusRow>> = RwSignal::new(default_rows());

    #[cfg(feature = "hydrate")]
    {
        Effect::new(move |_| {
            wasm_bindgen_futures::spawn_local(async move {
                if let Some(fresh) = fetch_source_status().await {
                    rows.set(fresh);
                }
            });
        });
    }

    view! {
        <ul class="source-status-list">
            {move || rows.get().into_iter().map(|r| view! { <SourceStatusRowView row=r/> }.into_any()).collect::<Vec<_>>()}
        </ul>
    }
}

#[component]
fn SourceStatusRowView(row: SourceStatusRow) -> impl IntoView {
    use chrono::Utc;
    let label = row.label;
    let age_s = if row.last_epoch > 0 {
        (Utc::now().timestamp() - row.last_epoch).max(0)
    } else {
        i64::MAX
    };
    let (status_text, status_class) = if !row.reachable {
        ("offline", "source-status-pill source-status-pill-offline")
    } else if age_s == i64::MAX {
        ("waiting", "source-status-pill source-status-pill-waiting")
    } else if row.stale_after_s > 0 && age_s > row.stale_after_s {
        ("stale", "source-status-pill source-status-pill-stale")
    } else {
        ("fresh", "source-status-pill source-status-pill-fresh")
    };
    let age_text = if age_s == i64::MAX {
        "no data yet".to_string()
    } else if age_s < 60 {
        format!("{age_s}s ago")
    } else if age_s < 3600 {
        format!("{}m ago", age_s / 60)
    } else {
        format!("{:.1}h ago", age_s as f64 / 3600.0)
    };
    view! {
        <li class="source-status-row">
            <span class="source-status-label">{label}</span>
            <span class=status_class>{status_text}</span>
            <span class="source-status-age">{age_text}</span>
        </li>
    }
}

fn default_rows() -> Vec<SourceStatusRow> {
    vec![
        SourceStatusRow {
            label: "Tempest weather station",
            last_epoch: 0,
            reachable: false,
            stale_after_s: 60,
        },
        SourceStatusRow {
            label: "Irrigation refresher",
            last_epoch: 0,
            reachable: false,
            stale_after_s: 60,
        },
        SourceStatusRow {
            label: "Open-Meteo forecast",
            last_epoch: 0,
            reachable: false,
            stale_after_s: 60 * 60,
        },
    ]
}

#[derive(Clone, Default)]
#[allow(dead_code)] // non-Idle variants are constructed only under feature = "hydrate"
enum UpdateStatus {
    #[default]
    Idle,
    Checking,
    UpToDate {
        current: String,
        latest: String,
    },
    Available {
        current: String,
        latest: String,
        url: String,
    },
    Error(String),
}

#[component]
fn UpdateStatusLine(status: RwSignal<UpdateStatus>) -> impl IntoView {
    view! {
        <div>
            {move || match status.get() {
                UpdateStatus::Idle => view! {
                    <p class="settings-page__subtitle" style="margin: 0">
                        "Enable the toggle above to check for updates."
                    </p>
                }.into_any(),
                UpdateStatus::Checking => view! {
                    <p class="settings-page__subtitle" style="margin: 0">
                        "Checking GitHub..."
                    </p>
                }.into_any(),
                UpdateStatus::UpToDate { current, latest } => view! {
                    <p class="settings-page__subtitle" style="margin: 0">
                        {format!("LocalSky v{current} is the latest release (GitHub: v{latest}).")}
                    </p>
                }.into_any(),
                UpdateStatus::Available { current, latest, url } => view! {
                    <p class="settings-page__subtitle" style="margin: 0">
                        {format!("Update available: v{latest} (running v{current}). ")}
                        <a href=url target="_blank" rel="noopener">"Release notes ->"</a>
                    </p>
                }.into_any(),
                UpdateStatus::Error(e) => view! {
                    <p class="settings-page__subtitle" style="margin: 0">
                        {format!("Update check failed: {e}")}
                    </p>
                }.into_any(),
            }}
        </div>
    }
}

#[cfg(feature = "hydrate")]
async fn fetch_update_status() -> Result<UpdateStatus, String> {
    use gloo_net::http::Request;
    use serde_json::Value;

    // Cache window: skip the network if we checked within the last 24h.
    if let Some(win) = web_sys::window() {
        if let Ok(Some(storage)) = win.local_storage() {
            if let (Ok(Some(ts)), Ok(Some(json))) = (
                storage.get_item("update_check_at"),
                storage.get_item("update_check_result"),
            ) {
                if let (Ok(ts), Some(parsed)) = (
                    ts.parse::<i64>(),
                    serde_json::from_str::<CachedUpdate>(&json).ok(),
                ) {
                    let now = chrono::Utc::now().timestamp();
                    if now - ts < 24 * 3600 {
                        return Ok(parsed.into_status());
                    }
                }
            }
        }
    }

    // Fetch current running version from /api/v1/info.
    let current = Request::get("/api/v1/info")
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json::<Value>()
        .await
        .map_err(|e| e.to_string())?;
    let current_version = current
        .get("service_version")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();

    // Fetch latest GitHub release.
    let latest =
        Request::get("https://api.github.com/repos/silenthooligan/localsky/releases/latest")
            .send()
            .await
            .map_err(|e| e.to_string())?;
    if !latest.ok() {
        return Err(format!("github returned HTTP {}", latest.status()));
    }
    let latest_json: Value = latest.json().await.map_err(|e| e.to_string())?;
    let latest_tag = latest_json
        .get("tag_name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim_start_matches('v')
        .to_string();
    let latest_url = latest_json
        .get("html_url")
        .and_then(Value::as_str)
        .unwrap_or("https://github.com/silenthooligan/localsky/releases")
        .to_string();

    let status = if latest_tag.is_empty() {
        UpdateStatus::Error("github returned no tag_name".to_string())
    } else if version_newer(&latest_tag, &current_version) {
        UpdateStatus::Available {
            current: current_version,
            latest: latest_tag,
            url: latest_url,
        }
    } else {
        UpdateStatus::UpToDate {
            current: current_version,
            latest: latest_tag,
        }
    };

    // Persist for the cache window.
    if let Some(win) = web_sys::window() {
        if let Ok(Some(storage)) = win.local_storage() {
            let _ = storage.set_item(
                "update_check_at",
                &chrono::Utc::now().timestamp().to_string(),
            );
            let cached = CachedUpdate::from_status(&status);
            if let Ok(json) = serde_json::to_string(&cached) {
                let _ = storage.set_item("update_check_result", &json);
            }
        }
    }

    Ok(status)
}

/// Lexical SemVer-ish comparison. Splits on '.' and '-' and compares
/// numeric segments numerically, falling back to string compare for
/// pre-release labels. Good enough for "is X.Y.Z newer than A.B.C".
#[cfg(feature = "hydrate")]
fn version_newer(candidate: &str, baseline: &str) -> bool {
    fn parts(s: &str) -> Vec<(u64, String)> {
        s.split(|c: char| c == '.' || c == '-')
            .map(|p| {
                let n = p
                    .chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect::<String>();
                let rest = p.chars().skip(n.len()).collect::<String>();
                (n.parse::<u64>().unwrap_or(0), rest)
            })
            .collect()
    }
    let a = parts(candidate);
    let b = parts(baseline);
    let n = a.len().max(b.len());
    for i in 0..n {
        let ai = a.get(i).cloned().unwrap_or((0, String::new()));
        let bi = b.get(i).cloned().unwrap_or((0, String::new()));
        match ai.0.cmp(&bi.0) {
            std::cmp::Ordering::Greater => return true,
            std::cmp::Ordering::Less => return false,
            std::cmp::Ordering::Equal => {}
        }
        match ai.1.cmp(&bi.1) {
            std::cmp::Ordering::Greater => return true,
            std::cmp::Ordering::Less => return false,
            std::cmp::Ordering::Equal => {}
        }
    }
    false
}

#[derive(serde::Serialize, serde::Deserialize)]
#[allow(dead_code)] // only used under feature = "hydrate"
struct CachedUpdate {
    kind: String,
    current: String,
    latest: String,
    url: String,
}

#[allow(dead_code)] // only used under feature = "hydrate"
impl CachedUpdate {
    fn from_status(s: &UpdateStatus) -> Self {
        match s {
            UpdateStatus::UpToDate { current, latest } => Self {
                kind: "up_to_date".into(),
                current: current.clone(),
                latest: latest.clone(),
                url: String::new(),
            },
            UpdateStatus::Available {
                current,
                latest,
                url,
            } => Self {
                kind: "available".into(),
                current: current.clone(),
                latest: latest.clone(),
                url: url.clone(),
            },
            _ => Self {
                kind: "idle".into(),
                current: String::new(),
                latest: String::new(),
                url: String::new(),
            },
        }
    }

    fn into_status(self) -> UpdateStatus {
        match self.kind.as_str() {
            "up_to_date" => UpdateStatus::UpToDate {
                current: self.current,
                latest: self.latest,
            },
            "available" => UpdateStatus::Available {
                current: self.current,
                latest: self.latest,
                url: self.url,
            },
            _ => UpdateStatus::Idle,
        }
    }
}

#[cfg(feature = "hydrate")]
async fn fetch_source_status() -> Option<Vec<SourceStatusRow>> {
    use gloo_net::http::Request;
    use serde_json::Value;
    async fn get_json(url: &str) -> Option<Value> {
        Request::get(url)
            .send()
            .await
            .ok()?
            .json::<Value>()
            .await
            .ok()
    }
    let (tempest, irrigation, forecast) = futures::join!(
        get_json("/api/v1/snapshot"),
        get_json("/api/v1/irrigation/snapshot"),
        get_json("/api/v1/forecast/snapshot"),
    );
    let mut rows = default_rows();
    if let Some(t) = tempest {
        let last = t
            .get("last_packet_epoch")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        rows[0].last_epoch = last;
        rows[0].reachable = last > 0;
    }
    if let Some(i) = irrigation {
        let last = i
            .get("last_refresh_epoch")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let reachable = i
            .get("ha_reachable")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        rows[1].last_epoch = last;
        rows[1].reachable = reachable;
    }
    if let Some(f) = forecast {
        let last = f
            .get("last_refresh_epoch")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let reachable = f
            .get("source_reachable")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        rows[2].last_epoch = last;
        rows[2].reachable = reachable;
    }
    Some(rows)
}

#[cfg(feature = "hydrate")]
async fn fetch_demo_mode() -> Result<bool, String> {
    use gloo_net::http::Request;
    let resp = Request::get("/api/config")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let val: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    Ok(val
        .get("features")
        .and_then(|f| f.get("demo_mode"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false))
}

// ------------------------------------------------------------
// Raw TOML editor.
//
// Loads /data/localsky.toml as plain text from GET /api/v1/config/raw,
// renders in a textarea, PUTs back as text/plain. Server-side validates
// (TOML parse + schema invariants) before writing.
// ------------------------------------------------------------

// Variants are only constructed from the hydrate branch; SSR build sees
// them as dead because the Effect + spawn_local block is cfg-gated.
#[derive(Clone, PartialEq)]
#[cfg_attr(not(feature = "hydrate"), allow(dead_code))]
enum RawSaveState {
    Idle,
    Loading,
    Saving,
    Saved,
    Error(String),
}

#[component]
fn RawTomlEditor() -> impl IntoView {
    let text = RwSignal::new(String::new());
    let state: RwSignal<RawSaveState> = RwSignal::new(RawSaveState::Idle);

    #[cfg(feature = "hydrate")]
    {
        // Initial load on mount.
        Effect::new(move |_| {
            state.set(RawSaveState::Loading);
            wasm_bindgen_futures::spawn_local(async move {
                match fetch_raw_toml().await {
                    Ok(s) => {
                        text.set(s);
                        state.set(RawSaveState::Idle);
                    }
                    Err(e) => state.set(RawSaveState::Error(format!("load: {e}"))),
                }
            });
        });
    }

    let on_save = move |_| {
        let body = text.get_untracked();
        state.set(RawSaveState::Saving);
        #[cfg(feature = "hydrate")]
        wasm_bindgen_futures::spawn_local(async move {
            match save_raw_toml(body).await {
                Ok(_) => state.set(RawSaveState::Saved),
                Err(e) => state.set(RawSaveState::Error(e)),
            }
        });
        let _ = body;
    };

    let status_class = move || match state.get() {
        RawSaveState::Saved => "raw-toml-status is-ok",
        RawSaveState::Error(_) => "raw-toml-status is-err",
        _ => "raw-toml-status",
    };
    let status_text = move || match state.get() {
        RawSaveState::Idle => String::new(),
        RawSaveState::Loading => "loading…".to_string(),
        RawSaveState::Saving => "saving…".to_string(),
        RawSaveState::Saved => {
            "saved. Container will load the new config on next restart.".to_string()
        }
        RawSaveState::Error(e) => format!("error: {e}"),
    };

    view! {
        <textarea
            class="raw-toml-textarea"
            spellcheck="false"
            autocomplete="off"
            on:input=move |ev| {
                let v = event_target_value(&ev);
                text.set(v);
            }
            prop:value=move || text.get()
        />
        <div class="raw-toml-actions">
            <button
                class="btn btn-primary"
                on:click=on_save
                disabled=move || matches!(state.get(), RawSaveState::Saving | RawSaveState::Loading)
            >
                "Save"
            </button>
            <span class=status_class>{status_text}</span>
        </div>
    }
}

#[cfg(feature = "hydrate")]
async fn fetch_raw_toml() -> Result<String, String> {
    let resp = gloo_net::http::Request::get("/api/v1/config/raw")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    resp.text().await.map_err(|e| e.to_string())
}

#[cfg(feature = "hydrate")]
async fn save_raw_toml(body: String) -> Result<(), String> {
    let resp = gloo_net::http::Request::put("/api/v1/config/raw")
        .header("Content-Type", "text/plain; charset=utf-8")
        .body(body)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if resp.ok() {
        Ok(())
    } else {
        let detail = resp.text().await.unwrap_or_default();
        Err(format!("{} {}", resp.status(), detail))
    }
}
