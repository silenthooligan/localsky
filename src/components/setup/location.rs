// LocationStep. Lat / lon / elevation / timezone, persisted into the
// wizard draft on every commit (load on mount, save on change). An
// address search drives the existing Nominatim proxy
// (GET /api/wizard/geocode?q=) so nobody has to know their coordinates,
// and the timezone autofills from the offline tzf dataset
// (GET /api/v1/location/timezone) whenever lat/lon change and the field
// is still empty.

use leptos::prelude::*;

use crate::components::setup::shell::{next_step_href, prev_step_href, SetupFooter};
use crate::components::ui::FormField;

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
    let lat = RwSignal::new(0.0f64);
    let lon = RwSignal::new(0.0f64);
    let elevation = RwSignal::new(0.0f64);
    let tz = RwSignal::new(String::new());

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
                    lat.set(loc.get("lat").and_then(|v| v.as_f64()).unwrap_or(0.0));
                    lon.set(loc.get("lon").and_then(|v| v.as_f64()).unwrap_or(0.0));
                    elevation.set(
                        loc.get("elevation_m")
                            .and_then(|v| v.as_f64())
                            .unwrap_or(0.0),
                    );
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
            let next_loc = serde_json::json!({
                "lat": lat.get_untracked(),
                "lon": lon.get_untracked(),
                "elevation_m": if elevation.get_untracked() == 0.0 {
                    serde_json::Value::Null
                } else {
                    elevation.get_untracked().into()
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
            let la = lat.get_untracked();
            let lo = lon.get_untracked();
            if (la == 0.0 && lo == 0.0) || !tz.get_untracked().trim().is_empty() {
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

    let commit = move || {
        suggest_tz();
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
        let v = lat.get();
        if !(-90.0..=90.0).contains(&v) {
            Some(format!("Latitude must be between -90 and 90 (got {v:.4})"))
        } else if v == 0.0 && lon.get() == 0.0 {
            Some("0,0 is the null island default; set your actual location".to_string())
        } else {
            None
        }
    });
    let lon_err: Signal<Option<String>> = Signal::derive(move || {
        let v = lon.get();
        if !(-180.0..=180.0).contains(&v) {
            Some(format!(
                "Longitude must be between -180 and 180 (got {v:.4})"
            ))
        } else {
            None
        }
    });

    let can_advance = move || lat_err.get().is_none() && lon_err.get().is_none();

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
                            lat.set(la);
                            lon.set(lo);
                            results.set(Vec::new());
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
            <h2 class="setup-step__title">"Where are you?"</h2>
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
                    <button
                        type="button"
                        class="setup-footer__btn setup-footer__btn--ghost"
                        prop:disabled=move || searching.get()
                        on:click=move |_| on_search(())
                    >
                        {move || if searching.get() { "Searching…" } else { "Search" }}
                    </button>
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
                    prop:value=move || lat.get()
                    on:input=move |ev| {
                        if let Ok(v) = event_target_value(&ev).parse::<f64>() {
                            lat.set(v);
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
                    prop:value=move || lon.get()
                    on:input=move |ev| {
                        if let Ok(v) = event_target_value(&ev).parse::<f64>() {
                            lon.set(v);
                        }
                    }
                    on:change=move |_| commit()
                />
            </FormField>

            <FormField
                label="Elevation (m)".to_string()
                helptext="Optional. Used by FAO-56 net-radiation. Leave at 0 for sea level.".to_string()
                error=Signal::derive(|| None::<String>)
            >
                <input
                    type="number"
                    step="1"
                    class="ui-input"
                    prop:value=move || elevation.get()
                    on:input=move |ev| {
                        if let Ok(v) = event_target_value(&ev).parse::<f64>() {
                            elevation.set(v);
                        }
                    }
                    on:change=move |_| persist_now()
                />
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
