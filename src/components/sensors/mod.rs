// Sensors, an integrations explorer. Pick any sensor on the left, see its
// TRUE live data on the right: the Tempest station's full readout, each
// soil probe's real moisture + 7-day projection, and every configured
// integration's health + what it provides. Reads the live snapshots; the
// integration list + health come from /api/v1/health.

use leptos::prelude::*;
use leptos_router::hooks::{use_location, use_navigate};

use crate::components::sources_form::SourceEditorPanel;
use crate::components::ui::{Button, Sparkline};
use crate::components::units_fmt::{
    fmt_distance_mi, fmt_pressure, fmt_rain_amount, fmt_rain_rate, fmt_temp_short, fmt_wind,
    temp_unit, temp_value, use_unit_prefs, UnitPrefs,
};
use crate::ha::snapshot::{IrrigationSnapshot, SoilForecast};
use crate::tempest::state::Snapshot;

#[cfg(feature = "hydrate")]
async fn save_config(cfg: serde_json::Value) -> Result<(), String> {
    use gloo_net::http::Request;
    let resp = Request::put("/api/config")
        .json(&cfg)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("HTTP {}: {body}", resp.status()));
    }
    Ok(())
}

/// Local-network data sources (the ones that matter as "sensors") vs cloud
/// data providers. Local = data physically on your LAN; cloud = a fetched
/// API/account.
fn is_local(kind: &str) -> bool {
    matches!(
        kind,
        "tempest_udp"
            | "ecowitt_local"
            | "ecowitt_gw_poll"
            | "davis_wll"
            | "mqtt"
            | "mqtt_subscribe"
            | "ha_passthrough"
            | "http_webhook"
            | "demo_replay"
    )
}

#[derive(Clone, Default)]
struct SourceRow {
    id: String,
    kind: String,
    status: String,
    stale_for_s: Option<i64>,
    enabled: bool,
}

/// One row of the per-field current-conditions provenance panel: which source
/// currently drives a given reading.
#[derive(Clone, Default)]
struct ConditionsRow {
    field: String,
    source: String,
}

#[derive(Clone, PartialEq)]
enum Sel {
    Tempest,
    Soil(String),
    Source(String),
    /// Inline "add a sensor" form in the detail pane (no navigation).
    AddSource,
    /// Inline edit form for the source with this id.
    EditSource(String),
}

/// Encode a selection as the URL for that detail view. The master-detail
/// selection is URL state, not a bare signal, so the phone back gesture steps
/// back one level (closes a form / returns to the Tempest readout) instead of
/// leaving /sensors. Tempest is the default and carries no param. The
/// `?source=` param name is kept for back-compat with existing deep links
/// (Edit buttons elsewhere point at it).
fn sel_url(sel: &Sel) -> String {
    match sel {
        Sel::Tempest => "/sensors".to_string(),
        Sel::Soil(slug) => format!("/sensors?soil={slug}"),
        Sel::Source(id) => format!("/sensors?view={id}"),
        Sel::AddSource => "/sensors?add=1".to_string(),
        Sel::EditSource(id) => format!("/sensors?source={id}"),
    }
}

/// Decode the selection from a raw search string. Reactive (driven by
/// use_location), so popstate / back updates it. Priority resolves a single
/// view deterministically: source -> EditSource, add -> AddSource,
/// view -> Source, soil -> Soil, else the default Tempest readout. An
/// unknown/missing value maps to the default (no phantom state).
fn parse_sel(search: &str) -> Sel {
    let param = |key: &str| -> Option<String> {
        search
            .trim_start_matches('?')
            .split('&')
            .find_map(|kv| kv.strip_prefix(&format!("{key}=")).map(str::to_string))
            .filter(|v| !v.is_empty())
    };
    if let Some(id) = param("source") {
        Sel::EditSource(id)
    } else if param("add").is_some() {
        Sel::AddSource
    } else if let Some(id) = param("view") {
        Sel::Source(id)
    } else if let Some(slug) = param("soil") {
        Sel::Soil(slug)
    } else {
        Sel::Tempest
    }
}

fn dot_color(status: &str) -> &'static str {
    match status {
        "fresh" => "var(--verdict-run)",
        "stale" => "var(--accent-warn)",
        _ => "var(--verdict-off)",
    }
}

fn dir_card(deg: f64) -> &'static str {
    const P: [&str; 16] = [
        "N", "NNE", "NE", "ENE", "E", "ESE", "SE", "SSE", "S", "SSW", "SW", "WSW", "W", "WNW",
        "NW", "NNW",
    ];
    P[((deg.rem_euclid(360.0) + 11.25) / 22.5) as usize % 16]
}

/// Parse the /api/v1/health `sources` array into rows.
#[cfg(feature = "hydrate")]
fn parse_source_rows(v: &serde_json::Value) -> Vec<SourceRow> {
    v.get("sources")
        .and_then(|s| s.as_array())
        .map(|arr| {
            arr.iter()
                .map(|s| SourceRow {
                    id: s
                        .get("id")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string(),
                    kind: s
                        .get("kind")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string(),
                    status: s
                        .get("status")
                        .and_then(|x| x.as_str())
                        .unwrap_or("offline")
                        .to_string(),
                    stale_for_s: s.get("stale_for_s").and_then(|x| x.as_i64()),
                    enabled: s.get("enabled").and_then(|x| x.as_bool()).unwrap_or(true),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Parse the /api/v1/health `conditions` array into provenance rows.
#[cfg(feature = "hydrate")]
fn parse_conditions(v: &serde_json::Value) -> Vec<ConditionsRow> {
    v.get("conditions")
        .and_then(|s| s.as_array())
        .map(|arr| {
            arr.iter()
                .map(|s| ConditionsRow {
                    field: s
                        .get("field")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string(),
                    source: s
                        .get("source")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string(),
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(feature = "hydrate")]
async fn load_health(sources: RwSignal<Vec<SourceRow>>, conditions: RwSignal<Vec<ConditionsRow>>) {
    if let Ok(resp) = gloo_net::http::Request::get("/api/v1/health").send().await {
        if let Ok(v) = resp.json::<serde_json::Value>().await {
            sources.set(parse_source_rows(&v));
            conditions.set(parse_conditions(&v));
        }
    }
}

#[cfg(feature = "hydrate")]
async fn load_config(config: RwSignal<serde_json::Value>) {
    if let Ok(resp) = gloo_net::http::Request::get("/api/config").send().await {
        if let Ok(v) = resp.json::<serde_json::Value>().await {
            config.set(v);
        }
    }
}

#[component]
pub fn SensorsPage(
    snap: ReadSignal<IrrigationSnapshot>,
    weather: ReadSignal<Snapshot>,
) -> impl IntoView {
    let prefs = use_unit_prefs();
    let sources = RwSignal::new(Vec::<SourceRow>::new());
    let conditions = RwSignal::new(Vec::<ConditionsRow>::new());
    // The detail selection is URL state (derived reactively from the search
    // string), so SSR + hydrate agree and the back gesture steps one level.
    // Picks navigate (push) via `nav_to`; the existing ?source= deep link still
    // lands in the edit form.
    let loc = use_location();
    let selected = Signal::derive(move || parse_sel(&loc.search.get()));
    let navigate = use_navigate();
    let nav_to: Callback<Sel> = Callback::new(move |sel: Sel| {
        navigate(&sel_url(&sel), Default::default());
    });
    // Full config (for the inline editor's read-modify-write + assignment
    // cross-reference). Fetched on mount alongside health.
    let config = RwSignal::new(serde_json::Value::Null);
    // Everything HA exposes that LocalSky could use, grouped by role.
    let discovered = RwSignal::new(serde_json::Value::Null);

    #[cfg(feature = "hydrate")]
    {
        Effect::new(move |_| {
            leptos::task::spawn_local(async move { load_health(sources, conditions).await });
        });
        Effect::new(move |_| {
            leptos::task::spawn_local(async move { load_config(config).await });
        });
        Effect::new(move |_| {
            leptos::task::spawn_local(async move {
                if let Ok(resp) = gloo_net::http::Request::get("/api/v1/sensors/discovered")
                    .send()
                    .await
                {
                    if let Ok(v) = resp.json::<serde_json::Value>().await {
                        discovered.set(v);
                    }
                }
            });
        });
    }
    #[cfg(not(feature = "hydrate"))]
    let _ = (sources, conditions, config, discovered);

    // Zone (slug, display_name) list for the MQTT soil-subscription per-zone
    // binding dropdown in the source editor. Derived from the loaded config so
    // it tracks adds/renames without a separate fetch.
    let zone_slugs = Memo::new(move |_| {
        config
            .get()
            .get("zones")
            .and_then(|z| z.as_object())
            .map(|zones| {
                zones
                    .iter()
                    .map(|(slug, z)| {
                        (
                            slug.clone(),
                            z.get("display_name")
                                .and_then(|v| v.as_str())
                                .unwrap_or(slug)
                                .to_string(),
                        )
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    });

    // Persist one source entry (add or replace-by-id) via read-modify-write
    // on the full config, then re-load health and land on the source's live
    // readings so the user immediately sees it ingesting.
    // Toast handle is only fired from hydrate-gated save paths below.
    #[allow(unused_variables)]
    let toast = crate::components::ui::use_toast();
    let persist_entry = Callback::new(move |entry: serde_json::Value| {
        let id = entry
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        config.update(|cfg| {
            if !cfg.is_object() {
                *cfg = serde_json::json!({});
            }
            let arr = cfg.as_object_mut().and_then(|o| {
                o.entry("sources")
                    .or_insert(serde_json::json!([]))
                    .as_array_mut()
            });
            if let Some(arr) = arr {
                if let Some(slot) = arr
                    .iter_mut()
                    .find(|s| s.get("id").and_then(|v| v.as_str()) == Some(id.as_str()))
                {
                    *slot = entry.clone();
                } else {
                    arr.push(entry.clone());
                }
            }
        });
        let candidate = config.get_untracked();
        #[cfg(feature = "hydrate")]
        leptos::task::spawn_local(async move {
            match save_config(candidate).await {
                Ok(()) => {
                    toast.success(format!("Saved {id}. Reloads on the next tick."));
                    nav_to.run(Sel::Source(id));
                    load_health(sources, conditions).await;
                }
                Err(e) => toast.error(format!("Save failed: {e}")),
            }
        });
        #[cfg(not(feature = "hydrate"))]
        let _ = candidate;
    });

    // Toggle a source's enabled flag (read-modify-write).
    let toggle_enabled = Callback::new(move |(id, want): (String, bool)| {
        config.update(|cfg| {
            if let Some(arr) = cfg.get_mut("sources").and_then(|v| v.as_array_mut()) {
                if let Some(slot) = arr
                    .iter_mut()
                    .find(|s| s.get("id").and_then(|v| v.as_str()) == Some(id.as_str()))
                {
                    if let Some(obj) = slot.as_object_mut() {
                        obj.insert("enabled".into(), serde_json::json!(want));
                    }
                }
            }
        });
        let candidate = config.get_untracked();
        #[cfg(feature = "hydrate")]
        leptos::task::spawn_local(async move {
            match save_config(candidate).await {
                Ok(()) => {
                    toast.success(if want { "Enabled." } else { "Disabled." });
                    load_health(sources, conditions).await;
                }
                Err(e) => toast.error(format!("Save failed: {e}")),
            }
        });
        #[cfg(not(feature = "hydrate"))]
        let _ = candidate;
    });

    view! {
        <div class="sensors-page">
            <header class="sensors-page__header">
                <div class="sensors-page__heading">
                    <p class="sensors-page__eyebrow">"Integrate"</p>
                    <h1 class="sensors-page__title">"Sensors"</h1>
                </div>
                <p class="sensors-page__sub">
                    "Every sensor LocalSky can use, from both Home Assistant and its own native sources, with the live readings flowing from them. Soil moisture feeds the per-zone skip decision; weather feeds ET."
                </p>
                <details class="sensors-howto">
                    <summary>"Where do sensors come from?"</summary>
                    <div class="sensors-howto__body">
                        <p><strong>"It doesn't matter where a device lives."</strong>" LocalSky and Home Assistant mirror each other, so a sensor added in either place shows up in both. You don't add probes here one by one."</p>
                        <p><strong>"From Home Assistant:"</strong>" anything HA already sees, Ecowitt soil probes, a Tempest, any weather or moisture entity, is imported automatically and appears here and on the "<a href="/settings?section=devices">"Devices"</a>" page. Pair a new probe in its own app first (for Ecowitt that's the Ecowitt / WS View app); once HA sees it, it shows up with no per-probe setup. Assign soil probes to zones in the "<a href="/settings?section=zones">"zone editor"</a>"."</p>
                        <p><strong>"From LocalSky directly:"</strong>" add a source LocalSky talks to itself, a LAN Ecowitt gateway, a webhook, MQTT, with \"Add a data source\" below. Receiver sources show live readings here the moment data arrives, so you can confirm it's working. Discovered gateways and controllers are listed on the "<a href="/settings?section=devices">"Devices"</a>" page."</p>
                    </div>
                </details>
                <div class="sensors-page__actions">
                    <Button variant="primary" icon="plus" on_click=Callback::new(move |_| nav_to.run(Sel::AddSource))>"Add a data source"</Button>
                    // One click into the settings SHELL (master-detail with the
                    // left section rail), not the bare /settings/devices route
                    // that drops the rail and reads as a different app.
                    <Button variant="secondary" icon="controllers" href="/settings?section=devices">"Manage all devices →"</Button>
                </div>
            </header>

            // Per-field provenance: which source currently drives each reading.
            // Makes the per-field merge visible (e.g. temp/wind from a station,
            // pressure from a barometer-only gateway) so source priority is
            // self-explanatory rather than a mystery.
            <Show when=move || !conditions.get().is_empty()>
                <details class="sensors-discovered" open=true>
                    <summary>"Current conditions sources"</summary>
                    <div class="sensors-discovered__body">
                        <p class="sensors-section__hint">
                            "Each reading is taken from its highest-priority source that provides it. "
                            "A partial source (e.g. a barometer-only gateway) contributes only its fields; "
                            "the rest stay with your main station. Adjust the order with each source's Priority."
                        </p>
                        {move || conditions.get().into_iter().map(|c| view! {
                            <div class="sensors-discovered__row">
                                <span class="sensors-discovered__name">{c.field}</span>
                                <span class="sensors-discovered__val">{c.source}</span>
                            </div>
                        }).collect_view()}
                    </div>
                </details>
            </Show>

            // Everything Home Assistant exposes that LocalSky can use, so
            // nothing is invisible regardless of where it was onboarded.
            <details class="sensors-discovered">
                <summary>"Discovered from Home Assistant"</summary>
                <div class="sensors-discovered__body">
                    {move || {
                        let d = discovered.get();
                        let Some(obj) = d.as_object() else {
                            return view! { <p class="sensors-section__hint">"Looking for connected devices…"</p> }.into_any();
                        };
                        if obj.is_empty() {
                            return view! { <p class="sensors-section__hint">"No Home Assistant sensors found (or HA isn't connected). Add a data source below, or check the ha_passthrough bridge."</p> }.into_any();
                        }
                        obj.iter().map(|(role, items)| {
                            let count = items.as_array().map(|a| a.len()).unwrap_or(0);
                            let rows = items.as_array().cloned().unwrap_or_default().into_iter().map(|s| {
                                let label = s.get("label").and_then(|v| v.as_str()).unwrap_or("").to_string();
                                let unit = s.get("unit").and_then(|v| v.as_str()).unwrap_or("").to_string();
                                let val = s.get("current_pct").and_then(|v| v.as_f64()).map(|v| format!("{v:.1} {unit}")).unwrap_or_else(|| "-".into());
                                view! { <div class="sensors-discovered__row"><span class="sensors-discovered__name">{label}</span><span class="sensors-discovered__val">{val}</span></div> }
                            }).collect_view();
                            view! {
                                <div class="sensors-discovered__group">
                                    <h4 class="sensors-discovered__role">{role.clone()}" ("{count}")"</h4>
                                    {rows}
                                </div>
                            }
                        }).collect_view().into_any()
                    }}
                    <p class="sensors-section__hint">"Soil sensors here are assignable to zones in the zone editor. Pair a new probe in its app and it appears here automatically."</p>
                </div>
            </details>

            <div class="sensors-shell">
                <aside class="sensors-list" aria-label="Sensors">
                    // Weather station.
                    <p class="sensors-list__group">"Weather station"</p>
                    <SensorRow
                        active=Signal::derive(move || selected.get() == Sel::Tempest)
                        on_pick=Callback::new(move |()| nav_to.run(Sel::Tempest))
                        dot=Signal::derive(move || if weather.get().last_packet_epoch > 0 { "var(--verdict-run)" } else { "var(--verdict-off)" })
                        title="Tempest".to_string()
                        sub=Signal::derive(move || {
                            let p = prefs.get();
                            let w = weather.get();
                            format!("{} · {:.0}% · {}", fmt_temp_short(w.air_temp_f, p), w.rh_pct, fmt_wind(w.wind_avg_mph, p))
                        })
                    />

                    // Soil probes.
                    {move || {
                        let fc = snap.get().soil_forecasts;
                        if fc.is_empty() { return ().into_any(); }
                        view! {
                            <p class="sensors-list__group">"Soil probes"</p>
                            {fc.into_iter().map(|z| {
                                let slug = z.zone_slug.clone();
                                let s_for_active = slug.clone();
                                let s_for_pick = slug.clone();
                                let cur = z.current_pct;
                                let name = z.zone_name.clone();
                                view! {
                                    <SensorRow
                                        active=Signal::derive(move || selected.get() == Sel::Soil(s_for_active.clone()))
                                        on_pick=Callback::new(move |()| nav_to.run(Sel::Soil(s_for_pick.clone())))
                                        dot=Signal::derive(move || if cur.is_some() { "var(--verdict-run)" } else { "var(--verdict-off)" })
                                        title=name
                                        sub=Signal::derive(move || match cur { Some(c) => format!("{c:.0}% moisture"), None => "probe offline".into() })
                                    />
                                }
                            }).collect_view()}
                        }.into_any()
                    }}

                    // Local sensors (the data that matters), then cloud.
                    {move || {
                        let rows = sources.get();
                        if rows.is_empty() { return ().into_any(); }
                        let (local, cloud): (Vec<_>, Vec<_>) = rows.into_iter().partition(|r| is_local(&r.kind));
                        // ha_passthrough with an empty field_map feeds no
                        // sensor data; it lives on the Home Assistant panel
                        // (Settings) instead of masquerading as a sensor here.
                        let cfg_now = config.get();
                        let feeds_nothing = |id: &str| -> bool {
                            cfg_now
                                .get("sources")
                                .and_then(|a| a.as_array())
                                .and_then(|arr| {
                                    arr.iter().find(|s| {
                                        s.get("id").and_then(|v| v.as_str()) == Some(id)
                                    })
                                })
                                .map(|s| {
                                    s.get("kind").and_then(|v| v.as_str()) == Some("ha_passthrough")
                                        && s.pointer("/config/field_map")
                                            .and_then(|m| m.as_object())
                                            .map(|m| m.is_empty())
                                            .unwrap_or(true)
                                })
                                .unwrap_or(false)
                        };
                        let local: Vec<SourceRow> =
                            local.into_iter().filter(|r| !feeds_nothing(&r.id)).collect();
                        let render = move |r: SourceRow| {
                            let id = r.id.clone();
                            let id_a = id.clone();
                            let id_p = id.clone();
                            // Friendly kind label in the row subtitle so a cloud
                            // source reads "Open-Meteo", not the raw "open_meteo"
                            // tag, matching the footer + conditions provenance.
                            let kind =
                                crate::components::sources_form::friendly_source_name(&r.kind);
                            let dotc = dot_color(&r.status);
                            view! {
                                <SensorRow
                                    active=Signal::derive(move || selected.get() == Sel::Source(id_a.clone()))
                                    on_pick=Callback::new(move |()| nav_to.run(Sel::Source(id_p.clone())))
                                    dot=Signal::derive(move || dotc)
                                    title=id
                                    sub=Signal::derive(move || kind.clone())
                                />
                            }
                        };
                        let r2 = render;
                        view! {
                            {(!local.is_empty()).then(|| view! {
                                <p class="sensors-list__group">"Local sensors"</p>
                                {local.into_iter().map(render).collect_view()}
                            })}
                            {(!cloud.is_empty()).then(|| view! {
                                <p class="sensors-list__group">"Cloud data"</p>
                                {cloud.into_iter().map(r2).collect_view()}
                            })}
                        }.into_any()
                    }}
                </aside>

                <div class="sensors-detail">
                    {move || match selected.get() {
                        Sel::Tempest => view! { <TempestDetail s=weather prefs/> }.into_any(),
                        Sel::Soil(slug) => {
                            match snap.get().soil_forecasts.into_iter().find(|z| z.zone_slug == slug) {
                                Some(z) => view! { <SoilDetail z/> }.into_any(),
                                None => view! { <div class="sensors-empty">"Probe not reporting."</div> }.into_any(),
                            }
                        }
                        Sel::Source(id) => {
                            match sources.get().into_iter().find(|r| r.id == id) {
                                Some(r) => {
                                    // Zones whose soil sensor references this
                                    // source ("source:<id>:<channel>" or a bare
                                    // id), so assignment is visible right here.
                                    let assigned: Vec<(String, String)> = config
                                        .get()
                                        .get("zones")
                                        .and_then(|z| z.as_object())
                                        .map(|zones| {
                                            zones
                                                .iter()
                                                .filter(|(_, z)| {
                                                    z.get("soil_sensor_id")
                                                        .and_then(|v| v.as_str())
                                                        .map(|spec| {
                                                            spec == id
                                                                || spec.starts_with(&format!("source:{id}"))
                                                        })
                                                        .unwrap_or(false)
                                                })
                                                .map(|(slug, z)| {
                                                    (
                                                        slug.clone(),
                                                        z.get("display_name")
                                                            .and_then(|v| v.as_str())
                                                            .unwrap_or(slug)
                                                            .to_string(),
                                                    )
                                                })
                                                .collect()
                                        })
                                        .unwrap_or_default();
                                    view! { <SourceDetail r nav_to toggle_enabled assigned prefs/> }.into_any()
                                }
                                None => view! { <div class="sensors-empty">"Source not found."</div> }.into_any(),
                            }
                        }
                        Sel::AddSource => view! {
                            <div class="sensors-detail__editor">
                                <SourceEditorPanel
                                    on_commit=persist_entry
                                    on_cancel=Callback::new(move |()| nav_to.run(Sel::Tempest))
                                    zone_slugs=zone_slugs
                                />
                            </div>
                        }.into_any(),
                        Sel::EditSource(id) => {
                            let existing = config.get()
                                .get("sources").and_then(|s| s.as_array())
                                .and_then(|arr| arr.iter().find(|s| s.get("id").and_then(|v| v.as_str()) == Some(id.as_str())).cloned());
                            let back = id.clone();
                            view! {
                                <div class="sensors-detail__editor">
                                    <SourceEditorPanel
                                        existing=existing
                                        on_commit=persist_entry
                                        on_cancel=Callback::new(move |()| nav_to.run(Sel::Source(back.clone())))
                                        zone_slugs=zone_slugs
                                    />
                                </div>
                            }.into_any()
                        }
                    }}
                </div>
            </div>
        </div>
    }
}

#[component]
fn SensorRow(
    active: Signal<bool>,
    on_pick: Callback<()>,
    dot: Signal<&'static str>,
    #[prop(into)] title: String,
    sub: Signal<String>,
) -> impl IntoView {
    view! {
        <button type="button" class="sensor-row" class:is-active=move || active.get() on:click=move |_| on_pick.run(())>
            <span class="sensor-row__dot" style=move || format!("background:{}", dot.get())></span>
            <span class="sensor-row__text">
                <span class="sensor-row__title">{title}</span>
                <span class="sensor-row__sub">{move || sub.get()}</span>
            </span>
        </button>
    }
}

/// One label/value row in a detail group.
#[component]
fn F(#[prop(into)] k: String, #[prop(into)] v: String) -> impl IntoView {
    view! { <div class="sensor-field"><dt>{k}</dt><dd>{v}</dd></div> }
}

#[component]
fn FieldGroup(#[prop(into)] title: String, children: Children) -> impl IntoView {
    view! {
        <section class="sensor-group">
            <h3 class="sensor-group__title">{title}</h3>
            <dl class="sensor-group__fields">{children()}</dl>
        </section>
    }
}

#[component]
fn TempestDetail(s: ReadSignal<Snapshot>, prefs: Signal<UnitPrefs>) -> impl IntoView {
    move || {
        let d = s.get();
        let p = prefs.get();
        let precip = match d.precip_type {
            1 => "rain",
            2 => "hail",
            _ => "none",
        };
        let fresh = if d.last_packet_epoch > 0 {
            "live"
        } else {
            "no packet yet"
        };
        view! {
            <div class="sensor-detail-card">
                <div class="sensor-detail-card__head">
                    <h2>"Tempest weather station"</h2>
                    <span class="sensor-detail-card__meta">{fresh}</span>
                </div>
                <div class="sensor-groups">
                    <FieldGroup title="Air">
                        <F k="Temperature" v=format!("{} {}", temp_value(d.air_temp_f, p), temp_unit(p))/>
                        <F k="Feels like" v=format!("{} {}", temp_value(d.feels_like_f, p), temp_unit(p))/>
                        <F k="Dew point" v=format!("{} {}", temp_value(d.dew_point_f, p), temp_unit(p))/>
                        <F k="Wet bulb" v=format!("{} {}", temp_value(d.wet_bulb_f, p), temp_unit(p))/>
                        <F k="Humidity" v=format!("{:.0} %", d.rh_pct)/>
                        <F k="Pressure" v=fmt_pressure(d.pressure_inhg, p)/>
                    </FieldGroup>
                    <FieldGroup title="Wind">
                        <F k="Average" v=fmt_wind(d.wind_avg_mph, p)/>
                        <F k="Gust" v=fmt_wind(d.wind_gust_mph, p)/>
                        <F k="Lull" v=fmt_wind(d.wind_lull_mph, p)/>
                        <F k="Direction" v=format!("{} ({:.0}°)", dir_card(d.wind_dir_deg), d.wind_dir_deg)/>
                        <F k="Rapid" v=fmt_wind(d.rapid_wind_mph, p)/>
                    </FieldGroup>
                    <FieldGroup title="Rain">
                        <F k="Today" v=fmt_rain_amount(d.rain_in_today, p)/>
                        <F k="Rate" v=fmt_rain_rate(d.rain_intensity_in_hr, p)/>
                        <F k="Last minute" v=fmt_rain_amount(d.rain_in_last_min, p)/>
                        <F k="Type" v=precip.to_string()/>
                    </FieldGroup>
                    <FieldGroup title="Light">
                        <F k="Solar" v=format!("{:.0} W/m²", d.solar_w_m2)/>
                        <F k="Illuminance" v=format!("{:.0} lux", d.illuminance_lx)/>
                        <F k="UV index" v=format!("{:.1}", d.uv_index)/>
                    </FieldGroup>
                    <FieldGroup title="Lightning">
                        <F k="Last minute" v=d.lightning_count_last_min.to_string()/>
                        <F k="Last hour" v=d.lightning_strikes_last_hour.to_string()/>
                        <F k="Avg distance" v=fmt_distance_mi(d.lightning_avg_dist_mi, p)/>
                        <F k="Last strike" v=d.last_strike_distance_mi.map(|m| fmt_distance_mi(m, p)).unwrap_or_else(|| "-".into())/>
                    </FieldGroup>
                    <FieldGroup title="Station">
                        <F k="Battery" v=format!("{:.2} V ({:.0}%)", d.battery_v, d.battery_pct)/>
                        <F k="Station" v=if d.station_serial.is_empty() { "-".into() } else { d.station_serial.clone() }/>
                        <F k="Hub" v=if d.hub_serial.is_empty() { "-".into() } else { d.hub_serial.clone() }/>
                    </FieldGroup>
                </div>
            </div>
        }
    }
}

#[component]
fn SoilDetail(z: SoilForecast) -> impl IntoView {
    let name = z.zone_name.clone();
    let (cur, status, color) = match z.current_pct {
        None => ("offline".to_string(), "OFFLINE", "var(--verdict-off)"),
        Some(c) => {
            let s = if c >= z.target_max_pct {
                ("SATURATED", "var(--verdict-skip)")
            } else if c < z.target_min_pct {
                ("DRY", "var(--accent-warm)")
            } else {
                ("HEALTHY", "var(--verdict-run)")
            };
            (format!("{c:.0}%"), s.0, s.1)
        }
    };
    let proj = z.predicted_pct.clone();
    view! {
        <div class="sensor-detail-card" style=format!("--sc:{color}")>
            <div class="sensor-detail-card__head">
                <h2>{name}" soil probe"</h2>
                <span class="soil-card__pill">{status}</span>
            </div>
            <div class="sensor-detail-card__big">{cur}</div>
            <div class="sensor-groups">
                <FieldGroup title="Moisture">
                    <F k="Current" v=z.current_pct.map(|c| format!("{c:.0} %")).unwrap_or_else(|| "-".into())/>
                    <F k="Healthy band" v=format!("{:.0} to {:.0} %", z.target_min_pct, z.target_max_pct)/>
                    <F k="Saturation (skip)" v=format!("{:.0} %", z.target_max_pct)/>
                </FieldGroup>
            </div>
            {(proj.len() > 1).then(|| view! {
                <section class="sensor-group">
                    <h3 class="sensor-group__title">"7-day projection (rain + ET, no watering)"</h3>
                    <Sparkline points=proj accent=color.to_string() height=48/>
                </section>
            })}
        </div>
    }
}

/// One live field reported by a source: (key, value, age in seconds).
#[derive(Clone)]
struct LiveReading {
    key: String,
    value: f64,
    age_s: i64,
}

/// Friendly label for an Ecowitt/raw sensor key. Falls back to the raw key
/// so nothing is hidden if a new field shows up.
fn reading_label(key: &str) -> String {
    let pretty = match key {
        "tempf" | "tempinf" => "Temperature (°F)",
        "humidity" | "humidityin" => "Humidity (%)",
        "baromrelin" => "Pressure, rel (inHg)",
        "baromabsin" => "Pressure, abs (inHg)",
        "windspeedmph" => "Wind avg (mph)",
        "windgustmph" => "Wind gust (mph)",
        "winddir" => "Wind dir (°)",
        "solarradiation" => "Solar (W/m²)",
        "uv" => "UV index",
        "rainratein" => "Rain rate (in/hr)",
        "dailyrainin" => "Rain today (in)",
        _ if key.starts_with("soilmoisture") => {
            // Zone-bound MQTT soil is keyed `soilmoisture_<zone_slug>`; native
            // channels are `soilmoisture<N>`. Render the slug form by zone.
            let rest = &key[12..];
            if let Some(zone) = rest.strip_prefix('_') {
                return format!("Soil moisture, {} (%)", zone.replace('_', " "));
            }
            return format!("Soil moisture ch{rest} (%)");
        }
        _ if key.starts_with("soilad") => {
            return format!("Soil raw ch{}", &key[6..]);
        }
        _ if key.starts_with("temp") && key.ends_with('f') => {
            return format!("{key} (°F)");
        }
        _ => return key.to_string(),
    };
    pretty.to_string()
}

/// The convertible measurement kind a raw sensor key carries, if any. Drives
/// both the value formatting (helper choice) and the parenthetical unit shown
/// in the field label, so a reading and its label always agree on the unit.
#[derive(Clone, Copy, PartialEq)]
enum ReadingKind {
    Temp,
    Pressure,
    Wind,
    RainRate,
    RainDepth,
    None,
}

fn reading_kind(key: &str) -> ReadingKind {
    match key {
        "tempf" | "tempinf" => ReadingKind::Temp,
        "baromrelin" | "baromabsin" => ReadingKind::Pressure,
        "windspeedmph" | "windgustmph" => ReadingKind::Wind,
        "rainratein" => ReadingKind::RainRate,
        "dailyrainin" => ReadingKind::RainDepth,
        _ if key.starts_with("temp") && key.ends_with('f') => ReadingKind::Temp,
        _ => ReadingKind::None,
    }
}

/// Field label for a live reading WITHOUT a baked-in unit parenthetical for
/// convertible kinds (the value string carries the live unit instead, so the
/// two can never disagree). Non-convertible keys keep `reading_label`'s text.
fn reading_label_unitless(key: &str) -> String {
    match key {
        "tempf" | "tempinf" => "Temperature".to_string(),
        "baromrelin" => "Pressure, rel".to_string(),
        "baromabsin" => "Pressure, abs".to_string(),
        "windspeedmph" => "Wind avg".to_string(),
        "windgustmph" => "Wind gust".to_string(),
        "rainratein" => "Rain rate".to_string(),
        "dailyrainin" => "Rain today".to_string(),
        _ if key.starts_with("temp") && key.ends_with('f') => key.to_string(),
        _ => reading_label(key),
    }
}

/// Format a live reading's value with the display unit for its kind. Source
/// values are imperial-as-stored (°F, inHg, mph, in/hr, in); convert at the
/// display boundary. Non-convertible kinds keep the raw two-decimal value (the
/// unit, if any, lives in `reading_label`).
fn fmt_reading_value(key: &str, value: f64, p: UnitPrefs) -> String {
    match reading_kind(key) {
        ReadingKind::Temp => format!("{} {}", temp_value(value, p), temp_unit(p)),
        ReadingKind::Pressure => fmt_pressure(value, p),
        ReadingKind::Wind => fmt_wind(value, p),
        ReadingKind::RainRate => fmt_rain_rate(value, p),
        ReadingKind::RainDepth => fmt_rain_amount(value, p),
        ReadingKind::None => format!("{value:.2}"),
    }
}

fn age_phrase(s: i64) -> String {
    match s {
        s if s < 90 => "just now".to_string(),
        s if s < 3600 => format!("{}m ago", s / 60),
        s if s < 86_400 => format!("{}h ago", s / 3600),
        s => format!("{}d ago", s / 86_400),
    }
}

#[component]
fn SourceDetail(
    r: SourceRow,
    nav_to: Callback<Sel>,
    toggle_enabled: Callback<(String, bool)>,
    /// Zones whose soil sensor is wired to this source: (slug, name).
    assigned: Vec<(String, String)>,
    prefs: Signal<UnitPrefs>,
) -> impl IntoView {
    let dot = dot_color(&r.status);
    let seen = match r.stale_for_s {
        Some(s) => age_phrase(s),
        None => "never".to_string(),
    };
    let edit_id = r.id.clone();
    let toggle_id = r.id.clone();
    let enabled_now = r.enabled;

    // Live per-field readings recorded for receiver sources (Ecowitt,
    // webhook) and the Tempest sampler. Lets the user validate that the
    // source is actually ingesting and SEE the values it posted.
    let readings = RwSignal::new(Vec::<LiveReading>::new());
    let loaded = RwSignal::new(false);
    #[cfg(feature = "hydrate")]
    {
        let id = r.id.clone();
        Effect::new(move |_| {
            // Clear the skeleton the instant we're on the client, BEFORE the
            // fetch resolves: the section then shows the resolved state (the
            // "no readings yet" hint for poll/cloud sources that record none, or
            // the live values once they arrive) and can never get stuck loading
            // if the request is slow/empty. (Many sources -- Tempest under its
            // "Tempest" label, cloud pollers -- legitimately have no per-field
            // readings here, so the hint is the correct resting state.)
            loaded.set(true);
            let id = id.clone();
            leptos::task::spawn_local(async move {
                let url = format!("/api/v1/weather/readings?source={id}");
                if let Ok(resp) = gloo_net::http::Request::get(&url).send().await {
                    if let Ok(arr) = resp.json::<Vec<serde_json::Value>>().await {
                        let rows = arr
                            .into_iter()
                            .filter_map(|v| {
                                Some(LiveReading {
                                    key: v.get("key")?.as_str()?.to_string(),
                                    value: v.get("value")?.as_f64()?,
                                    age_s: v.get("age_s").and_then(|x| x.as_i64()).unwrap_or(0),
                                })
                            })
                            .collect::<Vec<_>>();
                        readings.set(rows);
                    }
                }
            });
        });
    }
    #[cfg(not(feature = "hydrate"))]
    {
        let _ = (&readings, &loaded);
    }

    let kind = r.kind.clone();
    view! {
        <div class="sensor-detail-card">
            <div class="sensor-detail-card__head">
                <h2>{r.id.clone()}</h2>
                <span class="sensor-detail-card__status"><span class="source-health__dot" style=format!("background:{dot}")></span>{r.status.clone()}</span>
            </div>
            <div class="sensor-detail-card__actions">
                <Button variant="ghost"
                    on_click=Callback::new(move |_| nav_to.run(Sel::EditSource(edit_id.clone())))>"Edit"</Button>
                <Button variant="ghost"
                    on_click=Callback::new(move |_| toggle_enabled.run((toggle_id.clone(), !enabled_now)))>
                    {if enabled_now { "Disable" } else { "Enable" }}
                </Button>
            </div>
            <div class="sensor-groups">
                <FieldGroup title="Zone assignment">
                    {if assigned.is_empty() {
                        view! {
                            <p class="sensors-section__hint" style="margin:0">
                                "Not assigned to any zone. Open a zone's settings and pick "
                                "this source as its soil sensor to drive per-zone skip rules."
                            </p>
                        }.into_any()
                    } else {
                        assigned
                            .iter()
                            .map(|(slug, name)| {
                                let href = format!("/settings?section=zones&zone={slug}");
                                let label = name.clone();
                                view! {
                                    <a class="sensor-assigned" href=href>
                                        <crate::components::ui::Icon name="zones" size=14/>
                                        {label}
                                        <span class="sensor-assigned__edit">"edit zone"</span>
                                    </a>
                                }
                            })
                            .collect_view()
                            .into_any()
                    }}
                </FieldGroup>
                <FieldGroup title="Integration">
                    <F k="Type" v=crate::components::sources_form::friendly_source_name(&r.kind)/>
                    <F k="Scope" v=if is_local(&r.kind) { "Local network".to_string() } else { "Cloud".to_string() }/>
                    <F k="Enabled" v=if r.enabled { "yes".to_string() } else { "no".to_string() }/>
                    <F k="Last reading" v=seen/>
                    <F k="Status" v=r.status.clone()/>
                </FieldGroup>
            </div>

            // Live readings, the actual values this source has posted.
            {move || {
                let rows = readings.get();
                if !rows.is_empty() {
                    let p = prefs.get();
                    let newest = rows.iter().map(|x| x.age_s).min().unwrap_or(0);
                    return view! {
                        <section class="sensor-group">
                            <h3 class="sensor-group__title">
                                "Live readings · " {rows.len()} " fields · updated " {age_phrase(newest)}
                            </h3>
                            <dl class="sensor-group__fields">
                                {rows.into_iter().map(|x| {
                                    // Convertible kinds carry their unit in the
                                    // value (via the units_fmt helpers) and use
                                    // the unitless label so the two agree; other
                                    // keys keep reading_label's parenthetical.
                                    let label = if reading_kind(&x.key) == ReadingKind::None {
                                        reading_label(&x.key)
                                    } else {
                                        reading_label_unitless(&x.key)
                                    };
                                    view! {
                                        <F k=label v=fmt_reading_value(&x.key, x.value, p)/>
                                    }
                                }).collect_view()}
                            </dl>
                        </section>
                    }.into_any();
                }
                if loaded.get() {
                    // Receivers (Ecowitt LAN push, webhook) DO log per-field
                    // readings here once the device POSTs; everything else (a
                    // Tempest, a Davis, a forecast, a soil gateway) feeds the
                    // merged Weather/Soil views + the dashboard instead of a
                    // separate per-field log -- so "no per-field log" is normal,
                    // not an error.
                    let is_receiver = matches!(kind.as_str(), "ecowitt_local" | "http_webhook");
                    return if is_receiver {
                        view! {
                            <p class="sensors-section__hint">
                                "Waiting for the first report. Point the device at "
                                <code>{format!("/ingest/{}", if kind == "ecowitt_local" { "ecowitt" } else { "webhook/<id>" })}</code>
                                " and its values appear here within one reporting cycle."
                            </p>
                        }.into_any()
                    } else {
                        view! {
                            <p class="sensors-section__hint">
                                "This source feeds the Weather and Soil views (and the dashboard) directly, so it keeps no separate per-field log here. It's working as long as its status above reads fresh."
                            </p>
                        }.into_any()
                    };
                }
                view! { <crate::components::ui::SkeletonRows count=3/> }.into_any()
            }}
        </div>
    }
}
