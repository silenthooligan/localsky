// Results-based tuning surfaces: the per-zone Tuning panel on the zone
// detail (plain lines + at most one recommendation card with an Apply
// action) and the small irrigation-page strip (recommendation count +
// the install-wide forecast-skip scorecard line). Both fetch
// GET /api/v1/irrigation/tuning on demand, exactly like the zone
// detail's history Effect (hydrate-gated gloo_net into an RwSignal;
// try_* accessors in detached continuations per the disposed-signal
// discipline).

use leptos::prelude::*;

use crate::components::ui::{Button, SkeletonRows};
use crate::history::types::{TuningRecommendation, TuningReport};
// use_toast is referenced fully qualified inside the hydrate-only apply
// continuation, so no import goes unused on the ssr build.

#[cfg(feature = "hydrate")]
async fn fetch_report() -> Result<TuningReport, String> {
    use gloo_net::http::Request;
    let resp = Request::get("/api/v1/irrigation/tuning")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        let body = resp.text().await.unwrap_or_default();
        return Err(crate::components::settings_ui::load_error_message(
            resp.status(),
            &body,
        ));
    }
    resp.json::<TuningReport>().await.map_err(|e| e.to_string())
}

/// The Tuning panel on the zone detail. Fetch keyed on the slug plus a
/// refetch counter bumped after an Apply, so the panel re-reads the
/// regenerated report (the applied zone drops to its ok state).
#[component]
pub fn ZoneTuningPanel(slug: Signal<String>) -> impl IntoView {
    let report: RwSignal<Option<Result<TuningReport, String>>> = RwSignal::new(None);
    let refetch: RwSignal<u32> = RwSignal::new(0);
    let applying: RwSignal<bool> = RwSignal::new(false);

    #[cfg(feature = "hydrate")]
    {
        Effect::new(move |_| {
            let _ = slug.get();
            let _ = refetch.get();
            report.set(None);
            leptos::task::spawn_local(async move {
                let result = fetch_report().await;
                // Detached continuation: the route may be gone by now.
                let _ = report.try_set(Some(result));
            });
        });
    }
    view! {
        <section class="zone-detail__panel zone-tuning">
            <h2 class="zone-detail__panel-title">"Tuning"</h2>
            {move || match report.get() {
                None => view! { <SkeletonRows count=3/> }.into_any(),
                Some(Err(e)) => view! {
                    <p class="zone-tuning__line zone-tuning__line--muted">
                        {format!("Tuning report unavailable: {e}")}
                    </p>
                }
                .into_any(),
                Some(Ok(rep)) => {
                    let want = slug.get().replace('-', "_");
                    let zone = rep
                        .zones
                        .iter()
                        .find(|z| z.slug.replace('-', "_") == want)
                        .cloned();
                    match zone {
                        None => view! {
                            <p class="zone-tuning__line zone-tuning__line--muted">
                                "No tuning data for this zone yet."
                            </p>
                        }
                        .into_any(),
                        Some(zt) => {
                            let lines = zt.lines.clone();
                            let window_days = rep.window_days;
                            let card = zt.recommendation.clone().map(|rec| {
                                let zone_slug = zt.slug.clone();
                                view! { <TuningRecommendationCard rec zone_slug window_days applying refetch/> }
                            });
                            view! {
                                {lines
                                    .into_iter()
                                    .map(|l| view! { <p class="zone-tuning__line">{l}</p> })
                                    .collect_view()}
                                {card}
                            }
                            .into_any()
                        }
                    }
                }
            }}
        </section>
    }
}

/// One recommendation: plain headline, expandable evidence, Apply.
/// `window_days` is the window the report was fetched at; the apply body
/// echoes it so the server re-derives at the same window.
#[component]
fn TuningRecommendationCard(
    rec: TuningRecommendation,
    zone_slug: String,
    window_days: u32,
    applying: RwSignal<bool>,
    refetch: RwSignal<u32>,
) -> impl IntoView {
    let headline = rec.headline.clone();
    let evidence = rec.evidence.clone();
    let confidence = rec.confidence.clone();
    #[cfg(not(feature = "hydrate"))]
    let _ = (&rec, &zone_slug, window_days, applying, refetch);
    let on_apply = move |_: leptos::ev::MouseEvent| {
        #[cfg(feature = "hydrate")]
        {
            use gloo_net::http::Request;
            if applying.get_untracked() {
                return;
            }
            applying.set(true);
            let body = serde_json::json!({
                "zone_slug": zone_slug.clone(),
                "recommendation_id": rec.id.clone(),
                "field": rec.field.clone(),
                "value": rec.suggested_value.clone(),
                "window_days": window_days,
            });
            leptos::task::spawn_local(async move {
                let outcome: Result<serde_json::Value, (u16, String)> = async {
                    let resp = Request::post("/api/config/zones/apply")
                        .json(&body)
                        .map_err(|e| (0u16, e.to_string()))?
                        .send()
                        .await
                        .map_err(|e| (0u16, e.to_string()))?;
                    let status = resp.status();
                    if !resp.ok() {
                        let text = resp.text().await.unwrap_or_default();
                        return Err((status, text));
                    }
                    resp.json::<serde_json::Value>()
                        .await
                        .map_err(|e| (status, e.to_string()))
                }
                .await;
                // Detached continuation: only touch signals via try_*.
                let _ = applying.try_set(false);
                match outcome {
                    Ok(resp) => {
                        let restart = resp
                            .get("restart_required")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        if restart {
                            crate::components::ui::use_toast().success(
                                "Applied. This change takes effect after the next restart.",
                            );
                        } else {
                            crate::components::ui::use_toast().success(
                                "Applied. The engine uses the new value from \
                                                 its next evaluation.",
                            );
                        }
                    }
                    Err((409, text)) => {
                        let detail = crate::components::settings_ui::save_error_message(409, &text);
                        crate::components::ui::use_toast().warn(format!("Not applied: {detail}"));
                    }
                    Err((status, text)) => {
                        crate::components::ui::use_toast().error(
                            crate::components::settings_ui::save_error_message(status, &text),
                        );
                    }
                }
                // Re-read the regenerated report either way (success clears
                // the card; a stale 409 shows the current state).
                let _ = refetch.try_update(|n| *n += 1);
            });
        }
    };
    view! {
        <div class="zone-tuning__card">
            <p class="zone-tuning__headline">{headline}</p>
            <details class="zone-tuning__evidence">
                <summary>{format!("Why this suggestion ({confidence} confidence)")}</summary>
                <ul class="zone-tuning__evidence-list">
                    {evidence
                        .into_iter()
                        .map(|e| view! { <li>{e}</li> })
                        .collect_view()}
                </ul>
            </details>
            <div class="zone-tuning__apply">
                <Button
                    variant="primary"
                    loading=Signal::derive(move || applying.get())
                    on_click=Callback::new(on_apply)
                >"Apply"</Button>
            </div>
        </div>
    }
}

/// Irrigation-page strip: how many zones carry a recommendation (linked
/// to the zones view) plus the install-wide forecast-skip scorecard
/// line. Hidden entirely until the report loads AND has something worth
/// a row (a recommendation or a scored scorecard), so fresh installs
/// see nothing extra.
#[component]
pub fn TuningStrip() -> impl IntoView {
    let report: RwSignal<Option<TuningReport>> = RwSignal::new(None);
    #[cfg(feature = "hydrate")]
    {
        Effect::new(move |_| {
            leptos::task::spawn_local(async move {
                if let Ok(rep) = fetch_report().await {
                    let _ = report.try_set(Some(rep));
                }
            });
        });
    }
    move || {
        report.get().and_then(|rep| {
            let count = rep
                .zones
                .iter()
                .filter(|z| z.recommendation.is_some())
                .count();
            let scored = rep.scorecard.scored_days.is_some();
            let reactive = rep.scorecard.reactive_days.is_some();
            (count > 0 || scored || reactive).then(|| {
                let count_line = (count > 0).then(|| {
                    let label = if count == 1 {
                        "1 zone has a tuning suggestion".to_string()
                    } else {
                        format!("{count} zones have tuning suggestions")
                    };
                    view! { <a class="tuning-strip__link" href="/zones">{label}</a> }
                });
                let scorecard_line = scored.then(|| {
                    view! { <span class="tuning-strip__scorecard">{rep.scorecard.line.clone()}</span> }
                });
                // Reactive rain skips carry their own counted line (no
                // confirmation math; they confirm themselves).
                let reactive_line = reactive.then(|| {
                    view! { <span class="tuning-strip__scorecard">{rep.scorecard.reactive_line.clone()}</span> }
                });
                view! {
                    <div class="tuning-strip">
                        {count_line}
                        {scorecard_line}
                        {reactive_line}
                    </div>
                }
            })
        })
    }
}
