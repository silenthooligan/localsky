// Results-based tuning surfaces: the per-zone Tuning panel on the zone
// detail (a lead cadence line, secondary notes behind one disclosure,
// and at most one recommendation card with an Apply action) and the
// irrigation-page strip (attention-dotted recommendation count + the
// install-wide forecast-skip scorecard lines). The panel fetches
// GET /api/v1/irrigation/tuning on demand, exactly like the zone
// detail's history Effect (hydrate-gated gloo_net into an RwSignal;
// try_* accessors in detached continuations per the disposed-signal
// discipline); page-level consumers (ZonesPage, IrrigationPage) share
// ONE fetch via use_tuning_report instead of per-surface fetches.

use leptos::prelude::*;

use crate::components::ui::{Button, ConfirmSheet, SkeletonRows};
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

/// Page-scoped invalidation counter for the shared tuning report.
/// ZonesPage creates one and provide_context's it BEFORE calling
/// use_tuning_report; the Apply continuation bumps it, so the Suggestions
/// KPI, the zone-card pills, and the recommendation-aware auto-select all
/// reflect an Apply without a page reload. Optional everywhere it is
/// read: the standalone /zones/:slug route and the irrigation page
/// provide none and keep their single-fetch behavior.
#[derive(Clone, Copy)]
pub struct TuningEpoch(pub RwSignal<u32>);

/// Page-level tuning-report signal: ONE fetch per page, shared by every
/// consumer on it (the strip, the zone-card pills, the Suggestions KPI)
/// so no surface fetches per item. Re-fetches when the page's TuningEpoch
/// context (when provided) bumps. None until loaded; stays None on a
/// fetch error (every consumer renders its no-report state).
pub fn use_tuning_report() -> RwSignal<Option<TuningReport>> {
    let report: RwSignal<Option<TuningReport>> = RwSignal::new(None);
    #[cfg(feature = "hydrate")]
    {
        let epoch = use_context::<TuningEpoch>();
        Effect::new(move |_| {
            if let Some(e) = epoch {
                let _ = e.0.get();
            }
            leptos::task::spawn_local(async move {
                if let Ok(rep) = fetch_report().await {
                    // Detached continuation: the route may be gone by now.
                    let _ = report.try_set(Some(rep));
                }
            });
        });
    }
    report
}

/// Zones carrying a recommendation in this report, underscore-normalized
/// so they join directly against snapshot zone slugs.
pub fn recommended_slugs(rep: &TuningReport) -> std::collections::HashSet<String> {
    rep.zones
        .iter()
        .filter(|z| z.recommendation.is_some())
        .map(|z| z.slug.replace('-', "_"))
        .collect()
}

/// How many zones carry a recommendation in this report.
pub fn recommendation_count(rep: &TuningReport) -> usize {
    rep.zones
        .iter()
        .filter(|z| z.recommendation.is_some())
        .count()
}

/// Whether the irrigation strip has anything worth a row: a
/// recommendation, or a scored/reactive scorecard line.
pub fn strip_visible(rep: &TuningReport) -> bool {
    recommendation_count(rep) > 0
        || rep.scorecard.scored_days.is_some()
        || rep.scorecard.reactive_days.is_some()
}

/// The recommendation's current -> suggested summary for the card's mono
/// row: (row label, "from -> to").
pub(crate) fn delta_line(rec: &TuningRecommendation) -> (String, String) {
    let label = match rec.field.as_str() {
        "max_run_minutes" => "Run limit",
        "sessions_per_week" => "Sessions per week",
        "weekly_budget_in" => "Weekly target",
        "precip_rate_mm_hr" => "Precip rate",
        "root_depth_mm" => "Root depth",
        "mad_pct_override" => "MAD override",
        "soil_texture" => "Soil texture",
        other => other,
    }
    .to_string();
    let value = format!(
        "{} -> {}",
        fmt_rec_value(&rec.field, &rec.current_value),
        fmt_rec_value(&rec.field, &rec.suggested_value)
    );
    (label, value)
}

fn fmt_rec_value(field: &str, v: &serde_json::Value) -> String {
    let unit = match field {
        "max_run_minutes" => " min",
        "sessions_per_week" => "/wk",
        "weekly_budget_in" => " in",
        "precip_rate_mm_hr" => " mm/hr",
        "root_depth_mm" => " mm",
        _ => "",
    };
    match v {
        // Null means "the default": for the run limit that default is a
        // real number (60 min); everywhere else name it plainly.
        serde_json::Value::Null => match field {
            "max_run_minutes" => "60 min".to_string(),
            _ => "default".to_string(),
        },
        serde_json::Value::String(s) => s.replace('_', " "),
        n => format!("{n}{unit}"),
    }
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
                            // Density: the first line (the watering-cadence
                            // line when the zone has runs) leads; the
                            // probe-availability and insufficient-data lines
                            // collapse into ONE quiet disclosure.
                            let mut lines = zt.lines.clone().into_iter();
                            let lead = lines.next();
                            let notes: Vec<String> = lines.collect();
                            let window_days = rep.window_days;
                            let card = zt.recommendation.clone().map(|rec| {
                                let zone_slug = zt.slug.clone();
                                let zone_name = zt.display_name.clone();
                                view! { <TuningRecommendationCard rec zone_slug zone_name window_days applying refetch/> }
                            });
                            view! {
                                {lead.map(|l| view! {
                                    <p class="zone-tuning__line zone-tuning__line--lead">{l}</p>
                                })}
                                {(!notes.is_empty()).then(|| view! {
                                    <details class="zone-tuning__notes">
                                        <summary>{format!("Data notes ({})", notes.len())}</summary>
                                        {notes
                                            .into_iter()
                                            .map(|l| view! { <p>{l}</p> })
                                            .collect_view()}
                                    </details>
                                })}
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

/// One recommendation: attention-striped card with a SUGGESTION pill, a
/// status-chip confidence, the bumped headline, the mono current ->
/// suggested row, expandable evidence, and Apply. A max_run_minutes
/// suggestion above 60 gates the Apply behind the shared ConfirmSheet
/// (the same override-style confirm the zone editor uses). `window_days`
/// is the window the report was fetched at; the apply body echoes it so
/// the server re-derives at the same window.
#[component]
fn TuningRecommendationCard(
    rec: TuningRecommendation,
    zone_slug: String,
    /// Display name, for the confirmation copy (names the zone).
    zone_name: String,
    window_days: u32,
    applying: RwSignal<bool>,
    refetch: RwSignal<u32>,
) -> impl IntoView {
    // The page-level report invalidation (when a page provides one): an
    // Apply must also refresh the KPI tile, the zone-card pills, and the
    // auto-select, not just this panel's private report.
    let epoch = use_context::<TuningEpoch>();
    let headline = rec.headline.clone();
    let evidence = rec.evidence.clone();
    let confidence = rec.confidence.clone();
    let chip_class = match confidence.as_str() {
        "high" => "status-chip status-chip--online",
        "medium" => "status-chip status-chip--stale",
        _ => "status-chip status-chip--unknown",
    };
    let (delta_label, delta_value) = delta_line(&rec);
    // The override-style gate: a run limit raised past 60 confirms first.
    let confirm_open = RwSignal::new(false);
    let suggested_minutes: Option<u32> = (rec.field == "max_run_minutes")
        .then(|| {
            rec.suggested_value
                .as_u64()
                .and_then(|v| u32::try_from(v).ok())
        })
        .flatten();
    let needs_confirm = suggested_minutes.map(|m| m > 60).unwrap_or(false);
    let confirm_minutes = suggested_minutes.unwrap_or(60);

    // The POST itself, shared by the direct path and the sheet's Confirm.
    let rec_for_apply = rec.clone();
    let slug_for_apply = zone_slug.clone();
    let do_apply = Callback::new(move |()| {
        #[cfg(not(feature = "hydrate"))]
        let _ = (
            &rec_for_apply,
            &slug_for_apply,
            window_days,
            applying,
            refetch,
            epoch,
        );
        #[cfg(feature = "hydrate")]
        {
            use gloo_net::http::Request;
            if applying.get_untracked() {
                return;
            }
            applying.set(true);
            let body = serde_json::json!({
                "zone_slug": slug_for_apply.clone(),
                "recommendation_id": rec_for_apply.id.clone(),
                "field": rec_for_apply.field.clone(),
                "value": rec_for_apply.suggested_value.clone(),
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
                // the card; a stale 409 shows the current state), and bump
                // the page-level epoch so the KPI tile, the zone-card
                // pills, and the auto-select refresh in the same viewport.
                let _ = refetch.try_update(|n| *n += 1);
                if let Some(e) = epoch {
                    let _ = e.0.try_update(|n| *n += 1);
                }
            });
        }
    });
    let on_apply = move |_: leptos::ev::MouseEvent| {
        if applying.get_untracked() {
            return;
        }
        if needs_confirm {
            confirm_open.set(true);
            return;
        }
        do_apply.run(());
    };
    view! {
        <div class="zone-tuning__card is-control">
            <div class="zone-tuning__card-head">
                <span class="attention-pill">"Suggestion"</span>
                <span class=chip_class>{format!("{confidence} confidence")}</span>
            </div>
            <p class="zone-tuning__headline">{headline}</p>
            <dl class="zone-tuning__delta">
                <dt>{delta_label}</dt>
                <dd>{delta_value}</dd>
            </dl>
            <details class="zone-tuning__evidence">
                <summary>"Why this suggestion"</summary>
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
            <ConfirmSheet
                visible=confirm_open
                title="Raise run limit?"
                body=Signal::derive(move || {
                    format!(
                        "This allows {zone_name} to run up to {confirm_minutes} minutes in a \
                         single watering. Cycle and soak still splits long runs to limit \
                         runoff."
                    )
                })
                confirm_label=Signal::derive(move || format!("Allow {confirm_minutes} min"))
                on_confirm=do_apply
            />
        </div>
    }
}

/// Irrigation-page strip: the attention-dotted recommendation count
/// (linked to the zones view) plus the install-wide forecast-skip
/// scorecard lines. The report signal comes from the page
/// (use_tuning_report), so the desktop and mobile branches share one
/// fetch and the page can place the strip above the data columns when a
/// suggestion exists. Hidden entirely until the report loads AND has
/// something worth a row, so fresh installs see nothing extra.
#[component]
pub fn TuningStrip(report: RwSignal<Option<TuningReport>>) -> impl IntoView {
    move || {
        report.get().and_then(|rep| {
            strip_visible(&rep).then(|| {
                let count = recommendation_count(&rep);
                let scored = rep.scorecard.scored_days.is_some();
                let reactive = rep.scorecard.reactive_days.is_some();
                let count_line = (count > 0).then(|| {
                    let label = if count == 1 {
                        "1 zone has a tuning suggestion".to_string()
                    } else {
                        format!("{count} zones have tuning suggestions")
                    };
                    view! {
                        <span class="tuning-strip__count">
                            <a class="tuning-strip__link is-interactive" href="/zones">{label}</a>
                        </span>
                    }
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
                    <div class="tuning-strip is-static">
                        {count_line}
                        {scorecard_line}
                        {reactive_line}
                    </div>
                }
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::types::{TuningScorecard, ZoneTuning};
    use serde_json::json;

    fn rec(
        field: &str,
        current: serde_json::Value,
        suggested: serde_json::Value,
    ) -> TuningRecommendation {
        TuningRecommendation {
            id: "id".into(),
            field: field.into(),
            current_value: current,
            suggested_value: suggested,
            companion_fields: vec![],
            headline: "h".into(),
            evidence: vec![],
            confidence: "medium".into(),
        }
    }

    fn report(
        zones: Vec<(&str, bool)>,
        scored: Option<u32>,
        reactive: Option<u32>,
    ) -> TuningReport {
        TuningReport {
            generated_epoch: 0,
            window_days: 14,
            zones: zones
                .into_iter()
                .map(|(slug, has_rec)| ZoneTuning {
                    slug: slug.into(),
                    display_name: slug.into(),
                    status: "ok".into(),
                    lines: vec![],
                    recommendation: has_rec.then(|| rec("sessions_per_week", json!(2), json!(3))),
                })
                .collect(),
            scorecard: TuningScorecard {
                scored_days: scored,
                reactive_days: reactive,
                ..Default::default()
            },
        }
    }

    #[test]
    fn zone_card_join_normalizes_slugs_and_counts() {
        // Config keys may be dashed while snapshot slugs are underscored:
        // the join set is underscore-normalized so cards match directly.
        let rep = report(
            vec![
                ("back-yard", true),
                ("front_yard", true),
                ("side_yard", false),
            ],
            None,
            None,
        );
        let set = recommended_slugs(&rep);
        assert!(set.contains("back_yard"));
        assert!(set.contains("front_yard"));
        assert!(!set.contains("side_yard"));
        assert_eq!(recommendation_count(&rep), 2);
    }

    #[test]
    fn strip_visibility_needs_a_recommendation_or_a_scorecard() {
        assert!(!strip_visible(&report(vec![("a", false)], None, None)));
        assert!(strip_visible(&report(vec![("a", true)], None, None)));
        assert!(strip_visible(&report(vec![("a", false)], Some(4), None)));
        assert!(strip_visible(&report(vec![("a", false)], None, Some(2))));
    }

    #[test]
    fn delta_line_renders_the_run_limit_defaults_and_units() {
        // Unset current renders the real 60 minute default, not "null".
        let r = rec("max_run_minutes", serde_json::Value::Null, json!(116));
        assert_eq!(
            delta_line(&r),
            ("Run limit".to_string(), "60 min -> 116 min".to_string())
        );
        let r = rec("sessions_per_week", json!(2), json!(3));
        assert_eq!(
            delta_line(&r),
            ("Sessions per week".to_string(), "2/wk -> 3/wk".to_string())
        );
        let r = rec("soil_texture", json!("sandy_loam"), json!("loam"));
        assert_eq!(
            delta_line(&r),
            ("Soil texture".to_string(), "sandy loam -> loam".to_string())
        );
    }
}
