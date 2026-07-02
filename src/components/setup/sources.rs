// SourcesStep. The first-run weather + sources step. CLOUD is first-class here
// for EVERY user, side by side with LOCAL hardware, so both personas get full
// control of what data backs which reading:
//
//   * A no-hardware user lands set up with zero config (their region's keyless
//     feeds are already on, shown in the cloud list) and understands them.
//   * A hardware user adds their station AND sees the same cloud list as a
//     co-equal section, so they can turn on a cloud provider as a complement
//     (the wind-shadowed Tempest: keep the station for most readings, lean on a
//     cloud for wind) or a backup, understanding that local sensors always
//     outrank cloud.
//
// The two sections are co-equal: a LOCAL block (scan the network / add a
// station via the shared SourceEditorPanel + NetworkScan) and a CLOUD block
// (the same interactive provider list the Devices hub uses, via
// CloudWeatherWizardSection). Local station sources land in the wizard draft;
// the cloud list drives the live config's enable PUTs directly (the region
// keyless authority is still seeded at finalize for a user who changes nothing).
// Skipping is fine: sources can be added on the Sensors hub any time.

use leptos::prelude::*;

use crate::components::setup::shell::{next_step_href, prev_step_href, SetupFooter};
use crate::components::sources_form::{kind_caps, kind_pretty, SourceEditorPanel};
use crate::components::ui::{Button, HelpHint};

/// The cloud forecast kinds. A draft source whose kind is NOT one of these is a
/// LOCAL station / gateway / passthrough, which flips the cloud section's copy
/// to "complement/backup your station" framing. Kept in lockstep with the
/// server's `SourceKind::is_forecast` cloud set (cloud_catalog::cloud_kinds).
const CLOUD_KINDS: &[&str] = &[
    "open_meteo",
    "nws",
    "openweather",
    "weatherkit",
    "pirate_weather",
    "met_norway",
];

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
pub fn SourcesStep() -> impl IntoView {
    let draft = RwSignal::new(serde_json::Value::Null);
    let adding = RwSignal::new(false);

    #[cfg(feature = "hydrate")]
    Effect::new(move |_| {
        leptos::task::spawn_local(async move {
            if let Some(d) = fetch_draft().await {
                draft.set(d);
            }
        });
    });
    #[cfg(not(feature = "hydrate"))]
    let _ = draft;

    let persist = Callback::new(move |entry: serde_json::Value| {
        let id = entry
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        draft.update(|d| {
            if let Some(arr) = d
                .get_mut("config")
                .and_then(|c| c.get_mut("sources"))
                .and_then(|v| v.as_array_mut())
            {
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
        let candidate = draft.get_untracked();
        #[cfg(feature = "hydrate")]
        leptos::task::spawn_local(async move {
            let _ = save_draft(candidate).await;
        });
        #[cfg(not(feature = "hydrate"))]
        let _ = candidate;
        adding.set(false);
    });

    // The draft's local-station sources (any source whose kind is NOT a cloud
    // forecast kind). Drives both the "your hardware" list and the cloud
    // section's complement/backup framing.
    let local_sources = move || {
        draft
            .get()
            .get("config")
            .and_then(|c| c.get("sources"))
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter(|s| {
                        let kind = s.get("kind").and_then(|v| v.as_str()).unwrap_or("");
                        !CLOUD_KINDS.contains(&kind)
                    })
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    };

    // True once the user has a local station in the draft, so the cloud section
    // frames cloud as a complement/backup rather than the primary path.
    let has_hardware = Signal::derive(move || !local_sources().is_empty());

    let added_view = move || {
        let sources = local_sources();
        if sources.is_empty() {
            return view! {
                <p class="setup-step__body" style="margin:0">"No weather station added yet."</p>
            }
            .into_any();
        }
        sources
            .into_iter()
            .map(|s| {
                let id = s
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let kind = s.get("kind").and_then(|v| v.as_str()).unwrap_or("");
                let pretty = kind_pretty(kind).to_string();
                let caps = kind_caps(kind).to_string();
                view! {
                    <li class="cond-row">
                        <span class="cond-row__dot"></span>
                        <div class="cond-row__text">
                            <span class="cond-row__name">{id}</span>
                            <span class="cond-row__sum">{pretty}</span>
                            <span class="source-caps__badge">{caps}</span>
                        </div>
                    </li>
                }
            })
            .collect_view()
            .into_any()
    };

    // LOCAL block: scan the network or add a station; these land in the wizard
    // draft and go live at finish. The station the chain ranks first is added
    // from RIGHT HERE, co-located with the cloud toggles, under the teal/green
    // station identity stripe (entity-stripe--sensor).
    let local_block = move || {
        view! {
            <section class="setup-sources-block entity-stripe entity-stripe--sensor">
                <div class="setup-sources-block__head">
                    <h3 class="setup-sources-block__title">"Your weather stations (strongest signal)"</h3>
                    <p class="setup-sources-block__lede">
                        "Own a Tempest, Ecowitt, or Davis station on your network? Add it here and it "
                        "takes priority over cloud for the readings it carries. A mixed device (an "
                        "Ecowitt gateway that also reads soil) is added here as one source; its soil "
                        "channels become assignable in the Zones step. Most people skip this and start "
                        "on the cloud feeds."
                    </p>
                </div>

                <crate::components::setup::discover::NetworkScan mode="sources" draft=draft/>

                <ul class="cond-list">{added_view}</ul>

                {move || if adding.get() {
                    view! {
                        <SourceEditorPanel
                            on_commit=persist
                            on_cancel=Callback::new(move |()| adding.set(false))
                        />
                    }.into_any()
                } else {
                    view! {
                        <Button variant="primary" on_click=Callback::new(move |_| adding.set(true))>"+ Add a weather station"</Button>
                    }.into_any()
                }}
            </section>
        }
    };

    // CLOUD block: the same interactive provider list the Devices hub uses,
    // framed off for_hardware (a hardware user sees it as gap-fillers + downtime
    // coverage, a no-hardware user as the primary path). The cloud panel carries
    // its own LOCAL/CLOUD identity bands; this wrapper gives the section the blue
    // source stripe so the two sub-blocks read as one ranked picture.
    let cloud_block = move || {
        view! {
            <section class="setup-sources-block entity-stripe entity-stripe--source">
                <crate::components::settings::CloudWeatherWizardSection for_hardware=has_hardware/>
            </section>
        }
    };

    view! {
        <div class="setup-step">
            <h2 class="setup-step__title">"Where should LocalSky get its weather?"<HelpHint topic="forecast"/></h2>
            <p class="setup-step__sub">
                "LocalSky has two first-class sources of weather, and you can run either or both. A "
                "cloud weather service covers your location with no hardware (free options need no "
                "key). A weather station on your network is the strongest signal for the readings it "
                "carries. Below you have full control of both: turn on cloud providers, and add your "
                "station if you have one."
            </p>

            <p class="setup-step__body">
                "How they fit together: a live LAN station (Tempest, Ecowitt) always outranks cloud "
                "for the readings it covers; cloud fills the readings it does not, and backs it up if "
                "it goes quiet. So a no-hardware setup runs entirely on cloud, and a hardware setup "
                "uses cloud as a complement and a safety net. Cloud current conditions are model "
                "based, not a sensor in your yard, so they are not perfectly hyperlocal, but they are "
                "a legitimate source. Each option below states exactly what it provides."
            </p>

            // Local-first IA (spec 4a/4b). One unified Sources view, two
            // co-located identity sub-blocks under the one heading above: a LOCAL
            // block wearing the teal/green station identity (entity-stripe--sensor)
            // that holds the "add a weather station" affordance, and a CLOUD block
            // wearing the blue source identity (entity-stripe--source) that holds
            // the same interactive provider list the Devices hub uses. They are
            // ordered by has_hardware: a station owner sees LOCAL first (it wins
            // for the readings it carries, cloud fills the gaps), a no-hardware
            // user stays cloud-first (no empty local block as the hero). The cloud
            // panel reframes its own copy off for_hardware; finalize seeds the
            // region keyless authority for a user who changes nothing.
            {move || if has_hardware.get() {
                view! { {local_block()} {cloud_block()} }.into_any()
            } else {
                view! { {cloud_block()} {local_block()} }.into_any()
            }}

            <p class="sensors-section__hint" style="margin-top: var(--space-3)">
                "Everything you set here goes live when you finish setup: the cloud providers you "
                "turned on, and any station you added. To confirm a source is actually ingesting "
                "(and see its live readings), open the "<a href="/sensors">"Sensors hub"</a>
                " afterward, that is also where you can add or edit sensors any time, no wizard "
                "required. Change nothing and LocalSky runs on your region's free cloud feeds (and "
                "listens for a Tempest on your network if you have one)."
            </p>

            <SetupFooter
                prev=prev_step_href("sources")
                next=next_step_href("sources")
            />
        </div>
    }
}
