// HealthBanner. Hydrate-only poll of /api/v1/health every 60s. It fires ONLY on
// a TRUE coverage gap: a headline READING with no live owner AND no enabled
// backup left to take it over. It names the FIELD and the fix ("No rain source
// for your area; add NWS or Open-Meteo"), never an internal source id, and never
// calls a healthy backup offline. Dismissing snoozes that exact gap set for the
// session (a new gap re-raises the banner). SSR and the first hydrate frame
// render nothing, so the DOM matches.
//
// It reads the SAME honest source-status taxonomy the STATUS agent exposes:
// `offline` is the ONLY genuine-fault state (watching / standby / falling_through
// are the chain working, never a fault), and the `conditions[]` provenance is
// the live per-field ownership. A field is a gap only when nobody owns it AND no
// enabled source is in a calm (non-offline) state to feed it. The fall-through
// chain handing a field from one source to the next is never a gap.
//
// SOIL anomalies are deliberately NOT surfaced here: this banner is global
// (rendered on every page, including the weather home), and soil-probe
// offline/suspect conditions belong on irrigation + zones, where the
// `AnomalyBanner` is the single owner of them. Listing soil faults here too
// duplicated the AnomalyBanner and pushed soil warnings onto the weather tab.

use leptos::prelude::*;

use crate::components::ui::Icon;

/// The headline reading the banner guards, by its /api/health `conditions[]`
/// display name, paired with the homeowner fix line when it has no owner and no
/// backup. Rain is the load-bearing irrigation input, so it leads; the fix names
/// the keyless services that cover it with no hardware. (Other fields fall back
/// to the soft warming-up captions on their own pages; only a rain gap is loud
/// enough to raise the global banner.)
#[cfg(feature = "hydrate")]
const RAIN_FIELD: &str = "Rain";

#[component]
pub fn HealthBanner() -> impl IntoView {
    // The coverage-gap message driving the banner; empty = no gap, healthy.
    let gaps: RwSignal<Vec<String>> = RwSignal::new(Vec::new());
    // Gap set the user dismissed; compared as a joined key.
    let snoozed: RwSignal<String> = RwSignal::new(String::new());

    #[cfg(feature = "hydrate")]
    Effect::new(move |_| {
        leptos::task::spawn_local(async move {
            loop {
                let next = async {
                    let resp = gloo_net::http::Request::get("/api/v1/health")
                        .send()
                        .await
                        .ok()?;
                    let v = resp.json::<serde_json::Value>().await.ok()?;
                    Some(coverage_gaps(&v))
                }
                .await;
                if let Some(list) = next {
                    gaps.set(list);
                }
                gloo_timers::future::TimeoutFuture::new(60_000).await;
            }
        });
    });

    let dismiss = move |_| {
        snoozed.set(gaps.get_untracked().join(","));
    };

    move || {
        let list = gaps.get();
        if list.is_empty() || snoozed.get() == list.join(",") {
            return ().into_any();
        }
        // Each entry is already a complete, homeowner-facing gap sentence (the
        // field + its fix). One reads as itself; several join with a separator.
        let label = if list.len() == 1 {
            list[0].clone()
        } else {
            list.join(" ")
        };
        view! {
            <div class="health-banner" role="status">
                <span class="health-banner__icon" aria-hidden="true">
                    <Icon name="alert-triangle" size=16/>
                </span>
                <span class="health-banner__text">
                    {label}
                    " The engine keeps deciding from the freshest data it has."
                </span>
                <a class="health-banner__link" href="/settings/devices">"Add a source"</a>
                <button
                    type="button"
                    class="health-banner__dismiss"
                    aria-label="Dismiss health warning"
                    on:click=dismiss
                >
                    <Icon name="x" size=14/>
                </button>
            </div>
        }
        .into_any()
    }
}

/// Derive the TRUE coverage gaps from a parsed /api/health payload. A gap is a
/// headline reading with ZERO live owner AND ZERO enabled backup. We read the
/// honest taxonomy, not raw staleness:
///   * live owner: the field appears in `conditions[]` (the live per-field
///     provenance), so SOME source is driving it right now.
///   * enabled backup: any enabled source in a CALM state (active / watching /
///     standby / falling_through, i.e. NOT `offline`) could step in. A source
///     mid fall-through is a working backup, never a gap.
/// Only when BOTH are absent for a field is it a real gap, and only then do we
/// name the field and its fix. Returns homeowner-facing sentences (no source
/// ids). An empty Vec means every guarded reading is covered.
#[cfg(feature = "hydrate")]
fn coverage_gaps(v: &serde_json::Value) -> Vec<String> {
    // No detail to judge on: an anonymous caller on an auth-required instance
    // gets liveness only (sources/conditions trimmed off), and a wizard-state
    // instance has no sources yet. With no source detail we cannot tell a real
    // gap from a hidden one, so we stay silent rather than raise a phantom gap.
    let has_source_detail = v
        .get("sources")
        .and_then(|s| s.as_array())
        .map(|a| !a.is_empty())
        .unwrap_or(false);
    if !has_source_detail {
        return Vec::new();
    }
    // A field has a live owner iff it appears in the conditions provenance.
    let owned = |field: &str| -> bool {
        v.get("conditions")
            .and_then(|c| c.as_array())
            .map(|arr| {
                arr.iter()
                    .any(|c| c.get("field").and_then(|f| f.as_str()) == Some(field))
            })
            .unwrap_or(false)
    };
    // An enabled source in any CALM (non-offline) state can take a field over,
    // so its mere presence means there IS a backup. `offline` is the ONLY fault
    // state in the taxonomy, so a non-offline enabled source is a live or
    // standing-ready feeder. (We cannot read per-source field capability off the
    // health payload, so a single calm enabled source covering the merge is the
    // honest "a backup exists" signal: the engine would have used it.)
    let has_calm_enabled_source = v
        .get("sources")
        .and_then(|s| s.as_array())
        .map(|arr| {
            arr.iter().any(|s| {
                s.get("enabled").and_then(|e| e.as_bool()) == Some(true)
                    && s.get("status").and_then(|st| st.as_str()) != Some("offline")
            })
        })
        .unwrap_or(false);

    let mut out = Vec::new();
    // Rain: the load-bearing irrigation reading. A gap here means no source can
    // tell the engine whether it has rained, so it cannot honor a rain skip.
    if !owned(RAIN_FIELD) && !has_calm_enabled_source {
        out.push(
            "No rain source for your area yet; add NWS or Open-Meteo (free, no \
             key) under Devices so skip-on-rain works."
                .to_string(),
        );
    }
    out
}
