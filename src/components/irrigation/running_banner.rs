// Persistent "now running" banner that surfaces any active zone right at the
// top of the page, with a one-tap stop control. Closes the loop on the user's
// "I want to see what's actually happening, not just rely on automations" ask.
//
// Position rules:
// - On mobile, sticks to the top below the header so it's reachable without
//   scrolling. Bottom-tab nav is at the bottom; this is at the top.
// - On desktop, renders inline above the bento. Hidden when no zones run.
//
// Data: reads the same IrrigationSnapshot signal everything else uses.
// Deduplicates: if multiple zones run simultaneously (rare but possible
// during manual overlap), shows the first running zone with a "+N more"
// hint. The stop button always invokes stop_all in that case to be safe.

use crate::ha::snapshot::IrrigationSnapshot;
use leptos::prelude::*;
use serde_json::json;

#[component]
pub fn RunningBanner(snap: ReadSignal<IrrigationSnapshot>) -> impl IntoView {
    // Created at component scope (context lookup), shared by every
    // render of the inner closure.
    let stop_done = super::controls::toast_on_err("Stop failed; zone may still be running");
    move || {
        let s = snap.get();
        let running: Vec<_> = s.zones.iter().filter(|z| z.running).cloned().collect();
        let count = running.len();

        if count == 0 {
            return ().into_any();
        }

        // Take the first-running zone for the headline; show +N more if more.
        let first = running[0].clone();
        let first_name = first.name.clone();
        let first_slug = first.slug.clone();
        let first_planned = first.planned_run_seconds;

        let extra_count = if count > 1 { count - 1 } else { 0 };

        let on_stop = move |_| {
            // Single running zone -> stop just that one. Multiple -> stop_all
            // to handle the overlap case without a per-zone dance.
            if count > 1 {
                super::controls::post_action_then(json!({"kind": "stop_all"}), stop_done);
            } else {
                let slug = first_slug.clone();
                super::controls::post_action_then(json!({"kind": "stop", "zone": slug}), stop_done);
            }
        };

        let planned_label = if first_planned > 0 {
            format!("{} min planned", (first_planned + 30) / 60)
        } else {
            "running".to_string()
        };

        view! {
            <div class="running-banner" role="status" aria-live="polite">
                <div class="running-banner-pulse" aria-hidden="true"></div>
                <div class="running-banner-text">
                    <div class="running-banner-zone">{first_name}</div>
                    <div class="running-banner-meta">
                        {planned_label}
                        {move || if extra_count > 0 {
                            format!(" · +{extra_count} more")
                        } else {
                            String::new()
                        }}
                    </div>
                </div>
                <button class="running-banner-stop btn-clay btn-clay-hot" on:click=on_stop>
                    {if count > 1 { "STOP ALL" } else { "STOP" }}
                </button>
            </div>
        }
        .into_any()
    }
}
