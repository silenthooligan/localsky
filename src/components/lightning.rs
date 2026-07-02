// Lightning panel: a small radar plot showing recent strikes (last hour),
// plus stats, last strike time/distance and 1h/1m counts. Strikes plot
// at distance-scaled radii on concentric range rings (5/10/20/30 mi).

use crate::components::units_fmt::{fmt_distance_mi, use_unit_prefs};
use crate::tempest::packets::STRIKE_SOURCE_BLITZORTUNG;
use crate::tempest::state::Snapshot;
use leptos::prelude::*;

const MAX_RADIUS_MI: f64 = 30.0;

/// True for a cloud-only (Open-Meteo) deployment. Keys on the canonical
/// `has_live_station` signal (true for any live station: Tempest / Ecowitt /
/// Davis / MQTT / ...), NOT the old Tempest-only serial + battery heuristic that
/// misread a live non-Tempest station as cloud-only. Lightning detection is
/// station / community only (Open-Meteo carries no strikes), so a true
/// cloud-only deployment with nothing in the strike buffer gets a deliberate
/// empty state instead of a permanently-zeroed radar.
fn is_cloud_only(s: &Snapshot) -> bool {
    !s.has_live_station
}

#[component]
pub fn LightningPanel(snap: ReadSignal<Snapshot>) -> impl IntoView {
    let prefs = use_unit_prefs();
    let strikes = Memo::new(move |_| snap.get().lightning_recent.clone());
    // Show the live radar when a station is present OR there is any strike data
    // to plot (the opt-in Blitzortung community feed populates the buffer even
    // on a cloud-only deployment). Only an unsourced, empty-buffer cloud-only
    // deployment falls to the explainer below.
    let has_live_lightning = Memo::new(move |_| {
        let s = snap.get();
        !is_cloud_only(&s) || !s.lightning_recent.is_empty()
    });
    // This panel renders the same lightning_recent buffer the radar
    // layer does, so when the opt-in Blitzortung source contributes,
    // their CC BY-SA terms require the source be identified here too.
    // The credit renders only while community strikes are buffered.
    let has_community = Memo::new(move |_| {
        snap.get()
            .lightning_recent
            .iter()
            .any(|s| s.source == STRIKE_SOURCE_BLITZORTUNG)
    });
    let last_age = move || {
        let s = snap.get();
        match s.last_strike_epoch {
            Some(t) => {
                // Age the strike against the station's own clock when a live
                // station is reporting (last_packet_epoch advances with packets);
                // otherwise (a community-feed-only / cloud-only deployment never
                // stamps last_packet_epoch) age it against wall-clock now, so a
                // real Blitzortung strike reads "3m ago" instead of "-".
                let reference = if s.last_packet_epoch > 0 {
                    s.last_packet_epoch
                } else {
                    chrono::Utc::now().timestamp()
                };
                let secs = (reference - t).max(0);
                if secs < 60 {
                    format!("{}s ago", secs)
                } else if secs < 3600 {
                    format!("{}m ago", secs / 60)
                } else {
                    format!("{:.1}h ago", secs as f64 / 3600.0)
                }
            }
            None => "-".to_string(),
        }
    };

    view! {
        <section class="panel lightning">
            <h2 class="panel-title">"Lightning"</h2>
            <Show
                when=move || has_live_lightning.get()
                fallback=|| view! {
                    <div class="lightning-row lightning-empty">
                        <p class="panel-empty" style="color:var(--text-dim);font-size:0.85rem;line-height:1.4;margin:auto;text-align:center;">
                            "Add a weather station for live lightning detection."
                            <br/>
                            <a href="/settings/data-sources" style="color:var(--accent);text-decoration:none;">
                                "Connect a station →"
                            </a>
                        </p>
                    </div>
                }
            >
            <div class="lightning-row">
                <svg viewBox="-50 -50 100 100" class="strike-radar">
                    <defs>
                        <radialGradient id="radarGlow">
                            <stop offset="0%" stop-color="rgba(255,235,150,0.18)"/>
                            <stop offset="80%" stop-color="rgba(255,235,150,0)"/>
                        </radialGradient>
                    </defs>
                    <circle cx="0" cy="0" r="48" fill="url(#radarGlow)"/>
                    <circle cx="0" cy="0" r="48" class="ring outer"/>
                    <circle cx="0" cy="0" r="32" class="ring"/>
                    <circle cx="0" cy="0" r="16" class="ring"/>
                    <circle cx="0" cy="0" r="2" class="ring center"/>
                    <line x1="-48" y1="0" x2="48" y2="0" class="ring axis"/>
                    <line x1="0" y1="-48" x2="0" y2="48" class="ring axis"/>
                    {move || {
                        let p = prefs.get();
                        strikes.get().into_iter().map(move |s| {
                        let mi = s.distance_km * 0.621371;
                        // Ring geometry stays in miles (MAX_RADIUS_MI rings).
                        let r = (mi / MAX_RADIUS_MI * 48.0).min(48.0);
                        // We don't know the bearing, Tempest only reports distance.
                        // Plot strikes at deterministic angles derived from time so
                        // re-renders don't reshuffle the dots.
                        let angle = ((s.time_epoch as f64) * 137.508).to_radians();
                        let x = r * angle.cos();
                        let y = r * angle.sin();
                        view! {
                            <circle cx=x cy=y r="2.2" class="strike-dot">
                                <title>{fmt_distance_mi(mi, p)}</title>
                            </circle>
                        }
                    }).collect_view()
                    }}
                </svg>
                <div class="lightning-stats">
                    <div class="kv">
                        <span class="k">"strikes (1h)"</span>
                        <span class="v big">{move || snap.get().lightning_strikes_last_hour}</span>
                    </div>
                    <div class="kv">
                        <span class="k">"last strike"</span>
                        <span class="v">{last_age}</span>
                    </div>
                    <div class="kv">
                        <span class="k">"last distance"</span>
                        <span class="v">
                            {move || match snap.get().last_strike_distance_mi {
                                Some(d) => fmt_distance_mi(d, prefs.get()),
                                None => "-".to_string(),
                            }}
                        </span>
                    </div>
                    <div class="kv">
                        <span class="k">"avg distance"</span>
                        <span class="v">{move || fmt_distance_mi(snap.get().lightning_avg_dist_mi, prefs.get())}</span>
                    </div>
                </div>
            </div>
            <Show when=move || has_community.get()>
                <p class="lightning-attribution">
                    "Includes lightning data from "
                    <a href="https://www.blitzortung.org/" target="_blank" rel="noopener">
                        "Blitzortung.org"
                    </a>
                    " contributors, CC BY-SA 4.0"
                </p>
            </Show>
            </Show>
        </section>
    }
}
