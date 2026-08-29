// ControllerEditorPanel: a self-contained add/edit form for one irrigation
// controller, mirroring sources_form::SourceEditorPanel's contract so both the
// Settings -> Controllers page and the unified Devices hub share one form.
//
// Seeds from `existing` (None = add). On save it parses the config JSON,
// assembles the `{id, default, enabled, kind, config}` entry, and hands it to
// `on_commit`, the caller persists (and is responsible for the `default`
// mutual-exclusivity across controllers). `on_cancel` dismisses the form.
//
// Keeps the "Scan zones" probe (POST /api/v1/wizard/scan_zones): it lists a
// controller's stations from the draft config without saving first, and for
// the cloud kinds that hold a zone map in their config (Rachio, Hydrawise,
// B-hyve, Rain Bird) it merges the results into that map in the JSON below
// (see merge::merge_scanned_zones). Kinds that bind zones through zone
// entries instead (OpenSprinkler, DIY boards, HA, dry run) get an honest
// message pointing at the Zones page.

use leptos::prelude::*;

use crate::components::sources_form::{normalize_source_id_full, normalize_source_id_input};
use crate::components::ui::{Button, FormField, Panel};
use crate::docs::doc_url;

mod fields;
mod merge;
pub use fields::{controller_fields, ControllerConfigForm};
pub use merge::{
    apply_zone_binds, bind_message, merge_message, merge_scanned_zones, scan_opens_advanced,
    zone_slug, BindOutcome, DiscoveredZone, MergeOutcome, ZoneBind,
};

/// Controller kinds offered in the Kind picker. (value, label) pairs.
///
/// `esphome_native` is intentionally NOT offered: the backend adapter is
/// deferred (runtime::build_controllers warn-skips it), so a saved
/// esphome_native controller would silently water nothing. ESPHome users go
/// through `mqtt_command` (the path the bundled reference firmware implements)
/// or `http_generic`. Keep this list in lockstep with the ControllerKind enum
/// (schema.rs); the `kind_picker_covers_buildable_controllers` test guards it.
pub fn controller_kind_options() -> Vec<(String, String)> {
    vec![
        ("opensprinkler_direct".into(), "OpenSprinkler".into()),
        ("http_generic".into(), "DIY board (HTTP)".into()),
        ("mqtt_command".into(), "DIY board (MQTT)".into()),
        ("ha_service_call".into(), "Home Assistant".into()),
        ("rachio".into(), "Rachio".into()),
        ("hydrawise".into(), "Hydrawise".into()),
        ("bhyve".into(), "B-hyve".into()),
        ("rainbird".into(), "Rain Bird".into()),
        ("dry_run".into(), "No hardware (simulate)".into()),
    ]
}

/// Controller kinds that can enumerate their own zones, i.e. the kinds whose
/// adapter implements `discover_zones` and for which POST
/// /api/v1/wizard/scan_zones returns a zone list rather than an error.
///
/// Client code gates on this BEFORE firing a scan, because the server cannot
/// tell the caller "this kind cannot scan" by status code alone: Hydrawise,
/// B-hyve and Rain Bird reach the adapter and answer 502 zone_scan_failed
/// with the Unsupported detail, while MQTT, Home Assistant and ESPHome are
/// refused earlier with 422 controller_unsupported. Both look like a
/// transient failure from the outside, and asking costs a round trip to
/// learn nothing.
///
/// Keep this in lockstep with the adapters: the
/// `scannable_kinds_are_all_real_controller_kinds` test pins every entry to
/// `controller_kind_options()`, and an adapter that grows a
/// `discover_zones` impl belongs here the same day.
pub fn can_scan_zones(kind: &str) -> bool {
    matches!(
        kind,
        "rachio" | "opensprinkler_direct" | "http_generic" | "dry_run"
    )
}

/// Controller kinds grouped by the user's MENTAL MODEL instead of a flat
/// nine-wide protocol strip, mirroring `sources_form::kind_groups`. Each group
/// carries a short caption answering "is this me?" and lists its kinds in
/// onboarding order. Returns `(group title, group caption, [kind value, ...])`.
/// Every kind in `controller_kind_options()` appears in exactly one group; the
/// `every_controller_kind_is_in_exactly_one_group` test guards that coverage so
/// a newly-added kind can't silently vanish from the grouped picker.
pub fn controller_kind_groups() -> Vec<(&'static str, &'static str, Vec<&'static str>)> {
    vec![
        (
            "On your network",
            "Hardware that fires valves over your own LAN, no cloud account.",
            vec!["opensprinkler_direct", "http_generic", "mqtt_command"],
        ),
        (
            "Cloud account",
            "A vendor controller you drive through its cloud API using your account.",
            vec!["rachio", "hydrawise", "bhyve", "rainbird"],
        ),
        (
            "Home Assistant",
            "Let Home Assistant fire the valves; LocalSky calls an HA service.",
            vec!["ha_service_call"],
        ),
        (
            "Try it out",
            "No irrigation hardware yet? Simulate runs to explore scheduling without firing a single valve.",
            vec!["dry_run"],
        ),
    ]
}

/// True for the one recommended zero-hardware pick (DryRun): waters nothing,
/// works immediately, lets a hardware-less user explore scheduling. The picker
/// marks it so someone with no controller knows exactly where to start.
pub fn is_recommended_controller(kind: &str) -> bool {
    kind == "dry_run"
}

/// A plain-language "what this does" line for a controller kind, written for
/// someone who is NOT sure what they're looking at. Mirrors
/// `sources_form::kind_blurb`: what it is plus the one fact that decides whether
/// they can use it now (talks on the LAN, needs a cloud account/key, simulates).
pub fn controller_blurb(kind: &str) -> &'static str {
    match kind {
        "opensprinkler_direct" => {
            "An OpenSprinkler box on your network. LocalSky talks to it directly over the LAN: \
             just its IP (and the device password if you set one)."
        }
        "http_generic" => {
            "A DIY board (ESP32 or similar) that exposes a simple HTTP on/off contract. LocalSky \
             calls it directly on the LAN; bring the board's URL."
        }
        "mqtt_command" => {
            "A DIY board that listens on MQTT (the path the bundled reference firmware uses). \
             LocalSky publishes on/off commands to your broker; bring the broker's address."
        }
        "ha_service_call" => {
            "Already run your sprinklers through Home Assistant? LocalSky fires them by calling an \
             HA service. Needs your HA URL and a long-lived token."
        }
        "rachio" => {
            "Your Rachio controller, driven through the Rachio cloud. Needs an API token from \
             your Rachio account."
        }
        "hydrawise" => {
            "Your Hunter Hydrawise controller, driven through the Hydrawise cloud. Needs an API \
             key from your Hydrawise account."
        }
        "bhyve" => {
            "Your Orbit B-hyve controller, driven through the B-hyve cloud. Signs in with your \
             B-hyve account email and password."
        }
        "rainbird" => {
            "Your Rain Bird controller, driven through the Rain Bird cloud. Signs in with your \
             Rain Bird account email and password."
        }
        "dry_run" => {
            "No irrigation hardware? This simulates watering so you can explore scheduling and the \
             skip rules without firing a single valve. Add real hardware later in Settings."
        }
        _ => "Fires your irrigation valves.",
    }
}

/// Per-kind starter config JSON, auto-filled while adding.
pub fn default_config_text(kind: &str) -> String {
    match kind {
        "opensprinkler_direct" => "{\n  \"host\": \"192.0.2.10\",\n  \"port\": 80,\n  \"password_md5\": \"\",\n  \"poll_interval_s\": 10\n}".into(),
        "ha_service_call" => "{\n  \"base_url\": \"http://homeassistant.local:8123\",\n  \"bearer_token\": \"${HA_LONG_LIVED_TOKEN}\",\n  \"start_service\": \"script.os_zone_toggle\",\n  \"stop_service\": \"opensprinkler.stop\",\n  \"zone_entity_map\": {}\n}".into(),
        "http_generic" => "{\n  \"base_url\": \"http://192.0.2.50\",\n  \"bearer_token\": null,\n  \"poll_interval_s\": 10\n}".into(),
        "rachio" => "{\n  \"api_token\": \"\",\n  \"device_id\": \"\",\n  \"zone_uuid_map\": {}\n}".into(),
        "hydrawise" => "{\n  \"api_key\": \"\",\n  \"controller_id\": 0,\n  \"zone_relay_map\": {}\n}".into(),
        "bhyve" => "{\n  \"email\": \"\",\n  \"password\": \"\",\n  \"device_id\": \"\",\n  \"zone_station_map\": {}\n}".into(),
        "rainbird" => "{\n  \"email\": \"\",\n  \"password\": \"\",\n  \"controller_id\": \"\",\n  \"zone_station_map\": {},\n  \"base_url\": \"https://rdz-rest.rainbird.com\"\n}".into(),
        "mqtt_command" => "{\n  \"broker_host\": \"broker.local\",\n  \"broker_port\": 1883,\n  \"username\": null,\n  \"password\": null,\n  \"availability_topic\": \"localsky-irrig/status\",\n  \"flow_topic\": null,\n  \"zone_command_map\": {\n    \"back_yard\": {\n      \"topic\": \"localsky-irrig/switch/zone_1/command\",\n      \"on_payload\": \"ON\",\n      \"off_payload\": \"OFF\",\n      \"retain\": false,\n      \"state_topic\": \"localsky-irrig/switch/zone_1/state\",\n      \"state_on_payload\": \"ON\"\n    }\n  }\n}".into(),
        "dry_run" => "{\n  \"simulate_runs\": false\n}".into(),
        _ => "{}".into(),
    }
}

/// Grouped controller-kind picker. Renders the kinds under the mental-model
/// groups from `controller_kind_groups()` (each with a caption), instead of a
/// flat nine-wide pill strip. One option is active at a time; selecting one
/// writes its kind string into `value`. The recommended zero-hardware pick
/// (DryRun) carries a "Recommended" tag in its tile so a hardware-less user
/// knows where to start. Keyboard + screen-reader friendly: a labeled
/// radiogroup per group, native buttons as radios. Mirrors
/// `sources_form::KindPicker`.
#[component]
fn ControllerKindPicker(
    /// The selected controller kind (two-way; the active tile reflects it).
    value: RwSignal<String>,
) -> impl IntoView {
    view! {
        <div class="source-kind-picker">
            {controller_kind_groups()
                .into_iter()
                .map(|(title, caption, kinds)| {
                    let group_label = title.to_string();
                    view! {
                        <div class="source-kind-group" role="radiogroup" aria-label=group_label>
                            <div class="source-kind-group__head">
                                <span class="source-kind-group__title">{title}</span>
                                <span class="source-kind-group__caption">{caption}</span>
                            </div>
                            <div class="source-kind-group__options">
                                {kinds
                                    .into_iter()
                                    .map(|k| controller_kind_tile(k, value))
                                    .collect_view()}
                            </div>
                        </div>
                    }
                })
                .collect_view()}
        </div>
    }
}

/// One selectable tile in the grouped controller picker. A free function (not
/// inline view! nesting) so each tile monomorphizes in its own boundary,
/// keeping recursion depth flat per the no-deep-nesting guidance.
fn controller_kind_tile(kind: &'static str, value: RwSignal<String>) -> impl IntoView {
    let label = controller_kind_options()
        .into_iter()
        .find(|(v, _)| v == kind)
        .map(|(_, l)| l)
        .unwrap_or_else(|| kind.to_string());
    let recommended = is_recommended_controller(kind);
    view! {
        <button
            class="source-kind-tile"
            class:source-kind-tile--active=move || value.get() == kind
            class:source-kind-tile--recommended=recommended
            role="radio"
            aria-checked=move || (value.get() == kind).to_string()
            type="button"
            on:click=move |_| value.set(kind.to_string())
        >
            <span class="source-kind-tile__label">{label}</span>
            {recommended.then(|| view! {
                <span class="source-kind-tile__tag">"Recommended"</span>
            })}
        </button>
    }
}

/// Seed the bulk-bind table from a fresh scan, keeping any choice the user
/// already made for a station id the controller still reports. Returns
/// `None` when the panel was disposed mid-scan (a bare signal read then
/// panics, and wasm release is panic=abort).
#[cfg_attr(not(feature = "hydrate"), allow(dead_code))]
fn station_bind_rows(
    scanned: RwSignal<Vec<(String, String, String)>>,
    found: &[DiscoveredZone],
    zone_options: &[(String, String, String)],
) -> Option<()> {
    let prior = scanned.try_get_untracked()?;
    let rows: Vec<(String, String, String)> = found
        .iter()
        // A vendor zone with no id cannot bind anything, and two of them
        // would collide in the prior-choice lookup below. Drop them here so
        // an unbindable row never renders.
        .filter(|z| !z.station_id.trim().is_empty())
        .map(|z| {
            let keep = prior
                .iter()
                .find(|(id, _, _)| id == &z.station_id)
                .map(|(_, _, slug)| slug.clone())
                // Fall back to the zone ALREADY bound to this vendor id.
                // Without this the first scan of a session shows every row
                // as "(leave unbound)" even for correctly bound zones, and
                // the hint ("a zone you leave blank is not changed") reads
                // as "nothing here is bound yet" -- which is how this
                // feature would talk a user into repointing a working
                // install while he is checking it.
                .or_else(|| {
                    zone_options
                        .iter()
                        .find(|(_, _, station)| station.trim() == z.station_id.trim())
                        .map(|(slug, _, _)| slug.clone())
                })
                .unwrap_or_default();
            (z.station_id.clone(), z.name.clone(), keep)
        })
        .collect();
    scanned.try_set(rows).is_none().then_some(())
}

#[component]
pub fn ControllerEditorPanel(
    #[prop(default = None)] existing: Option<serde_json::Value>,
    on_commit: Callback<serde_json::Value>,
    on_cancel: Callback<()>,
    /// Ids of the OTHER configured controllers (all except the one being edited).
    /// Used to reject a rename that collides with a sibling up front, in-form,
    /// instead of corrupting the local config and bouncing off a server 422
    /// (parity with SourceEditorPanel).
    #[prop(optional)]
    sibling_ids: Vec<String>,
    /// Existing LocalSky zones as (slug, display name, current
    /// controller_station), for the bulk-bind table a scan produces. Empty
    /// (the default) hides the table entirely.
    ///
    /// The station is load-bearing, not decoration: it is what lets each row
    /// pre-select the zone already bound to that vendor id, so a user who
    /// opens the editor to CHECK a working install is not shown seven rows
    /// reading "(leave unbound)" and invited to rebuild bindings that were
    /// already correct.
    #[prop(optional)]
    zone_options: Vec<(String, String, String)>,
    /// Apply a set of zone bindings the user chose in the bulk-bind table.
    ///
    /// This is the one place a CONTROLLER editor writes the ZONES half of the
    /// config, so it is threaded explicitly rather than folded into
    /// `on_commit`: the caller owns the config and decides whether its page
    /// persists immediately (Devices) or stages behind a Save (Controllers).
    /// Absent means the table renders no Apply and is not offered.
    #[prop(optional)]
    on_bind_zones: Option<Callback<Vec<ZoneBind>>>,
) -> impl IntoView {
    let editing = existing.is_some();
    let seed_id = existing
        .as_ref()
        .and_then(|s| s.get("id"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let seed_kind = existing
        .as_ref()
        .and_then(|s| s.get("kind"))
        .and_then(|v| v.as_str())
        .unwrap_or("opensprinkler_direct")
        .to_string();
    // Keep the stored kind VERBATIM. Kind is identity and is locked on edit now
    // (the picker is hidden), so we must NOT coerce a no-longer-offered kind
    // (e.g. the deferred esphome_native) to a working default: that would
    // silently re-commit a DIFFERENT kind, mismatched against the stored config,
    // with no picker for the user to notice or correct. On edit the locked hint
    // already tells them to remove + re-add to change the type; meanwhile editing
    // an unrelated field (priority/enabled) still saves against the real kind.
    let seed_default = existing
        .as_ref()
        .and_then(|s| s.get("default"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let seed_enabled = existing
        .as_ref()
        .and_then(|s| s.get("enabled"))
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let seed_config = existing
        .as_ref()
        .and_then(|s| s.get("config"))
        .map(|c| serde_json::to_string_pretty(c).unwrap_or_else(|_| "{}".into()))
        .unwrap_or_else(|| default_config_text(&seed_kind));

    // The id as it was when the form opened, so a rename can tell the caller
    // which slot to replace and which references (every zone's controller_id) to
    // migrate. Mirrors sources_form::SourceEditorPanel.
    let original_id = seed_id.clone();
    let id = RwSignal::new(seed_id);
    // True once the user actually types in the ID field. Without this, saving an
    // unrelated field (default/enabled) on a controller whose STORED id is a
    // legacy non-slug would full-normalize the id on save, differ from the
    // original, and trigger a spurious rename the user never asked for. We only
    // rename when the ID field was edited.
    let id_touched = RwSignal::new(false);
    let kind = RwSignal::new(seed_kind);
    let default_flag = RwSignal::new(seed_default);
    let enabled = RwSignal::new(seed_enabled);
    let config_text = RwSignal::new(seed_config);
    let error = RwSignal::new(String::new());
    let scan_msg = RwSignal::new(String::new());
    // The last scan's zones, and the LocalSky zone chosen for each. This is
    // what issue #8's reporter had no way to express: he scanned seven zones
    // and the editor gave him nowhere to say which was which.
    // (station_id, vendor name, chosen slug; empty slug = leave unbound).
    let scanned: RwSignal<Vec<(String, String, String)>> = RwSignal::new(Vec::new());
    let bind_msg = RwSignal::new(String::new());
    // Whether this mount can write bindings at all (a saved controller in a
    // page that threaded the callback), vs. whether there is anything to
    // bind TO. The two reasons the table cannot act read differently, so
    // they are kept apart rather than collapsed into one message.
    let can_bind_here = on_bind_zones.is_some();
    let bindable = !zone_options.is_empty() && can_bind_here;
    // The scan continuation is detached (the panel can be disposed mid-scan),
    // so it cannot borrow `zone_options` directly. StoredValue is Copy and
    // reads with try_* like every other signal in that continuation.
    #[cfg_attr(not(feature = "hydrate"), allow(unused_variables))]
    let zone_options_for_rows = StoredValue::new(zone_options.clone());

    // While adding, swap the JSON template as the kind changes.
    #[cfg(feature = "hydrate")]
    if !editing {
        Effect::new(move |_| {
            let k = kind.get();
            config_text.set(default_config_text(&k));
        });
    }

    let on_save = move |_| {
        // The id we will store. If the user never edited the ID field, keep the
        // stored id VERBATIM (do not full-normalize a legacy non-slug id, which
        // would look like a rename the user never asked for). Only when they
        // actually typed do we normalize to a clean slug. Mirrors SourceEditorPanel.
        let id_v = if id_touched.get() {
            normalize_source_id_full(&id.get())
        } else {
            original_id.clone()
        };
        if id_v.is_empty() {
            error.set("Controller id is required".into());
            return;
        }
        let is_rename = editing && original_id != id_v;
        // Reject a rename that collides with another controller up front, in-form,
        // instead of corrupting the local config + bouncing off a server 422.
        if is_rename && sibling_ids.iter().any(|s| s == &id_v) {
            error.set(format!(
                "A controller with id \"{id_v}\" already exists. Pick a different id."
            ));
            return;
        }
        let cfg_value: serde_json::Value = match serde_json::from_str(&config_text.get()) {
            Ok(v) => v,
            Err(e) => {
                error.set(format!("Config JSON parse error: {e}"));
                return;
            }
        };
        error.set(String::new());
        let mut payload = serde_json::json!({
            "id": id_v,
            "default": default_flag.get(),
            "enabled": enabled.get(),
            "kind": kind.get(),
            "config": cfg_value,
        });
        // On a RENAME carry the old id so the caller replaces the right slot,
        // repoints every zone's controller_id to the new id, and resolves the
        // entry's redacted secrets from the old id.
        if is_rename {
            payload["old_id"] = serde_json::Value::String(original_id.clone());
        }
        on_commit.run(payload);
    };

    let on_scan = move |_| {
        let cfg_value: serde_json::Value = match serde_json::from_str(&config_text.get()) {
            Ok(v) => v,
            Err(e) => {
                scan_msg.set(format!("Config JSON parse error: {e}"));
                return;
            }
        };
        // Consumed only by the hydrate-gated scan request below.
        #[allow(unused_variables)]
        let entry = serde_json::json!({
            "id": id.get(),
            "default": default_flag.get(),
            "enabled": enabled.get(),
            "kind": kind.get(),
            "config": cfg_value,
        });
        scan_msg.set("Scanning…".into());
        #[cfg(feature = "hydrate")]
        wasm_bindgen_futures::spawn_local(async move {
            let body = serde_json::json!({ "controller": entry });
            let req = match gloo_net::http::Request::post("/api/v1/wizard/scan_zones").json(&body) {
                Ok(r) => r,
                Err(e) => {
                    scan_msg.set(format!("encode failed: {e}"));
                    return;
                }
            };
            match req.send().await {
                Ok(resp) => {
                    let v = resp
                        .json::<serde_json::Value>()
                        .await
                        .unwrap_or(serde_json::Value::Null);
                    if let Some(zones) = v.get("zones").and_then(|z| z.as_array()) {
                        let found: Vec<DiscoveredZone> = zones
                            .iter()
                            .filter_map(|z| serde_json::from_value(z.clone()).ok())
                            .collect();
                        if found.is_empty() {
                            let _ = scanned.try_set(Vec::new());
                            scan_msg.set("No zones found on this controller.".into());
                            return;
                        }
                        // THE #8 FIX: merge the scan results into the config
                        // JSON's zone map (per kind), then report what actually
                        // happened. Re-parse config_text fresh: the user may
                        // have edited it while the scan was in flight.
                        // Detached continuation: reads are try_* with a clean
                        // bail (the panel can be disposed mid-scan; a bare
                        // read then panics and wasm-release is panic=abort).
                        let Some(text_now) = config_text.try_get_untracked() else {
                            return;
                        };
                        let mut cfg: serde_json::Value = match serde_json::from_str(&text_now) {
                            Ok(c) => c,
                            Err(e) => {
                                scan_msg.set(format!("Config JSON parse error: {e}"));
                                return;
                            }
                        };
                        let Some(kind_now) = kind.try_get_untracked() else {
                            return;
                        };
                        let outcome = merge_scanned_zones(&kind_now, &mut cfg, &found);
                        if scan_opens_advanced(&outcome) {
                            // The kind's zone map is still filled, and still
                            // read at dispatch as the fallback that keeps a
                            // pre-picker config watering. It is no longer the
                            // thing the user is sent to read: the bind table
                            // below is.
                            config_text.set(serde_json::to_string_pretty(&cfg).unwrap_or(text_now));
                        }
                        // FOLLOW-THROUGH (issue #8's other half). The scan used
                        // to force the Advanced fold open and scroll a wall of
                        // JSON into view, which told the user what was found
                        // but not how to say which vendor zone is which of
                        // theirs. Put the results on screen as a table of
                        // choices instead.
                        let Some(seeded) = zone_options_for_rows
                            .try_with_value(|opts| station_bind_rows(scanned, &found, opts))
                        else {
                            return;
                        };
                        if seeded.is_none() {
                            return;
                        }
                        bind_msg.set(String::new());
                        scan_msg.set(merge_message(found.len(), &outcome));
                    } else {
                        let detail = v
                            .get("detail")
                            .and_then(|d| d.as_str())
                            .unwrap_or("controller unreachable or kind not probeable");
                        scan_msg.set(format!("Scan failed: {detail}"));
                    }
                }
                Err(e) => scan_msg.set(format!("request failed: {e}")),
            }
        });
    };

    view! {
        <div id="controller-form-panel"><Panel title="Controller details".to_string()>
            <FormField
                label="ID".to_string()
                helptext="A short slug you control (e.g. os_main, ha_backup). Anything you type is normalized to snake_case as you go. Used by zones to reference this controller. You CAN rename it while editing: the rename repoints every zone that fires from this controller to the new id automatically, so nothing breaks.".to_string()
                error=Signal::derive(|| None::<String>)
            >
                <input
                    type="text"
                    class="ui-input"
                    placeholder="e.g. os_main"
                    prop:value=move || id.get()
                    on:input=move |ev| {
                        id_touched.set(true);
                        id.set(normalize_source_id_input(&event_target_value(&ev)));
                    }
                />
            </FormField>

            <FormField
                label=(if editing { "Type" } else { "What runs your sprinklers?" }).to_string()
                helptext=(if editing {
                    ""
                } else {
                    "Pick the group that matches what you have. No irrigation hardware yet? Choose No hardware (simulate) under Try it out."
                })
                .to_string()
                error=Signal::derive(|| None::<String>)
            >
                // On ADD the type is chosen here. On EDIT it is IDENTITY and LOCKED
                // (changing a live controller's kind would strand its config keys +
                // zone maps), so we show what it is + how to change it instead.
                {if editing {
                    view! {
                        <p class="sensors-section__hint">
                            "The type is fixed once a controller exists. To use a different type, remove this controller and add a new one."
                        </p>
                    }
                    .into_any()
                } else {
                    view! { <ControllerKindPicker value=kind/> }.into_any()
                }}
                // What-this-is panel: on ADD it updates as you pick; on EDIT it
                // explains the existing controller.
                <div class="source-pick" aria-live="polite">
                    <p class="source-pick__blurb">{move || controller_blurb(&kind.get())}</p>
                    {move || is_recommended_controller(&kind.get()).then(|| view! {
                        <p class="source-pick__zero-config">
                            <span class="source-pick__zero-config-tag">"Recommended"</span>
                            "Waters nothing, works immediately. The best choice if you "
                            "don't have irrigation hardware yet."
                        </p>
                    })}
                </div>
                <p class="ui-form-field__helptext" style="margin-top: 0.4rem">
                    "See the "
                    <a href=doc_url("controllers")
                        target="_blank" rel="noopener noreferrer"
                        style="color: var(--accent)">"controller docs"</a>
                    " for the capabilities of each."
                </p>
            </FormField>

            <FormField
                label="Make this the default?".to_string()
                helptext="If checked, other controllers lose default status. Zones without an explicit controller_id use the default.".to_string()
                error=Signal::derive(|| None::<String>)
            >
                <label style="display: flex; gap: 0.5rem; align-items: center; min-height: 44px;">
                    <input
                        type="checkbox"
                        prop:checked=move || default_flag.get()
                        on:input=move |ev| default_flag.set(event_target_checked(&ev))
                    />
                    "Set as default controller"
                </label>
            </FormField>

            <FormField
                label="Enabled".to_string()
                helptext="Unchecked controllers stay in the config but don't dispatch.".to_string()
                error=Signal::derive(|| None::<String>)
            >
                <label style="display: flex; gap: 0.5rem; align-items: center; min-height: 44px;">
                    <input
                        type="checkbox"
                        prop:checked=move || enabled.get()
                        on:input=move |ev| enabled.set(event_target_checked(&ev))
                    />
                    "Enable this controller"
                </label>
            </FormField>

            // PRIMARY editing surface: labeled, per-kind connection fields
            // (host/url/tokens/poll cadence), two-way-synced to config_text.
            // The JSON textarea below stays as the advanced escape hatch and
            // owns the nested zone maps. For the cloud kinds that keep a zone
            // map in their config, "Scan zones" merges results into it (the
            // merge module); zone-entry kinds bind zones on the Zones page.
            <Panel title="Connection".to_string()>
                <ControllerConfigForm config_text=config_text kind=Signal::derive(move || kind.get())/>
            </Panel>

            // BULK BIND. Each zone the controller reported, its id, and which
            // LocalSky zone fires it. This is the step the scan never had:
            // the results used to land as keys in a JSON blob, named after
            // the VENDOR's zones, which bound anything only when the two
            // sides happened to be named the same.
            {move || {
                let rows = scanned.get();
                if rows.is_empty() {
                    return None;
                }
                let opts = zone_options.clone();
                Some(view! {
                    <Panel title="Bind these zones".to_string()>
                        <p class="sensors-section__hint">
                            {if bindable {
                                "Choose which of your zones each of the controller's zones fires. \
                                 A zone you leave blank is not changed."
                            } else if can_bind_here {
                                "Add your zones under Settings, then Zones first; then scan again \
                                 here to say which of them each of these fires."
                            } else {
                                "Save this controller first, then reopen it here to say which of \
                                 your zones each of these fires."
                            }}
                        </p>
                        <ul class="zone-bind-list">
                            {rows
                                .into_iter()
                                .enumerate()
                                .map(|(i, (station_id, name, chosen))| {
                                    controller_bind_row(scanned, i, station_id, name, chosen, &opts, bindable)
                                })
                                .collect_view()}
                        </ul>
                        {bindable.then(|| view! {
                            <div class="settings-form-actions">
                                <Button
                                    variant="primary"
                                    aria_label="Bind the chosen zones to this controller".to_string()
                                    on_click=Callback::new(move |_| {
                                        let binds: Vec<ZoneBind> = scanned
                                            .get_untracked()
                                            .into_iter()
                                            .filter(|(_, _, slug)| !slug.is_empty())
                                            .map(|(station_id, vendor_name, slug)| ZoneBind {
                                                station_id,
                                                vendor_name,
                                                slug,
                                            })
                                            .collect();
                                        if binds.is_empty() {
                                            bind_msg.set(bind_message(&BindOutcome::Applied {
                                                bound: 0,
                                                replaced: 0,
                                            }));
                                            return;
                                        }
                                        if let Some(cb) = on_bind_zones {
                                            cb.run(binds);
                                            bind_msg.set(String::new());
                                        }
                                    })
                                >
                                    "Bind zones"
                                </Button>
                            </div>
                        })}
                        {move || {
                            let m = bind_msg.get();
                            (!m.is_empty()).then(|| view! { <p class="sensors-section__hint">{m}</p> })
                        }}
                    </Panel>
                })
            }}

            // ADVANCED: the raw config JSON escape hatch, demoted into a fold so
            // the labeled Connection form reads as the primary editor. Still
            // two-way synced; "Scan zones" populates the zone map in here, and a
            // successful merge auto-opens this fold and scrolls the JSON into
            // view (the ids below are the scan follow-through's DOM handles).
            <details class="settings-section-fold" id="controller-advanced-fold">
                <summary class="settings-section-fold__summary">
                    "Advanced: raw config JSON"
                    <span class="settings-section-fold__hint">"per-zone maps and other keys; for cloud controllers, Scan zones fills the zone map"</span>
                </summary>
                <div class="settings-section-fold__body">
                    <FormField
                        label="Config (JSON)".to_string()
                        helptext="Escape hatch for keys not in the labeled Connection form above, mainly the per-zone maps (zone_command_map, zone_*_map). Stays in sync both ways. For cloud controllers (Rachio, Hydrawise, B-hyve, Rain Bird), \"Scan zones\" fills the zone map here; other kinds bind their zones on the Zones page instead.".to_string()
                        error=Signal::derive(|| None::<String>)
                    >
                        <textarea
                            class="ui-input"
                            id="controller-config-json"
                            style="min-height: 180px; font-family: var(--font-mono); font-size: 0.85rem;"
                            prop:value=move || config_text.get()
                            on:input=move |ev| config_text.set(event_target_value(&ev))
                        ></textarea>
                    </FormField>
                </div>
            </details>

            {move || {
                let m = scan_msg.get();
                (!m.is_empty()).then(|| view! { <p class="sensors-section__hint">{m}</p> })
            }}
            {move || {
                let e = error.get();
                (!e.is_empty()).then(|| view! { <p class="source-editor__error">{e}</p> })
            }}

            <div class="settings-form-actions">
                <Button variant="ghost" on_click=Callback::new(move |_| on_cancel.run(()))>
                    "Cancel"
                </Button>
                <Button
                    variant="ghost"
                    on_click=Callback::new(on_scan)
                    aria_label="Probe the controller and list its zones/stations (no save needed)".to_string()
                >
                    "Scan zones"
                </Button>
                <Button variant="primary" on_click=Callback::new(on_save)>
                    {if editing { "Save controller changes" } else { "Add controller" }}
                </Button>
            </div>
        </Panel></div>
    }
}

/// One row of the bulk-bind table. A free function so each row
/// monomorphizes in its own boundary, per the no-deep-nesting guidance.
fn controller_bind_row(
    scanned: RwSignal<Vec<(String, String, String)>>,
    index: usize,
    station_id: String,
    name: String,
    chosen: String,
    zone_options: &[(String, String, String)],
    bindable: bool,
) -> impl IntoView {
    let opts: Vec<(String, String)> = zone_options
        .iter()
        .map(|(slug, label, _)| (slug.clone(), label.clone()))
        .collect();
    let already_bound = zone_options
        .iter()
        .any(|(slug, _, station)| slug == &chosen && station.trim() == station_id.trim());
    let id_label = station_id.clone();
    view! {
        <li class="zone-bind-row">
            <span class="zone-bind-row__name">{name}</span>
            <span class="zone-bind-row__id">{id_label}</span>
            // Say which rows are already wired, so a blank row unambiguously
            // means unbound rather than unknown.
            {already_bound.then(|| view! {
                <span class="zone-bind-row__state">"currently bound"</span>
            })}
            {bindable.then(|| view! {
                <select
                    class="ui-input"
                    aria-label=format!("LocalSky zone for controller zone {station_id}")
                    on:change=move |ev| {
                        let v = event_target_value(&ev);
                        scanned.update(|rows| {
                            if let Some(row) = rows.get_mut(index) {
                                row.2 = v;
                            }
                        });
                    }
                >
                    <option value="" selected=chosen.is_empty()>"(leave unbound)"</option>
                    {opts
                        .into_iter()
                        .map(|(slug, label)| {
                            let sel = slug == chosen;
                            let text = if label.is_empty() || label == slug {
                                slug.clone()
                            } else {
                                format!("{label} ({slug})")
                            };
                            view! { <option value=slug selected=sel>{text}</option> }
                        })
                        .collect_view()}
                </select>
            })}
        </li>
    }
}

#[cfg(all(test, feature = "ssr"))]
mod tests {
    use super::{
        controller_blurb, controller_kind_groups, controller_kind_options, default_config_text,
        is_recommended_controller,
    };

    // The grouped picker must show ALL of controller_kind_options(): a kind that
    // falls out of every group would silently vanish from the add-controller UI
    // (the exact failure mode that motivated the parity fix). Mirrors the
    // sources form's `every_kind_is_in_exactly_one_group`.
    #[test]
    fn every_controller_kind_is_in_exactly_one_group() {
        let flat: std::collections::BTreeSet<String> = controller_kind_options()
            .into_iter()
            .map(|(v, _)| v)
            .collect();
        let mut grouped: Vec<&'static str> = Vec::new();
        for (_title, _caption, kinds) in controller_kind_groups() {
            grouped.extend(kinds);
        }
        let grouped_set: std::collections::BTreeSet<String> =
            grouped.iter().map(|k| k.to_string()).collect();
        assert_eq!(
            grouped.len(),
            grouped_set.len(),
            "a controller kind appears in more than one picker group"
        );
        assert_eq!(
            grouped_set, flat,
            "grouped controller picker kinds must exactly match controller_kind_options()"
        );
    }

    #[test]
    fn dry_run_is_the_only_recommended_no_hardware_pick() {
        // DryRun is the zero-hardware default surfaced in the empty state.
        assert!(is_recommended_controller("dry_run"));
        for (kind, _label) in controller_kind_options() {
            assert_eq!(
                is_recommended_controller(&kind),
                kind == "dry_run",
                "only dry_run should be the recommended no-hardware pick (got {kind})"
            );
        }
    }

    #[test]
    fn every_controller_kind_has_a_real_blurb() {
        // Each kind needs a plain-language "what this does" line; the generic
        // fallback would mean a kind slipped through unlabeled.
        for (kind, label) in controller_kind_options() {
            let blurb = controller_blurb(&kind);
            assert_ne!(
                blurb, "Fires your irrigation valves.",
                "controller kind `{kind}` ({label}) has only the generic blurb fallback"
            );
            assert!(!blurb.is_empty());
        }
    }

    // Root-cause guard for the class of bug where a ControllerKind variant is
    // added to the backend but never surfaced in the UI (exactly how
    // `http_generic` shipped invisible). The sources form has the analogous
    // `coverage_kind_list_matches_default_config_text`; controllers had none.
    #[test]
    fn kind_picker_covers_buildable_controllers() {
        use crate::config::schema::ControllerKind as K;

        // Compile-time guard: adding a ControllerKind variant forces a new arm
        // here, i.e. a conscious "is this in the picker, or intentionally
        // hidden?" decision. The returned tag must match the serde
        // (tag = "kind", rename_all = "snake_case") wire string. Some(tag) =>
        // offered in the picker; None => deliberately hidden.
        fn classify(k: &K) -> Option<&'static str> {
            match k {
                K::OpensprinklerDirect(_) => Some("opensprinkler_direct"),
                K::HaServiceCall(_) => Some("ha_service_call"),
                K::Rachio(_) => Some("rachio"),
                K::Hydrawise(_) => Some("hydrawise"),
                K::Bhyve(_) => Some("bhyve"),
                K::Rainbird(_) => Some("rainbird"),
                K::MqttCommand(_) => Some("mqtt_command"),
                K::HttpGeneric(_) => Some("http_generic"),
                K::DryRun(_) => Some("dry_run"),
                // Deferred backend: build_controllers warn-skips it, so a saved
                // esphome_native controller would silently water nothing. Hidden
                // from the picker on purpose until the adapter is built.
                K::EsphomeNative(_) => None,
            }
        }
        // Referenced so the exhaustive match above is compiled (and thus its
        // E0004 guard enforced) even though we can't enumerate variant instances
        // without samples. NOTE: the runtime assertions below check a fixed
        // allowlist (`expected`), not classify's outputs; classify's job is the
        // compile-time "every variant is consciously handled" guard.
        let _ = classify;

        let picker: std::collections::BTreeSet<String> = controller_kind_options()
            .into_iter()
            .map(|(v, _)| v)
            .collect();

        // Every picker entry must have a real (non-empty) starter template.
        // This is the exact invariant http_generic violated: an entry with no
        // template falls through to "{}" and 422s on apply.
        for kind in &picker {
            let t = default_config_text(kind);
            assert!(
                t.starts_with('{') && t != "{}",
                "default_config_text(`{kind}`) must be a real template, got `{t}`"
            );
        }

        // The deferred backend must never be offered.
        assert!(
            !picker.contains("esphome_native"),
            "esphome_native is deferred and must stay out of the kind picker"
        );

        // The buildable kinds we expect to be selectable, and ONLY those.
        let expected = [
            "opensprinkler_direct",
            "http_generic",
            "mqtt_command",
            "ha_service_call",
            "rachio",
            "hydrawise",
            "bhyve",
            "rainbird",
            "dry_run",
        ];
        for kind in expected {
            assert!(
                picker.contains(kind),
                "kind picker is missing buildable controller `{kind}`"
            );
        }
        // Bidirectional: the picker has no surprise extras beyond `expected`
        // (so a typo'd or accidentally-added entry is caught too).
        assert_eq!(
            picker.len(),
            expected.len(),
            "kind picker has unexpected entries: {picker:?}"
        );
    }
}
