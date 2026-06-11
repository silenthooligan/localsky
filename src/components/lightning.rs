// Lightning panel: a small radar plot showing recent strikes (last hour),
// plus stats, last strike time/distance and 1h/1m counts. Strikes plot
// at distance-scaled radii on concentric range rings (5/10/20/30 mi).

use crate::tempest::state::Snapshot;
use leptos::prelude::*;

const MAX_RADIUS_MI: f64 = 30.0;

#[component]
pub fn LightningPanel(snap: ReadSignal<Snapshot>) -> impl IntoView {
    let strikes = Memo::new(move |_| snap.get().lightning_recent.clone());
    let last_age = move || {
        let s = snap.get();
        match s.last_strike_epoch {
            Some(t) if s.last_packet_epoch > 0 => {
                let secs = (s.last_packet_epoch - t).max(0);
                if secs < 60 {
                    format!("{}s ago", secs)
                } else if secs < 3600 {
                    format!("{}m ago", secs / 60)
                } else {
                    format!("{:.1}h ago", secs as f64 / 3600.0)
                }
            }
            _ => "-".to_string(),
        }
    };

    view! {
        <section class="panel lightning">
            <h2 class="panel-title">"Lightning"</h2>
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
                    {move || strikes.get().into_iter().map(|s| {
                        let mi = s.distance_km * 0.621371;
                        let r = (mi / MAX_RADIUS_MI * 48.0).min(48.0);
                        // We don't know the bearing, Tempest only reports distance.
                        // Plot strikes at deterministic angles derived from time so
                        // re-renders don't reshuffle the dots.
                        let angle = ((s.time_epoch as f64) * 137.508).to_radians();
                        let x = r * angle.cos();
                        let y = r * angle.sin();
                        view! {
                            <circle cx=x cy=y r="2.2" class="strike-dot">
                                <title>{format!("{:.1} mi", mi)}</title>
                            </circle>
                        }
                    }).collect_view()}
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
                                Some(d) => format!("{:.1} mi", d),
                                None => "-".to_string(),
                            }}
                        </span>
                    </div>
                    <div class="kv">
                        <span class="k">"avg distance"</span>
                        <span class="v">{move || format!("{:.1} mi", snap.get().lightning_avg_dist_mi)}</span>
                    </div>
                </div>
            </div>
        </section>
    }
}
