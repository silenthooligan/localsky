// LocationStep. Lat / lon / elevation / timezone, persisted into the
// wizard draft on every commit (load on mount, save on change). An
// address search drives the existing Nominatim proxy
// (GET /api/wizard/geocode?q=) so nobody has to know their coordinates,
// and the timezone autofills from the offline tzf dataset
// (GET /api/v1/location/timezone) whenever lat/lon change and the field
// is still empty.

use leptos::prelude::*;

use crate::components::setup::shell::{next_step_href, prev_step_href, SetupFooter};
use crate::components::ui::{Button, FormField, HelpHint};
use crate::components::units_fmt::use_unit_prefs;

/// Meters per foot. Elevation is stored in meters (Open-Meteo and the
/// elevation_m config field are both meters) but shown/entered in the user's
/// selected length unit, so imperial users never type or read a metric value.
const M_PER_FT: f64 = 0.3048;

#[cfg(feature = "hydrate")]
async fn fetch_draft() -> Option<serde_json::Value> {
    let resp = gloo_net::http::Request::get("/api/wizard/draft")
        .send()
        .await
        .ok()?;
    resp.json::<serde_json::Value>().await.ok()
}

#[cfg(feature = "hydrate")]
async fn save_draft(draft: serde_json::Value) -> Result<(), String> {
    let resp = gloo_net::http::Request::put("/api/wizard/draft")
        .json(&draft)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("HTTP {}", resp.status()));
    }
    Ok(())
}

#[component]
pub fn LocationStep() -> impl IntoView {
    // Option-backed so an unset field renders as an EMPTY placeholder, not a
    // real-looking literal "0" (which read as a filled-in value for the single
    // mandatory field). None => placeholder; Some(v) => the entered value.
    let lat = RwSignal::new(Option::<f64>::None);
    let lon = RwSignal::new(Option::<f64>::None);
    // Elevation is stored in METERS (config + Open-Meteo are meters) but shown
    // and entered in the user's selected length unit. None => empty placeholder.
    let elevation_m = RwSignal::new(Option::<f64>::None);
    // True once the user has typed in the elevation field, so the
    // location-driven auto-fill stops clobbering their manual value.
    let elevation_user_edited = RwSignal::new(false);
    let tz = RwSignal::new(String::new());

    // Length-unit preference: imperial shows/accepts feet, metric meters. The
    // elevation field always stores meters internally regardless.
    let prefs = use_unit_prefs();
    let elev_metric = move || prefs.get().distance_metric;
    let elev_unit = move || if elev_metric() { "m" } else { "ft" };
    // Meters (stored) -> the displayed unit, and back.
    let m_to_display = move |m: f64| if elev_metric() { m } else { m / M_PER_FT };
    let display_to_m = move |v: f64| if elev_metric() { v } else { v * M_PER_FT };

    let draft = RwSignal::new(serde_json::Value::Null);
    let loaded = RwSignal::new(false);

    // Address search state.
    let query = RwSignal::new(String::new());
    let searching = RwSignal::new(false);
    let results: RwSignal<Vec<(String, f64, f64)>> = RwSignal::new(Vec::new());

    #[cfg(feature = "hydrate")]
    Effect::new(move |_| {
        leptos::task::spawn_local(async move {
            if let Some(d) = fetch_draft().await {
                if let Some(loc) = d
                    .get("config")
                    .and_then(|c| c.get("deployment"))
                    .and_then(|dep| dep.get("location"))
                {
                    // Treat a persisted 0,0 (the null-island default a blank
                    // draft carries) as "unset" so the field shows its empty
                    // placeholder rather than a real-looking "0".
                    let plat = loc.get("lat").and_then(|v| v.as_f64());
                    let plon = loc.get("lon").and_then(|v| v.as_f64());
                    if plat != Some(0.0) || plon != Some(0.0) {
                        lat.set(plat);
                        lon.set(plon);
                    }
                    let elev = loc.get("elevation_m").and_then(|v| v.as_f64());
                    elevation_m.set(elev);
                    // A persisted elevation is treated as a manual value: don't
                    // auto-overwrite it on the next location set.
                    if elev.is_some() {
                        elevation_user_edited.set(true);
                    }
                }
                if let Some(t) = d
                    .get("config")
                    .and_then(|c| c.get("deployment"))
                    .and_then(|dep| dep.get("timezone"))
                    .and_then(|v| v.as_str())
                {
                    tz.set(t.to_string());
                }
                draft.set(d);
                loaded.set(true);
            }
        });
    });

    // Persist the four fields into the draft.
    let persist_now = move || {
        if !loaded.get_untracked() {
            return;
        }
        let mut changed = false;
        draft.update(|d| {
            let Some(dep) = d
                .get_mut("config")
                .and_then(|c| c.get_mut("deployment"))
                .and_then(|dep| dep.as_object_mut())
            else {
                return;
            };
            // lat/lon persist as numbers (the config schema wants f64); an
            // unset field writes 0.0 so the draft stays valid, and the Review
            // step still flags 0,0 as "not set". elevation_m is meters or null.
            let next_loc = serde_json::json!({
                "lat": lat.get_untracked().unwrap_or(0.0),
                "lon": lon.get_untracked().unwrap_or(0.0),
                "elevation_m": match elevation_m.get_untracked() {
                    Some(m) => m.into(),
                    None => serde_json::Value::Null,
                },
            });
            let tz_v = tz.get_untracked();
            let next_tz = if tz_v.trim().is_empty() {
                serde_json::Value::Null
            } else {
                tz_v.trim().into()
            };
            if dep.get("location") != Some(&next_loc) || dep.get("timezone") != Some(&next_tz) {
                dep.insert("location".into(), next_loc);
                dep.insert("timezone".into(), next_tz);
                changed = true;
            }
        });
        if !changed {
            return;
        }
        let candidate = draft.get_untracked();
        #[cfg(feature = "hydrate")]
        leptos::task::spawn_local(async move {
            let _ = save_draft(candidate).await;
        });
        #[cfg(not(feature = "hydrate"))]
        let _ = candidate;
    };

    // Autofill the timezone from lat/lon whenever they change and the
    // field is still empty. Quiet failure: the field stays editable.
    let suggest_tz = move || {
        #[cfg(feature = "hydrate")]
        {
            let (Some(la), Some(lo)) = (lat.get_untracked(), lon.get_untracked()) else {
                return;
            };
            if !tz.get_untracked().trim().is_empty() {
                return;
            }
            leptos::task::spawn_local(async move {
                let url = format!("/api/v1/location/timezone?lat={la}&lon={lo}");
                if let Ok(resp) = gloo_net::http::Request::get(&url).send().await {
                    if let Ok(v) = resp.json::<serde_json::Value>().await {
                        if let Some(name) = v.get("timezone").and_then(|t| t.as_str()) {
                            if tz.get_untracked().trim().is_empty() {
                                tz.set(name.to_string());
                                persist_now();
                            }
                        }
                    }
                }
            });
        }
    };

    // Prefill the elevation from lat/lon whenever they change, unless the
    // user has manually edited the field. The API returns meters and we store
    // meters (Open-Meteo and the elevation_m config field are both meters);
    // the display layer converts to the user's unit for the input. Quiet
    // failure: the field stays editable for manual entry. Mirrors suggest_tz's
    // lifecycle (fires from commit()).
    let suggest_elevation = move || {
        #[cfg(feature = "hydrate")]
        {
            let (Some(la), Some(lo)) = (lat.get_untracked(), lon.get_untracked()) else {
                return;
            };
            if elevation_user_edited.get_untracked() {
                return;
            }
            leptos::task::spawn_local(async move {
                let url = format!("/api/v1/location/elevation?lat={la}&lon={lo}");
                if let Ok(resp) = gloo_net::http::Request::get(&url).send().await {
                    if resp.ok() {
                        if let Ok(v) = resp.json::<serde_json::Value>().await {
                            if let Some(m) = v.get("elevation_m").and_then(|e| e.as_f64()) {
                                // Re-check the guard: the user may have typed
                                // while the request was in flight. The API
                                // returns meters; store meters (rounded).
                                if !elevation_user_edited.get_untracked() {
                                    elevation_m.set(Some(m.round()));
                                    persist_now();
                                }
                            }
                        }
                    }
                }
            });
        }
    };

    let commit = move || {
        suggest_tz();
        suggest_elevation();
        persist_now();
    };

    let on_search = move |_| {
        let q = query.get_untracked().trim().to_string();
        if q.is_empty() || searching.get_untracked() {
            return;
        }
        searching.set(true);
        results.set(Vec::new());
        #[cfg(feature = "hydrate")]
        leptos::task::spawn_local(async move {
            let url = format!("/api/wizard/geocode?q={}", urlencoding_lite(&q));
            if let Ok(resp) = gloo_net::http::Request::get(&url).send().await {
                if let Ok(v) = resp.json::<serde_json::Value>().await {
                    let list = v
                        .as_array()
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|r| {
                                    Some((
                                        r.get("display_name")?.as_str()?.to_string(),
                                        r.get("lat")?.as_str()?.parse::<f64>().ok()?,
                                        r.get("lon")?.as_str()?.parse::<f64>().ok()?,
                                    ))
                                })
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    results.set(list);
                }
            }
            searching.set(false);
        });
        #[cfg(not(feature = "hydrate"))]
        {
            let _ = q;
            searching.set(false);
        }
    };

    let lat_err: Signal<Option<String>> = Signal::derive(move || {
        // None (empty field) is not an error message on its own, but it does
        // gate advancing (can_advance below requires a set value); only flag a
        // typed value that's out of range, or the null-island 0,0 case.
        match lat.get() {
            Some(v) if !(-90.0..=90.0).contains(&v) => {
                Some(format!("Latitude must be between -90 and 90 (got {v:.4})"))
            }
            Some(v) if v == 0.0 && lon.get() == Some(0.0) => {
                Some("0,0 is the null island default; set your actual location".to_string())
            }
            _ => None,
        }
    });
    let lon_err: Signal<Option<String>> = Signal::derive(move || match lon.get() {
        Some(v) if !(-180.0..=180.0).contains(&v) => Some(format!(
            "Longitude must be between -180 and 180 (got {v:.4})"
        )),
        _ => None,
    });

    // Location is the single mandatory step: both coordinates must be set (not
    // the empty placeholder, not the null-island 0,0) and in range.
    let can_advance = move || {
        let (Some(la), Some(lo)) = (lat.get(), lon.get()) else {
            return false;
        };
        !(la == 0.0 && lo == 0.0) && lat_err.get().is_none() && lon_err.get().is_none()
    };

    let next_href = move || {
        if can_advance() {
            next_step_href("location")
        } else {
            None
        }
    };

    let results_view = move || {
        let list = results.get();
        if list.is_empty() {
            return ().into_any();
        }
        list.into_iter()
            .map(|(name, la, lo)| {
                let label = name.clone();
                view! {
                    <button
                        type="button"
                        class="geo-result"
                        on:click=move |_| {
                            lat.set(Some(la));
                            lon.set(Some(lo));
                            results.set(Vec::new());
                            // A freshly chosen place is a new location:
                            // re-derive its elevation, overriding any prior
                            // auto-fill (but the user can still edit after).
                            elevation_user_edited.set(false);
                            commit();
                        }
                    >
                        <span class="geo-result__name">{label}</span>
                        <span class="geo-result__coords">{format!("{la:.4}, {lo:.4}")}</span>
                    </button>
                }
            })
            .collect_view()
            .into_any()
    };

    view! {
        <div class="setup-step">
            <h2 class="setup-step__title">"Where are you?"<HelpHint topic="location"/></h2>
            <p class="setup-step__body">
                "LocalSky uses latitude and longitude for the radar center, the "
                "Open-Meteo forecast, sunrise/sunset, and the FAO-56 ET0 "
                "calculation. Search an address or type coordinates; the "
                "timezone fills itself in from the location."
            </p>

            <FormField
                label="Find by address".to_string()
                helptext="City, street, or landmark (OpenStreetMap lookup).".to_string()
                error=Signal::derive(|| None::<String>)
            >
                <div class="geo-search">
                    <input
                        type="text"
                        class="ui-input"
                        placeholder="e.g. Springfield, Sydney, or 51.5, -0.1"
                        prop:value=move || query.get()
                        on:input=move |ev| query.set(event_target_value(&ev))
                        on:keydown=move |ev| if ev.key() == "Enter" { on_search(()) }
                    />
                    <Button
                        variant="ghost"
                        disabled=Signal::derive(move || searching.get())
                        on_click=Callback::new(move |_| on_search(()))
                    >
                        {move || if searching.get() { "Searching…" } else { "Search" }}
                    </Button>
                </div>
            </FormField>
            <div class="geo-results">{results_view}</div>

            <FormField
                label="Latitude".to_string()
                helptext="Decimal degrees (positive north).".to_string()
                error=lat_err
            >
                <input
                    type="number"
                    step="0.0001"
                    class="ui-input"
                    placeholder="e.g. 40.7128"
                    prop:value=move || lat.get().map(|v| v.to_string()).unwrap_or_default()
                    on:input=move |ev| {
                        let raw = event_target_value(&ev);
                        if raw.trim().is_empty() {
                            lat.set(None);
                        } else if let Ok(v) = raw.parse::<f64>() {
                            lat.set(Some(v));
                        }
                    }
                    on:change=move |_| commit()
                />
            </FormField>

            <FormField
                label="Longitude".to_string()
                helptext="Decimal degrees (positive east).".to_string()
                error=lon_err
            >
                <input
                    type="number"
                    step="0.0001"
                    class="ui-input"
                    placeholder="e.g. -74.0060"
                    prop:value=move || lon.get().map(|v| v.to_string()).unwrap_or_default()
                    on:input=move |ev| {
                        let raw = event_target_value(&ev);
                        if raw.trim().is_empty() {
                            lon.set(None);
                        } else if let Ok(v) = raw.parse::<f64>() {
                            lon.set(Some(v));
                        }
                    }
                    on:change=move |_| commit()
                />
            </FormField>

            <FormField
                label="Elevation".to_string()
                helptext="Auto-filled from your location; edit to override. Used by FAO-56 net-radiation.".to_string()
                error=Signal::derive(|| None::<String>)
            >
                // Unit-suffixed input: the value is shown and entered in your
                // selected length unit (feet for imperial, meters for metric)
                // but stored as meters internally. The suffix updates with the
                // unit preference (it resolves client-side after hydration).
                <div style="display:flex; align-items:center; gap: var(--space-2)">
                    <input
                        type="number"
                        step="1"
                        class="ui-input"
                        style="flex:1"
                        placeholder=move || if elev_metric() { "meters" } else { "feet" }
                        prop:value=move || {
                            // Stored meters -> the displayed unit, rounded for a
                            // clean field; empty when unset.
                            elevation_m
                                .get()
                                .map(|m| m_to_display(m).round().to_string())
                                .unwrap_or_default()
                        }
                        on:input=move |ev| {
                            let raw = event_target_value(&ev);
                            elevation_user_edited.set(true);
                            if raw.trim().is_empty() {
                                elevation_m.set(None);
                            } else if let Ok(v) = raw.parse::<f64>() {
                                // Entered in the displayed unit; convert to meters.
                                elevation_m.set(Some(display_to_m(v)));
                            }
                        }
                        on:change=move |_| persist_now()
                    />
                    <span style="color: var(--text-dim); min-width: 1.5rem">{move || elev_unit()}</span>
                </div>
            </FormField>

            <FormField
                label="Timezone".to_string()
                helptext="IANA name (e.g. America/New_York or Europe/Berlin). Autofills from the location; clear to re-derive at boot.".to_string()
                error=Signal::derive(|| None::<String>)
            >
                <input
                    type="text"
                    class="ui-input"
                    placeholder="America/New_York"
                    prop:value=move || tz.get()
                    on:input=move |ev| tz.set(event_target_value(&ev))
                    on:change=move |_| persist_now()
                />
            </FormField>

            <SetupFooter
                prev=prev_step_href("location")
                next=Signal::derive(next_href)
            />
        </div>
    }
}

/// Tiny query encoder for the geocode call (space + reserved chars).
#[cfg(feature = "hydrate")]
fn urlencoding_lite(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b' ' => out.push('+'),
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b',' => {
                out.push(b as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}
