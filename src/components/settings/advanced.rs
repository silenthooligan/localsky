// SettingsAdvanced. Nerd mode toggle + kiosk mode toggle + source
// freshness + rollback snapshot list + demo mode display.

use leptos::prelude::*;
use leptos::tachys::view::any_view::IntoAny;

use crate::app::NerdMode;
use crate::components::ui::{HelpHint, Panel, Toggle};

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
    let restore_msg = RwSignal::new(String::new());

    // Restore upload: POST the picked bundle as multipart to
    // /api/v1/backup/restore and surface the server's note.
    let on_restore_file = move |ev: leptos::ev::Event| {
        #[cfg(feature = "hydrate")]
        {
            use wasm_bindgen::JsCast;
            let Some(input) = ev
                .target()
                .and_then(|t| t.dyn_into::<web_sys::HtmlInputElement>().ok())
            else {
                return;
            };
            let Some(file) = input.files().and_then(|f| f.item(0)) else {
                return;
            };
            // Destructive: a restore replaces the configuration and the
            // full history database at the next container restart, so the
            // file pick alone must not trigger it.
            let confirmed = web_sys::window()
                .map(|w| {
                    w.confirm_with_message(
                        "Restore from this bundle? It replaces the current \
                         configuration and history database at the next \
                         container restart.",
                    )
                    .unwrap_or(false)
                })
                .unwrap_or(false);
            if !confirmed {
                // Clear the picker so re-selecting the same file fires
                // another change event.
                input.set_value("");
                return;
            }
            restore_msg.set("Uploading…".into());
            wasm_bindgen_futures::spawn_local(async move {
                let form = web_sys::FormData::new().ok();
                let Some(form) = form else {
                    restore_msg.set("FormData unavailable".into());
                    return;
                };
                let _ = form.append_with_blob_and_filename("bundle", &file, &file.name());
                let result = async {
                    let resp = gloo_net::http::Request::post("/api/v1/backup/restore")
                        .body(form)
                        .map_err(|e| e.to_string())?
                        .send()
                        .await
                        .map_err(|e| e.to_string())?;
                    let v = resp
                        .json::<serde_json::Value>()
                        .await
                        .map_err(|e| e.to_string())?;
                    if resp.ok() {
                        Ok(v.get("note")
                            .and_then(|n| n.as_str())
                            .unwrap_or("restored")
                            .to_string())
                    } else {
                        Err(v
                            .get("error")
                            .and_then(|e| e.as_str())
                            .unwrap_or("restore failed")
                            .to_string())
                    }
                }
                .await;
                match result {
                    Ok(note) => restore_msg.set(format!("Restore accepted: {note}")),
                    Err(e) => restore_msg.set(format!("Restore failed: {e}")),
                }
            });
        }
        #[cfg(not(feature = "hydrate"))]
        let _ = ev;
    };

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
        <div class="settings-page">
            <header class="settings-page__header">
                <a class="settings-page__back" href="/settings">"← Settings"</a>
                <h1 class="settings-page__title">"Advanced"<HelpHint topic="advanced"/></h1>
                <p class="settings-page__subtitle">
                    "Per-device preferences and deployment maintenance. The "
                    <strong>"This device"</strong>" options are local and harmless; the "
                    <strong>"Maintenance & danger"</strong>" tools below change server-side "
                    "config and can affect your whole deployment."
                </p>
            </header>

            <section class="settings-band">
                <header class="settings-band__head">
                    <h2 class="settings-band__title">"This device"</h2>
                    <p class="settings-band__sub">
                        "Per-browser preferences, stored locally. Safe to toggle."
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
                    label="Check for new LocalSky releases".to_string()
                    helptext="Off by default. When on, this device checks https://localsky.io/latest.json at most once per 24 hours and shows the latest version below. Disclosure: that request reveals this device's IP to the localsky.io server; no other data is sent. Per-device, persisted to localStorage.".to_string()
                />
                <UpdateStatusLine status=update_status/>
            </Panel>
            </section>

            <section class="settings-band settings-band--danger">
                <header class="settings-band__head">
                    <h2 class="settings-band__title">"Maintenance & danger"</h2>
                    <p class="settings-band__sub">
                        "These change server-side config. Backup, restore, raw editing, and "
                        "rollback affect the whole deployment, not just this device."
                    </p>
                </header>

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
                    "Every saved change snapshots the previous config first. "
                    "Roll back to any of the last 20 versions using the list below."
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
                                {s.note.map(|n| format!(", {n}"))}
                            </li>
                        }).collect_view()}
                    </ul>
                </Show>
            </Panel>

            <Panel title="Backup and restore".to_string()>
                <p class="settings-page__subtitle" style="margin: 0 0 0.75rem">
                    "One bundle holds the config and the full history database "
                    "(runs, sensor readings, decisions). The VAPID push key and "
                    "instance identity stay out of it on purpose. Restoring a "
                    "database requires a container restart; a config restore "
                    "applies on the next engine tick."
                </p>
                <div class="settings-form-actions" style="justify-content:flex-start; gap: var(--space-2)">
                    <a class="setup-footer__btn setup-footer__btn--primary" href="/api/v1/backup" download>
                        "Download backup"
                    </a>
                    <label class="setup-footer__btn setup-footer__btn--ghost" style="cursor:pointer">
                        "Restore from bundle…"
                        <input
                            type="file"
                            accept=".tar.gz,.tgz,application/gzip"
                            style="display:none"
                            on:change=on_restore_file
                        />
                    </label>
                </div>
                {move || {
                    let m = restore_msg.get();
                    (!m.is_empty()).then(|| view! {
                        <p class="settings-page__subtitle" style="margin: 0.5rem 0 0">{m}</p>
                    })
                }}
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
            </section>
        </div>
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

/// One freshness row, now driven off the CONFIGURED sources reported by
/// /api/v1/health rather than three hardcoded names. `label` is the source's
/// own id + a friendly kind label, so a Davis / NWS / Open-Meteo-only user sees
/// their actual sources instead of a phantom "Tempest weather station" row.
/// `status` is the server-computed classification ("fresh" | "stale" |
/// "offline"); `last_epoch` is its last-seen epoch (0 = never) for the age text.
#[derive(Clone, Default)]
struct SourceStatusRow {
    label: String,
    last_epoch: i64,
    /// Server-computed freshness: "fresh" | "stale" | "offline". Empty before
    /// the first health fetch lands (the loading state).
    status: String,
}

#[component]
fn SourceStatusList() -> impl IntoView {
    // Empty until the first /api/v1/health fetch lands; SSR + first frame show
    // the "loading" caption so the DOM matches before hydration.
    let rows: RwSignal<Vec<SourceStatusRow>> = RwSignal::new(Vec::new());
    let loaded = RwSignal::new(false);

    #[cfg(feature = "hydrate")]
    {
        Effect::new(move |_| {
            wasm_bindgen_futures::spawn_local(async move {
                if let Some(fresh) = fetch_source_status().await {
                    rows.set(fresh);
                }
                loaded.set(true);
            });
        });
    }

    view! {
        <Show
            when=move || !rows.get().is_empty()
            fallback=move || {
                let msg = if loaded.get() {
                    "No weather sources are configured yet. Add one under Devices."
                } else {
                    "Loading source freshness…"
                };
                view! { <p class="settings-page__subtitle" style="margin: 0">{msg}</p> }
            }
        >
            <ul class="source-status-list">
                {move || rows.get().into_iter().map(|r| view! { <SourceStatusRowView row=r/> }.into_any()).collect::<Vec<_>>()}
            </ul>
        </Show>
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
    // Trust the server's classification (it knows each kind's expected cadence)
    // rather than re-deriving a stale threshold client-side. A never-seen source
    // reads as "waiting" instead of the raw "offline" so a just-added source
    // does not look broken before its first reading.
    let (status_text, status_class) = match row.status.as_str() {
        "fresh" => ("fresh", "source-status-pill source-status-pill-fresh"),
        "stale" => ("stale", "source-status-pill source-status-pill-stale"),
        _ if age_s == i64::MAX => ("waiting", "source-status-pill source-status-pill-waiting"),
        _ => ("offline", "source-status-pill source-status-pill-offline"),
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

    // Fetch the latest published version from the project manifest.
    let latest = Request::get("https://localsky.io/latest.json")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !latest.ok() {
        return Err(format!("update server returned HTTP {}", latest.status()));
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
        UpdateStatus::Error("update server returned no version".to_string())
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
        s.split(['.', '-'])
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

/// Build the freshness rows off the CONFIGURED sources reported by
/// /api/v1/health (its `sources` array, one entry per configured source with a
/// server-computed `status`, `last_seen_epoch`, and `kind`). This replaces the
/// old three-hardcoded-rows approach so a Davis / NWS / Open-Meteo-only user
/// sees their actual sources, never a phantom "Tempest weather station" or a
/// mislabeled "Open-Meteo forecast" row. Each row is labeled by the source id
/// plus a friendly kind label. Returns None on a transport failure (the panel
/// keeps its loading/empty state); Some(empty) when health reports no sources.
#[cfg(feature = "hydrate")]
async fn fetch_source_status() -> Option<Vec<SourceStatusRow>> {
    use gloo_net::http::Request;
    use serde_json::Value;
    let health = Request::get("/api/v1/health")
        .send()
        .await
        .ok()?
        .json::<Value>()
        .await
        .ok()?;
    let sources = health.get("sources").and_then(Value::as_array)?;
    let rows = sources
        .iter()
        .map(|s| {
            let id = s
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let kind = s.get("kind").and_then(Value::as_str).unwrap_or("");
            // "id (Friendly kind)", e.g. "tempest_lan (Tempest UDP (LAN))". The
            // kind label is the same one the source forms use, so an unknown
            // kind reads "Unknown" rather than a raw slug.
            let pretty = crate::components::sources_form::kind_pretty(kind);
            let label = if pretty == "Unknown" || id.is_empty() {
                if id.is_empty() {
                    pretty.to_string()
                } else {
                    id
                }
            } else {
                format!("{id} ({pretty})")
            };
            SourceStatusRow {
                label,
                last_epoch: s
                    .get("last_seen_epoch")
                    .and_then(Value::as_i64)
                    .unwrap_or(0),
                status: s
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            }
        })
        .collect();
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
        // Toast handle captured at component scope; the load task runs
        // detached, where context lookup isn't reliable.
        let toast = crate::components::ui::use_toast();
        // Initial load on mount.
        Effect::new(move |_| {
            state.set(RawSaveState::Loading);
            wasm_bindgen_futures::spawn_local(async move {
                match fetch_raw_toml().await {
                    Ok(s) => {
                        text.set(s);
                        state.set(RawSaveState::Idle);
                    }
                    Err(e) => {
                        state.set(RawSaveState::Error(format!("load: {e}")));
                        toast.error(format!("Couldn't load localsky.toml: {e}"));
                    }
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
    // An error body (auth page, JSON error) must not land in the
    // textarea, where saving it back would clobber the real config.
    if !resp.ok() {
        let detail = resp.text().await.unwrap_or_default();
        return Err(format!("{} {}", resp.status(), detail));
    }
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
