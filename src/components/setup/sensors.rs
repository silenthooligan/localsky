// SensorsStep. Bind discovered soil probes to the zones the operator just
// defined, so onboarding finishes complete instead of forcing a trip to
// Settings -> Sensors afterward. Mirrors the Settings Sensors lens visually
// (gateway card -> probe cards -> bind-to-zone select), but reads from the
// DRAFT (zones + gateway sources) and writes a zone's `soil_sensor_id` the
// same way every other draft mutation does.
//
// THE WRINKLE: the draft is not applied while the wizard runs, so the
// `ecowitt_gw_poll` source the user just added in the Weather step is NOT
// polling and `/api/v1/sensors/inventory` lists nothing for it. So this step
// does NOT use the live inventory. Instead, for each Ecowitt gateway source in
// the draft, it calls POST /api/wizard/probe_soil { host } which queries the
// gateway's local API directly over the LAN and returns its current soil
// channels (reusing the same parser the live poller uses, so the channel ids
// are byte-identical to the eventual live binding).
//
// HTTP-only (gloo_net), so it compiles for both ssr and hydrate without
// touching any ssr-gated module.

use leptos::prelude::*;

use crate::components::setup::shell::{next_step_href, prev_step_href, SetupFooter};
use crate::components::ui::{HelpHint, Icon, Panel};
use crate::docs::doc_url;

// ---------------------------------------------------------------------------
// HTTP (hydrate-only; SSR never runs these).
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Draft projections (plain serde_json reads, identical to the other steps).
// ---------------------------------------------------------------------------

/// A zone option for the bind dropdown: (config key / slug, display name).
type ZoneOpt = (String, String);

/// Friendly label for a source kind (snake_case slug from the draft),
/// mirroring the Settings Sensors `kind_label` so a newcomer never sees the
/// bare slug. Only `ecowitt_gw_poll` reaches this step today, but the others
/// are kept so the mapping stays in lockstep with Settings.
fn kind_label(kind: &str) -> String {
    match kind {
        "ecowitt_gw_poll" | "ecowitt" => "Ecowitt gateway".to_string(),
        "ecowitt_push" => "Ecowitt push".to_string(),
        "mqtt" => "MQTT".to_string(),
        "home_assistant" | "ha" => "Home Assistant".to_string(),
        "opensprinkler" => "OpenSprinkler".to_string(),
        "esphome" => "ESPHome".to_string(),
        "" => "Gateway".to_string(),
        other => other.replace('_', " "),
    }
}

/// Human title for a gateway card: the friendly kind plus the host in parens
/// when present, e.g. "Ecowitt gateway (10.0.0.50)". Never the raw slug.
fn gateway_title(kind: &str, host: &str) -> String {
    let base = kind_label(kind);
    if host.is_empty() {
        base
    } else {
        format!("{base} ({host})")
    }
}

/// An Ecowitt gateway source found in the draft, the unit we probe + group by.
#[derive(Clone, PartialEq)]
struct DraftGateway {
    /// Source id; channel ids are `source:<source_id>:soilmoisture<N>`.
    source_id: String,
    /// Source kind slug (e.g. `ecowitt_gw_poll`), turned into a friendly
    /// card title via `kind_label`.
    kind: String,
    /// Gateway IP / hostname to probe.
    host: String,
}

/// One live soil channel returned by /api/wizard/probe_soil.
#[derive(Clone, PartialEq)]
struct ProbeChannel {
    channel: String,
    id: String,
    moisture_pct: Option<f64>,
    battery_pct: Option<f64>,
    temp_f: Option<f64>,
    ec: Option<f64>,
}

/// Zone options straight from the draft, the same shape the Settings Sensors
/// view and the zone editor read (object keyed by slug, value has
/// display_name). Sorted for stable order.
fn draft_zone_opts(draft: &serde_json::Value) -> Vec<ZoneOpt> {
    let zones = draft
        .get("config")
        .and_then(|c| c.get("zones"))
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();
    let mut opts: Vec<ZoneOpt> = zones
        .iter()
        .map(|(slug, z)| {
            let name = z
                .get("display_name")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .unwrap_or_else(|| slug.replace(['-', '_'], " "));
            (slug.clone(), name)
        })
        .collect();
    opts.sort_by_key(|o| o.1.to_lowercase());
    opts
}

/// Every Ecowitt gateway source in the draft that carries a host to probe.
fn draft_gateways(draft: &serde_json::Value) -> Vec<DraftGateway> {
    draft
        .get("config")
        .and_then(|c| c.get("sources"))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter(|s| s.get("kind").and_then(|k| k.as_str()) == Some("ecowitt_gw_poll"))
                .filter_map(|s| {
                    let source_id = s.get("id").and_then(|v| v.as_str())?.to_string();
                    let kind = s
                        .get("kind")
                        .and_then(|k| k.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let host = s
                        .get("config")
                        .and_then(|c| c.get("host"))
                        .and_then(|v| v.as_str())
                        .filter(|h| !h.is_empty())?
                        .to_string();
                    Some(DraftGateway {
                        source_id,
                        kind,
                        host,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The zone slug currently bound to `probe_id`, if any (scans the draft
/// zones' `soil_sensor_id`).
fn bound_zone_for(draft: &serde_json::Value, probe_id: &str) -> Option<String> {
    let zones = draft
        .get("config")
        .and_then(|c| c.get("zones"))
        .and_then(|v| v.as_object())?;
    for (slug, z) in zones {
        if z.get("soil_sensor_id").and_then(|v| v.as_str()) == Some(probe_id) {
            return Some(slug.clone());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Component.
// ---------------------------------------------------------------------------

#[component]
pub fn SensorsStep() -> impl IntoView {
    // Full draft, kept in sync so a bind mutation + PUT round-trips exactly
    // the way zones.rs / the Settings view persist `soil_sensor_id`.
    let draft = RwSignal::new(serde_json::Value::Null);
    // Probed channels keyed by gateway source_id. Populated on load (and on
    // demand via a re-probe button) by hitting /api/wizard/probe_soil.
    let probes: RwSignal<Vec<(String, Vec<ProbeChannel>)>> = RwSignal::new(Vec::new());
    let probing = RwSignal::new(false);
    let probe_err = RwSignal::new(String::new());
    let saving = RwSignal::new(false);
    // Same toast hub the Settings Sensors bind path uses, so a confirmed save
    // (or a failure) gives the same lightweight feedback here.
    let toast = crate::components::ui::use_toast();

    #[cfg(feature = "hydrate")]
    Effect::new(move |_| {
        leptos::task::spawn_local(async move {
            if let Some(d) = fetch_draft().await {
                let gws = draft_gateways(&d);
                draft.set(d);
                probe_all(gws, probes, probing, probe_err).await;
            }
        });
    });
    #[cfg(not(feature = "hydrate"))]
    {
        let _ = (probes, probing, probe_err);
    }

    // Re-probe on demand (e.g. user just plugged in a probe). Re-reads the
    // draft so a gateway added without leaving this step is picked up.
    let on_reprobe = move |_| {
        if probing.get_untracked() {
            return;
        }
        let gws = draft_gateways(&draft.get_untracked());
        #[cfg(feature = "hydrate")]
        leptos::task::spawn_local(async move {
            probe_all(gws, probes, probing, probe_err).await;
        });
        #[cfg(not(feature = "hydrate"))]
        let _ = gws;
    };

    // Bind a probe to a zone (or unbind when `zone_key` is empty). Writes the
    // chosen zone's `soil_sensor_id` to the probe id and clears the probe off
    // any OTHER zone (a probe maps to one zone), then PUTs the whole draft.
    // Identical mutation shape to the Settings Sensors bind path.
    let bind = Callback::new(move |(probe_id, zone_key): (String, String)| {
        if saving.get_untracked() {
            return;
        }
        draft.update(|d| {
            let Some(zones) = d
                .get_mut("config")
                .and_then(|c| c.get_mut("zones"))
                .and_then(|z| z.as_object_mut())
            else {
                return;
            };
            for (slug, z) in zones.iter_mut() {
                let Some(obj) = z.as_object_mut() else {
                    continue;
                };
                let holds_this = obj
                    .get("soil_sensor_id")
                    .and_then(|v| v.as_str())
                    .map(|s| s == probe_id)
                    .unwrap_or(false);
                if slug == &zone_key {
                    obj.insert(
                        "soil_sensor_id".into(),
                        serde_json::Value::String(probe_id.clone()),
                    );
                } else if holds_this {
                    obj.insert("soil_sensor_id".into(), serde_json::Value::Null);
                }
            }
        });
        let candidate = draft.get_untracked();
        saving.set(true);
        #[cfg(feature = "hydrate")]
        leptos::task::spawn_local(async move {
            match save_draft(candidate).await {
                Ok(()) => toast.success("Binding saved."),
                Err(e) => toast.error(format!("Could not save binding: {e}")),
            }
            saving.set(false);
        });
        #[cfg(not(feature = "hydrate"))]
        {
            saving.set(false);
            let _ = (candidate, toast);
        }
    });

    // Body: a card per gateway with its probed channels, or the friendly
    // skip-fine empty state when there is no gateway / no probe to bind.
    let body = move || {
        let d = draft.get();
        let gws = draft_gateways(&d);
        let opts = draft_zone_opts(&d);
        let probed = probes.get();

        if gws.is_empty() {
            return no_gateway_state().into_any();
        }
        if opts.is_empty() {
            return no_zones_state().into_any();
        }

        let is_probing = probing.get();
        let any_probe = probed.iter().any(|(_, ch)| !ch.is_empty());
        let cards: Vec<_> = gws
            .into_iter()
            .map(|gw| {
                let channels = probed
                    .iter()
                    .find(|(sid, _)| sid == &gw.source_id)
                    .map(|(_, ch)| ch.clone())
                    .unwrap_or_default();
                gateway_card(&d, gw, channels, opts.clone(), bind, is_probing)
            })
            .collect();

        // While a probe is in flight, the "no probes" hint would be a false
        // negative, so suppress it until the read settles.
        let show_no_probes_hint = !any_probe && !is_probing;

        view! {
            <Panel title="Soil probes on your gateway".to_string()>
                <p class="sensors-section__hint" style="margin-bottom: var(--space-4)">
                    "Read live off each gateway you added in the Weather step. Bind a probe "
                    "to a zone to let it drive that zone's skip decision once setup is applied."
                </p>
                {cards}
                {show_no_probes_hint.then(no_probes_hint)}
            </Panel>
        }
        .into_any()
    };

    view! {
        <div class="setup-step">
            <h2 class="setup-step__title">"Match probes to zones"<HelpHint topic="soil-sensors"/></h2>
            <p class="setup-step__body">
                "If you have soil-moisture probes on an Ecowitt gateway, tell LocalSky which "
                "zone each one sits in. A bound probe lets the engine skip a zone that is "
                "already wet enough, instead of relying on the weather model alone. This is "
                "optional, you can do it any time under Settings, Sensors."
            </p>

            <div class="scan-panel">
                <button
                    type="button"
                    class="setup-footer__btn setup-footer__btn--ghost scan-panel__btn"
                    prop:disabled=move || probing.get()
                    on:click=on_reprobe
                >
                    <Icon name="refresh" size=15/>
                    {move || if probing.get() { " Reading gateways…" } else { " Re-read probes" }}
                </button>
                {move || {
                    let e = probe_err.get();
                    (!e.is_empty()).then(|| view! {
                        <p class="setup-test-result is-err">{e}</p>
                    })
                }}
            </div>

            {body}

            <SetupFooter
                prev=prev_step_href("sensors")
                next=next_step_href("sensors")
            />
        </div>
    }
}

// ---------------------------------------------------------------------------
// Probe orchestration (hydrate-only network calls; the SSR build links a
// no-op so the component type-checks for both targets).
// ---------------------------------------------------------------------------

#[cfg(feature = "hydrate")]
async fn probe_all(
    gateways: Vec<DraftGateway>,
    probes: RwSignal<Vec<(String, Vec<ProbeChannel>)>>,
    probing: RwSignal<bool>,
    probe_err: RwSignal<String>,
) {
    if gateways.is_empty() {
        probes.set(Vec::new());
        return;
    }
    probing.set(true);
    probe_err.set(String::new());
    let mut out: Vec<(String, Vec<ProbeChannel>)> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    for gw in gateways {
        match probe_one(&gw).await {
            Ok(channels) => out.push((gw.source_id, channels)),
            Err(e) => {
                errors.push(format!("{}: {e}", gw.host));
                // Keep an empty entry so the card still renders with a hint.
                out.push((gw.source_id, Vec::new()));
            }
        }
    }
    probes.set(out);
    if !errors.is_empty() {
        probe_err.set(format!("Could not read {}.", errors.join("; ")));
    }
    probing.set(false);
}

#[cfg(feature = "hydrate")]
async fn probe_one(gw: &DraftGateway) -> Result<Vec<ProbeChannel>, String> {
    let body = serde_json::json!({ "host": gw.host, "source_id": gw.source_id });
    let resp = gloo_net::http::Request::post("/api/wizard/probe_soil")
        .json(&body)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let v = resp
        .json::<serde_json::Value>()
        .await
        .map_err(|e| e.to_string())?;
    if v.get("ok").and_then(|b| b.as_bool()) != Some(true) {
        return Err(v
            .get("detail")
            .and_then(|d| d.as_str())
            .unwrap_or("gateway unreachable")
            .to_string());
    }
    let channels = v
        .get("channels")
        .and_then(|c| c.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|c| {
                    Some(ProbeChannel {
                        channel: c.get("channel")?.as_str()?.to_string(),
                        id: c.get("id")?.as_str()?.to_string(),
                        moisture_pct: c.get("moisture_pct").and_then(|v| v.as_f64()),
                        battery_pct: c.get("battery_pct").and_then(|v| v.as_f64()),
                        temp_f: c.get("temp_f").and_then(|v| v.as_f64()),
                        ec: c.get("ec").and_then(|v| v.as_f64()),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(channels)
}

// ---------------------------------------------------------------------------
// Pieces (plain fns to keep each view tree in its own monomorphization
// boundary, per the no-deep-nesting rule).
// ---------------------------------------------------------------------------

/// One gateway card with its probed soil channels nested inside.
fn gateway_card(
    draft: &serde_json::Value,
    gw: DraftGateway,
    channels: Vec<ProbeChannel>,
    zone_opts: Vec<ZoneOpt>,
    bind: Callback<(String, String)>,
    probing: bool,
) -> impl IntoView {
    let count = channels.len();
    let count_label = format!("{count} probe{}", if count == 1 { "" } else { "s" });
    let host = gw.host.clone();
    // Friendly title (e.g. "Ecowitt gateway (10.0.0.50)"), never the slug.
    let title = gateway_title(&gw.kind, &gw.host);
    let kind = kind_label(&gw.kind);

    // While the probe request is in flight and we have nothing cached for
    // this gateway yet, show a clear reading state instead of an empty card.
    if channels.is_empty() && probing {
        return view! {
            <section class="sensor-detail-card">
                <div class="soil-card__head">
                    <span class="soil-card__name">{title}</span>
                    <span class="source-health__kind">{kind}</span>
                </div>
                <div class="source-health__status" style="display:flex; align-items:center; gap:0.5rem">
                    <Icon name="refresh" size=14/>
                    <span>"Reading probes…"</span>
                </div>
            </section>
        }
        .into_any();
    }

    if channels.is_empty() {
        return view! {
            <section class="sensor-detail-card">
                <div class="soil-card__head">
                    <span class="soil-card__name">{title}</span>
                    <span class="source-health__kind">{kind}</span>
                </div>
                <div class="source-health__status">{format!("{host} answered, no soil probes reported yet")}</div>
            </section>
        }
        .into_any();
    }

    let rows: Vec<_> = channels
        .into_iter()
        .map(|c| {
            let bound = bound_zone_for(draft, &c.id);
            soil_probe_row(c, bound, zone_opts.clone(), bind)
        })
        .collect();

    view! {
        <section class="sensor-detail-card">
            <div class="soil-card__head">
                <span class="soil-card__name">{title}</span>
                <span class="source-health__kind">{kind}</span>
            </div>
            <div class="source-health__status" style="display:flex; align-items:center; gap:0.5rem">
                <span class="source-health__dot" style="background: var(--verdict-run)"></span>
                <span>{host}</span>
                <span style="color: var(--text-faint)">"·"</span>
                <span>{count_label}</span>
            </div>
            <div class="soil-grid">
                {rows}
            </div>
        </section>
    }
    .into_any()
}

/// One probe card: live reading, optional chips, and the bind-to-zone select.
fn soil_probe_row(
    c: ProbeChannel,
    bound_slug: Option<String>,
    zone_opts: Vec<ZoneOpt>,
    bind: Callback<(String, String)>,
) -> impl IntoView {
    let probe_id = c.id.clone();
    let label = format!("Channel {}", c.channel);
    let reading = c
        .moisture_pct
        .map(|m| format!("{m:.1}%"))
        .unwrap_or_else(|| "—".to_string());
    let selected_slug = bound_slug.clone().unwrap_or_default();

    // Chips: battery, temp, EC, shown only when present (Icon, no emoji).
    let mut chips: Vec<_> = Vec::new();
    if let Some(b) = c.battery_pct {
        // Plain-language qualifier so a bare "100%" reads as a status, not a
        // mystery number. Under ~20% flags Low (and tints the pill amber).
        let low = b < 20.0;
        let qualifier = if low { "Low" } else { "OK" };
        // Static style strings (not format!) so every chip in this Vec shares
        // one concrete view type.
        let pill_style = if low {
            "--sc: var(--warn, #d9a200); display:inline-flex; align-items:center; gap:0.25rem"
        } else {
            "--sc: var(--verdict-run); display:inline-flex; align-items:center; gap:0.25rem"
        };
        chips.push(view! {
            <span class="soil-card__pill" style=pill_style>
                <Icon name="zap" size=12/>
                {format!("{b:.0}% {qualifier}")}
            </span>
        });
    }
    if let Some(t) = c.temp_f {
        chips.push(view! {
            <span class="soil-card__pill" style="--sc: var(--accent); display:inline-flex; align-items:center; gap:0.25rem">
                <Icon name="thermometer" size=12/>
                {format!("{t:.0}°F")}
            </span>
        });
    }
    if let Some(e) = c.ec {
        chips.push(view! {
            <span class="soil-card__pill" style="--sc: var(--accent); display:inline-flex; align-items:center; gap:0.25rem">
                <Icon name="activity" size=12/>
                {format!("EC {e:.0}")}
            </span>
        });
    }

    let bind_id = probe_id.clone();
    let opts_for_select = zone_opts.clone();

    view! {
        <div class="soil-card" style="--sc: var(--accent)">
            <div class="soil-card__head">
                <span class="soil-card__name">{label}</span>
            </div>
            <div class="soil-card__value">{reading}</div>
            <div class="zone-soil-live" style="margin-top:0; flex-wrap:wrap">
                {chips}
            </div>
            <label class="sensor-readout__k" style="margin-top:0.4rem">"Bound zone"</label>
            <select
                class="ui-input"
                on:change=move |ev| {
                    bind.run((bind_id.clone(), event_target_value(&ev)));
                }
            >
                <option value="" selected=selected_slug.is_empty()>
                    "Not bound"
                </option>
                {opts_for_select
                    .into_iter()
                    .map(|(slug, name)| {
                        let sel = slug == selected_slug;
                        view! { <option value=slug selected=sel>{name}</option> }
                    })
                    .collect_view()}
            </select>
        </div>
    }
}

/// Shown above the cards when gateways answered but no probe is reporting:
/// nudge to plug a probe in, then re-read.
fn no_probes_hint() -> impl IntoView {
    view! {
        <p class="sensors-section__hint" style="margin-top: var(--space-3)">
            "No soil probes are reporting on your gateway(s) yet. Make sure each WH51/WS-soil "
            "probe is paired to the gateway (in the Ecowitt WS View app) and reading, then use "
            "Re-read probes above. You can also bind them later under Settings, Sensors. "
            <a href=doc_url("first-soil-sensor") target="_blank" rel="noopener noreferrer"
                style="color: var(--accent)">
                "Add your first soil sensor"
            </a>
            "."
        </p>
    }
}

/// Friendly skip-fine state: no Ecowitt gateway in the draft at all.
fn no_gateway_state() -> impl IntoView {
    view! {
        <Panel title="Soil probes".to_string()>
            <div class="sensors-empty">
                <p style="margin:0 0 var(--space-3); color: var(--text-bright); font-weight: 600">
                    "No soil gateway added, that's fine."
                </p>
                <p style="margin:0 0 var(--space-3)">
                    "A soil probe never connects to LocalSky directly: it rides in through a "
                    "source. Add one and its probes show up here, ready to bind to a zone. "
                    "Without a probe, the engine schedules from the weather model alone, which "
                    "works well. Three ways in:"
                </p>
                <ul class="sensors-empty__ways">
                    <li>
                        <strong>"Ecowitt gateway"</strong>
                        " (recommended, cheapest). Go back to the Weather step, Scan my network, "
                        "and Add it; its probes appear here when you return."
                    </li>
                    <li>
                        <strong>"Any MQTT-published probe"</strong>
                        ". Add an MQTT source, point a soil subscription at the topic, and bind "
                        "it to a zone. Doable here or under Settings, Sensors."
                    </li>
                    <li>
                        <strong>"A Home Assistant soil entity"</strong>
                        ". Bridge HA, then pick the probe from the zone editor; it mirrors in "
                        "automatically. Doable after setup under Settings."
                    </li>
                </ul>
                <p style="margin: var(--space-3) 0 0">
                    <a href="/setup/sources" style="color: var(--accent)">"← Back to Weather sources"</a>
                    "  ·  "
                    <a href=doc_url("first-soil-sensor") target="_blank" rel="noopener noreferrer"
                        style="color: var(--accent)">
                        "Add your first soil sensor"
                    </a>
                </p>
            </div>
        </Panel>
    }
}

/// Skip-fine state when there are gateways but no zones to bind to yet.
fn no_zones_state() -> impl IntoView {
    view! {
        <Panel title="Soil probes".to_string()>
            <div class="sensors-empty">
                <p style="margin:0 0 var(--space-3); color: var(--text-bright); font-weight: 600">
                    "Define a zone first."
                </p>
                <p style="margin:0 0 var(--space-3)">
                    "Probes bind to zones, so there is nothing to bind to until you add at least "
                    "one zone. You can add probes any time later under Settings, Sensors."
                </p>
                <p style="margin: var(--space-3) 0 0">
                    <a href="/setup/zones" style="color: var(--accent)">"← Back to Zones"</a>
                    "  ·  "
                    <a href=doc_url("first-soil-sensor") target="_blank" rel="noopener noreferrer"
                        style="color: var(--accent)">
                        "Add your first soil sensor"
                    </a>
                </p>
            </div>
        </Panel>
    }
}
