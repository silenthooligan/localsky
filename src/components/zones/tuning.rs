// Results-based tuning surfaces: the per-zone Tuning panel on the zone
// detail (a lead cadence line, secondary notes behind one disclosure,
// and at most one recommendation card with an Apply action) and the
// irrigation-page strip (attention-dotted recommendation count + the
// install-wide forecast-skip scorecard lines). The panel fetches
// GET /api/v1/irrigation/tuning on demand, exactly like the zone
// detail's history Effect (hydrate-gated gloo_net into an RwSignal;
// try_* accessors in detached continuations per the disposed-signal
// discipline). App-level consumers (the Zones nav badge, ZonesPage,
// IrrigationPage) share ONE fetch via the TuningSummary context the App
// root provides (provide_tuning_summary); use_tuning_report hands every
// caller that same signal instead of per-surface fetches.

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

/// App-scoped invalidation counter for the shared tuning report. The App
/// root provides it (see `provide_tuning_summary`); the Apply / snooze /
/// dismiss / undo continuations bump it from wherever they run, so the
/// nav badge, the Suggestions KPI, the zone-card pills, and the
/// recommendation-aware auto-select all reflect the change without a
/// reload. Read as an Option everywhere so a surface mounted outside the
/// app shell (tests, isolated mounts) degrades to its own fetch.
#[derive(Clone, Copy)]
pub struct TuningEpoch(pub RwSignal<u32>);

/// App-level shared tuning-report signal (see `provide_tuning_summary`).
/// ONE fetch feeds every consumer: the Zones nav badge (desktop +
/// mobile), ZonesPage, and IrrigationPage. Beside the signal rides a
/// monotonic fetch generation: every fetch captures a token at spawn and
/// commits only while that token is still the newest, so an older
/// response resolving after a newer one can never overwrite it (the
/// epoch can bump twice within one round-trip: snooze then undo).
#[derive(Clone, Copy)]
pub struct TuningSummary {
    report: RwSignal<Option<TuningReport>>,
    generation: StoredValue<u32>,
}

impl TuningSummary {
    pub fn new() -> Self {
        Self {
            report: RwSignal::new(None),
            generation: StoredValue::new(0),
        }
    }

    /// The shared report signal every consumer renders from.
    pub fn report(&self) -> RwSignal<Option<TuningReport>> {
        self.report
    }

    /// Open a new fetch generation and return its token. Any in-flight
    /// fetch holding an older token is superseded: its `commit` becomes
    /// a no-op. None when the arena slot is gone (app teardown).
    pub fn begin_fetch(&self) -> Option<u32> {
        self.generation.try_update_value(|g| {
            *g += 1;
            *g
        })
    }

    /// Write a fetched report only while `token` is still the newest
    /// generation. Detached-continuation safe: try_* throughout.
    pub fn commit(&self, token: u32, rep: TuningReport) {
        if self.generation.try_get_value() == Some(token) {
            let _ = self.report.try_set(Some(rep));
        }
    }

    /// Write-through for a surface that fetched fresh on its own (the
    /// zone panel's per-mount read): opens a new generation AND commits
    /// in one step, so it supersedes any older in-flight epoch fetch
    /// and the panel can never disagree with the pills/KPI/badge in the
    /// same viewport.
    pub fn write_fresh(&self, rep: TuningReport) {
        if self
            .generation
            .try_update_value(|g| {
                *g += 1;
                *g
            })
            .is_some()
        {
            let _ = self.report.try_set(Some(rep));
        }
    }
}

impl Default for TuningSummary {
    fn default() -> Self {
        Self::new()
    }
}

/// Bump the app-wide TuningEpoch so the shared report re-fetches. Called
/// from the on-mount effects of the tuning surfaces (ZonesPage,
/// IrrigationPage), restoring the per-mount freshness the page-local
/// fetches had before the report was lifted into app context: entering a
/// tuning surface re-reads the report, so the badge/KPI/pills reflect
/// server-side changes (the nightly recompute, an apply from another
/// device) without a reload. A quiet no-op without the app context.
pub fn refresh_tuning_report() {
    if let Some(e) = use_context::<TuningEpoch>() {
        let _ = e.0.try_update(|n| *n += 1);
    }
}

/// Provide the app-level tuning contexts: the shared report signal and
/// the epoch that invalidates it. Hydrate fetches once on mount,
/// re-fetches whenever the epoch bumps (tuning actions, tuning-surface
/// mounts via `refresh_tuning_report`), and re-fetches when the document
/// becomes visible again, so a long-lived PWA tab picks up server-side
/// changes on re-focus. SSR provides the same contexts but never
/// fetches, so the SSR first frame and hydrate's first frame agree (no
/// badge, no count) until the client fetch resolves. A fetch error keeps
/// the last loaded report (or None on the first load), so consumers
/// render their no-report state, never a spinner. Every fetch runs under
/// the generation guard: an older response can never overwrite a newer
/// one.
pub fn provide_tuning_summary() {
    let epoch = TuningEpoch(RwSignal::new(0));
    let summary = TuningSummary::new();
    provide_context(epoch);
    provide_context(summary);
    #[cfg(feature = "hydrate")]
    {
        Effect::new(move |_| {
            let _ = epoch.0.get();
            let Some(token) = summary.begin_fetch() else {
                return;
            };
            leptos::task::spawn_local(async move {
                if let Ok(rep) = fetch_report().await {
                    // Detached continuation: commit is try_* throughout
                    // and drops the write if a newer fetch superseded it.
                    summary.commit(token, rep);
                }
            });
        });
        // Long-lived PWA tabs never re-navigate, so becoming visible
        // again is their re-entry point: bump the epoch when the tab
        // comes back so the badge and KPIs catch up with the server.
        // forget(): the listener lives for the app lifetime, like the
        // is_mobile media-query listener in app.rs.
        if let Some(doc) = web_sys::window().and_then(|w| w.document()) {
            let listener = gloo_events::EventListener::new(&doc, "visibilitychange", move |_| {
                let hidden = web_sys::window()
                    .and_then(|w| w.document())
                    .map(|d| d.hidden())
                    .unwrap_or(true);
                if !hidden {
                    let _ = epoch.0.try_update(|n| *n += 1);
                }
            });
            listener.forget();
        }
    }
}

/// The shared tuning-report signal. When the app-level summary context is
/// provided (the normal case: App() calls `provide_tuning_summary`), every
/// caller gets the SAME signal fed by the one app-level fetch, so pages
/// never double-fetch what the nav badge already loaded. Without the
/// context (isolated mounts) it falls back to a page-local signal with
/// its own hydrate fetch, re-fetched when a TuningEpoch context bumps and
/// guarded by its own generation counter against out-of-order responses.
/// None until loaded; stays None on a fetch error (every consumer
/// renders its no-report state).
pub fn use_tuning_report() -> RwSignal<Option<TuningReport>> {
    if let Some(shared) = use_context::<TuningSummary>() {
        return shared.report();
    }
    let report: RwSignal<Option<TuningReport>> = RwSignal::new(None);
    #[cfg(feature = "hydrate")]
    {
        let epoch = use_context::<TuningEpoch>();
        let generation: StoredValue<u32> = StoredValue::new(0);
        Effect::new(move |_| {
            if let Some(e) = epoch {
                let _ = e.0.get();
            }
            let Some(token) = generation.try_update_value(|g| {
                *g += 1;
                *g
            }) else {
                return;
            };
            leptos::task::spawn_local(async move {
                if let Ok(rep) = fetch_report().await {
                    // Detached continuation: the route may be gone by
                    // now, and a newer fetch may have superseded this one.
                    if generation.try_get_value() == Some(token) {
                        let _ = report.try_set(Some(rep));
                    }
                }
            });
        });
    }
    report
}

/// Count of zones with an ACTIVE suggestion (the server strips dismissed
/// and snoozed ones from `recommendation` before it answers, so
/// `recommendation_count` already means active-only), for the nav
/// badges. Zero until the shared report loads and zero when no summary
/// context exists, so SSR and hydrate's first frame both render no badge.
pub fn use_suggestion_count() -> Signal<usize> {
    let shared = use_context::<TuningSummary>();
    Signal::derive(move || {
        shared
            .and_then(|s| s.report().get())
            .map(|rep| recommendation_count(&rep))
            .unwrap_or(0)
    })
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
/// row: (row label, "from -> to"). Depth fields convert at the display
/// boundary via `prefs`.
pub(crate) fn delta_line(
    rec: &TuningRecommendation,
    prefs: crate::components::units_fmt::UnitPrefs,
) -> (String, String) {
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
        fmt_rec_value(&rec.field, &rec.current_value, false, prefs),
        fmt_rec_value(&rec.field, &rec.suggested_value, true, prefs)
    );
    (label, value)
}

fn fmt_rec_value(
    field: &str,
    v: &serde_json::Value,
    suggested: bool,
    prefs: crate::components::units_fmt::UnitPrefs,
) -> String {
    let unit = match field {
        "max_run_minutes" => " min",
        "sessions_per_week" => "/wk",
        "precip_rate_mm_hr" => " mm/hr",
        "root_depth_mm" => " mm",
        _ => "",
    };
    match v {
        // Null means "the default" on the CURRENT side: for the run
        // limit that default is a real number (60 min); everywhere else
        // name it plainly. A null SUGGESTED weekly target is the
        // raise-or-clear recommendation, and applying it clears the
        // target, which under the soil model means NO ceiling at all;
        // "default" said the opposite of the effect.
        serde_json::Value::Null => match field {
            "max_run_minutes" => "60 min".to_string(),
            "weekly_budget_in" if suggested => "cleared (no ceiling)".to_string(),
            _ => "default".to_string(),
        },
        serde_json::Value::String(s) => s.replace('_', " "),
        // The weekly target is a depth in inches on the wire; render it
        // in the viewer's depth unit like every other depth on the page.
        n if field == "weekly_budget_in" => match n.as_f64() {
            Some(v) => crate::components::units_fmt::depth_phrase_in(v, prefs),
            None => format!("{n}"),
        },
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
        // The panel's own generation guard: a slug switch or an Apply
        // refetch can start a second fetch while the first is in
        // flight, and the older response must not win.
        let shared = use_context::<TuningSummary>();
        let generation: StoredValue<u32> = StoredValue::new(0);
        Effect::new(move |_| {
            let _ = slug.get();
            let _ = refetch.get();
            report.set(None);
            let Some(token) = generation.try_update_value(|g| {
                *g += 1;
                *g
            }) else {
                return;
            };
            leptos::task::spawn_local(async move {
                let result = fetch_report().await;
                // Detached continuation: the route may be gone by now,
                // and a newer panel fetch may have superseded this one.
                if generation.try_get_value() != Some(token) {
                    return;
                }
                // Write-through: the panel's fresh read also feeds the
                // shared summary, so the pills, the Suggestions KPI, the
                // nav badge, and the auto-select can never disagree with
                // the panel in the same viewport (it supersedes any
                // older in-flight epoch fetch via the generation guard).
                if let (Some(s), Ok(rep)) = (shared, result.as_ref()) {
                    s.write_fresh(rep.clone());
                }
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
                            // collapse into ONE quiet disclosure. When the
                            // zone's suggestion is dismissed/snoozed the
                            // server appends the muted annotation last; pull
                            // it out of the notes so it renders in place
                            // with its Undo.
                            let mut all_lines = zt.lines.clone();
                            let dismissed_note = if zt.dismissed {
                                all_lines.pop()
                            } else {
                                None
                            };
                            let mut lines = all_lines.into_iter();
                            let lead = lines.next();
                            // The soil-vs-weekly comparison surfaces
                            // beside the lead when the two models
                            // DIVERGE (its line starts with the
                            // divergence prefix); agreement stays a
                            // quiet data note. Decision support does
                            // not belong behind the disclosure the
                            // provenance rows live in.
                            let (compare, notes): (Vec<String>, Vec<String>) = lines.partition(|l| {
                                l.starts_with(crate::ha::snapshot::SOIL_DIVERGENCE_PREFIX)
                            });
                            let window_days = rep.window_days;
                            let card = zt.recommendation.clone().map(|rec| {
                                let zone_slug = zt.slug.clone();
                                let zone_name = zt.display_name.clone();
                                view! { <TuningRecommendationCard rec zone_slug zone_name window_days applying refetch/> }
                            });
                            let undo = dismissed_note.map(|note| {
                                let zone_slug = zt.slug.clone();
                                let fields = zt.dismissed_fields.clone();
                                view! { <DismissedNote note zone_slug fields refetch/> }
                            });
                            view! {
                                {lead.map(|l| view! {
                                    <p class="zone-tuning__line zone-tuning__line--lead">{l}</p>
                                })}
                                {compare
                                    .into_iter()
                                    .map(|l| view! {
                                        <p class="zone-tuning__line zone-tuning__line--compare">
                                            {l}
                                        </p>
                                    })
                                    .collect_view()}
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
                                {undo}
                            }
                            .into_any()
                        }
                    }
                }
            }}
        </section>
    }
}

/// The muted dismissed/snoozed annotation with its Undo. Undo posts
/// undismiss for every silenced field on the zone and bumps the shared
/// TuningEpoch so all surfaces update in place.
#[component]
fn DismissedNote(
    note: String,
    zone_slug: String,
    fields: Vec<String>,
    refetch: RwSignal<u32>,
) -> impl IntoView {
    let epoch = use_context::<TuningEpoch>();
    let busy = RwSignal::new(false);
    let on_undo = move |_: leptos::ev::MouseEvent| {
        #[cfg(not(feature = "hydrate"))]
        let _ = (&zone_slug, &fields, busy, refetch, epoch);
        #[cfg(feature = "hydrate")]
        {
            use gloo_net::http::Request;
            if busy.get_untracked() {
                return;
            }
            busy.set(true);
            let slug = zone_slug.clone();
            let fields = fields.clone();
            leptos::task::spawn_local(async move {
                let mut failed: Option<(u16, String)> = None;
                for field in fields {
                    let body = serde_json::json!({
                        "zone_slug": slug.clone(),
                        "field": field,
                    });
                    let result: Result<(), (u16, String)> = async {
                        let resp = Request::post("/api/v1/irrigation/tuning/undismiss")
                            .json(&body)
                            .map_err(|e| (0u16, e.to_string()))?
                            .send()
                            .await
                            .map_err(|e| (0u16, e.to_string()))?;
                        if !resp.ok() {
                            let status = resp.status();
                            let text = resp.text().await.unwrap_or_default();
                            return Err((status, text));
                        }
                        Ok(())
                    }
                    .await;
                    if let Err(e) = result {
                        failed = Some(e);
                    }
                }
                let _ = busy.try_set(false);
                match failed {
                    None => {
                        crate::components::ui::use_toast().success("Suggestion restored.");
                    }
                    Some((status, text)) => {
                        crate::components::ui::use_toast().error(
                            crate::components::settings_ui::save_error_message(status, &text),
                        );
                    }
                }
                let _ = refetch.try_update(|n| *n += 1);
                if let Some(e) = epoch {
                    let _ = e.0.try_update(|n| *n += 1);
                }
            });
        }
    };
    view! {
        <p class="zone-tuning__line zone-tuning__line--muted zone-tuning__dismissed">
            {note}
            " "
            <button class="zone-tuning__undo is-interactive" on:click=on_undo>"Undo"</button>
        </p>
    }
}

/// One recommendation: attention-striped card with a SUGGESTION pill, a
/// status-chip confidence, the bumped headline, the mono current ->
/// suggested row, expandable evidence, and Apply. A max_run_minutes
/// suggestion above 60 gates the Apply behind the shared ConfirmSheet
/// (the same override-style confirm the zone editor uses). `window_days`
/// is the window the report was fetched at; the apply body echoes it so
/// the server re-derives at the same window. Beside Apply: Snooze
/// (30 days, id-keyed) and a subtle do-not-suggest-again action
/// (field-keyed, survives value drift).
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
    // Depth values in the delta row follow the display-units setting;
    // read prefs.get() in the render closure so the post-hydration
    // preference load re-renders the row.
    let prefs = crate::components::units_fmt::use_unit_prefs();
    let rec_for_delta = rec.clone();
    let delta = move || delta_line(&rec_for_delta, prefs.get());
    let delta_label = {
        let d = delta.clone();
        move || d().0
    };
    let delta_value = move || delta().1;
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
    let confirm_minutes =
        suggested_minutes.unwrap_or(crate::config::schema::DEFAULT_MAX_RUN_MINUTES);

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

    // Snooze / permanent dismissal. Both post the same endpoint and bump
    // the shared TuningEpoch so every surface (cards, KPI, strip,
    // auto-select) updates in place.
    let dismiss_busy = RwSignal::new(false);
    let rec_for_dismiss = rec.clone();
    let slug_for_dismiss = zone_slug.clone();
    let do_dismiss = Callback::new(move |kind: &'static str| {
        #[cfg(not(feature = "hydrate"))]
        let _ = (
            &rec_for_dismiss,
            &slug_for_dismiss,
            dismiss_busy,
            refetch,
            epoch,
            kind,
        );
        #[cfg(feature = "hydrate")]
        {
            use gloo_net::http::Request;
            if dismiss_busy.get_untracked() {
                return;
            }
            dismiss_busy.set(true);
            let body = serde_json::json!({
                "zone_slug": slug_for_dismiss.clone(),
                "field": rec_for_dismiss.field.clone(),
                "recommendation_id": rec_for_dismiss.id.clone(),
                "kind": kind,
            });
            leptos::task::spawn_local(async move {
                let outcome: Result<(), (u16, String)> = async {
                    let resp = Request::post("/api/v1/irrigation/tuning/dismiss")
                        .json(&body)
                        .map_err(|e| (0u16, e.to_string()))?
                        .send()
                        .await
                        .map_err(|e| (0u16, e.to_string()))?;
                    if !resp.ok() {
                        let status = resp.status();
                        let text = resp.text().await.unwrap_or_default();
                        return Err((status, text));
                    }
                    Ok(())
                }
                .await;
                let _ = dismiss_busy.try_set(false);
                match outcome {
                    Ok(()) => {
                        if kind == "snooze" {
                            crate::components::ui::use_toast().success("Snoozed for 30 days.");
                        } else {
                            crate::components::ui::use_toast()
                                .success("Dismissed. This suggestion will not return.");
                        }
                    }
                    Err((status, text)) => {
                        crate::components::ui::use_toast().error(
                            crate::components::settings_ui::save_error_message(status, &text),
                        );
                    }
                }
                let _ = refetch.try_update(|n| *n += 1);
                if let Some(e) = epoch {
                    let _ = e.0.try_update(|n| *n += 1);
                }
            });
        }
    });
    let on_snooze = move |_: leptos::ev::MouseEvent| do_dismiss.run("snooze");
    let dismiss_cb = do_dismiss;
    let on_dismiss_forever = move |_: leptos::ev::MouseEvent| dismiss_cb.run("permanent");

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
                <Button
                    variant="secondary"
                    loading=Signal::derive(move || dismiss_busy.get())
                    on_click=Callback::new(on_snooze)
                >"Snooze 30 days"</Button>
            </div>
            <button
                class="zone-tuning__dismiss is-interactive"
                on:click=on_dismiss_forever
            >"Do not suggest this again"</button>
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
                    ..Default::default()
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
    fn badge_count_is_active_only_a_dismissed_zone_adds_nothing() {
        // The server strips a dismissed/snoozed suggestion out of
        // `recommendation` and flags the zone via `dismissed` instead, so
        // the count the nav badge renders is active-only by construction.
        let mut rep = report(vec![("active", true), ("silenced", false)], None, None);
        rep.zones[1].dismissed = true;
        rep.zones[1].dismissed_fields = vec!["sessions_per_week".into()];
        assert_eq!(recommendation_count(&rep), 1);
        assert!(!recommended_slugs(&rep).contains("silenced"));
    }

    #[test]
    fn use_tuning_report_consumes_the_shared_summary_signal() {
        // With the app-level summary provided, every caller gets the SAME
        // signal: one fetch feeds all surfaces, and ZonesPage can never
        // double-fetch what the nav badge already loaded.
        let owner = Owner::new();
        owner.set();
        provide_context(TuningSummary::new());
        let a = use_tuning_report();
        let b = use_tuning_report();
        a.set(Some(report(vec![("x", true)], None, None)));
        assert_eq!(
            b.get_untracked().map(|r| recommendation_count(&r)),
            Some(1),
            "second caller must observe the first caller's write"
        );
        let ctx = use_context::<TuningSummary>().expect("summary context");
        assert!(ctx.report().get_untracked().is_some());
        assert_eq!(use_suggestion_count().get_untracked(), 1);
    }

    #[test]
    fn suggestion_count_is_zero_without_a_loaded_report() {
        // No summary context (isolated mount) and an unloaded report must
        // both render NO badge: zero, never a placeholder or spinner.
        let owner = Owner::new();
        owner.set();
        assert_eq!(use_suggestion_count().get_untracked(), 0);
        provide_context(TuningSummary::new());
        assert_eq!(use_suggestion_count().get_untracked(), 0);
    }

    #[test]
    fn refresh_bumps_the_app_epoch_and_noops_without_it() {
        // The tuning surfaces call this from their on-mount effects:
        // entering the page re-fetches the shared report. Without the app
        // context (isolated mounts) it must be a quiet no-op.
        let owner = Owner::new();
        owner.set();
        refresh_tuning_report();
        let epoch = TuningEpoch(RwSignal::new(0));
        provide_context(epoch);
        refresh_tuning_report();
        assert_eq!(epoch.0.get_untracked(), 1);
        refresh_tuning_report();
        assert_eq!(epoch.0.get_untracked(), 2);
    }

    #[test]
    fn an_older_response_cannot_overwrite_a_newer_one() {
        // Two fetches race (snooze bump then undo bump inside one round
        // trip): the response for the OLDER generation resolves last and
        // must be dropped, whichever order the commits arrive in.
        let owner = Owner::new();
        owner.set();
        let summary = TuningSummary::new();
        let older = summary.begin_fetch().expect("token");
        let newer = summary.begin_fetch().expect("token");
        summary.commit(newer, report(vec![("a", true), ("b", true)], None, None));
        summary.commit(older, report(vec![("stale", true)], None, None));
        assert_eq!(
            summary
                .report()
                .get_untracked()
                .map(|r| recommendation_count(&r)),
            Some(2),
            "the older response must not overwrite the newer one"
        );
    }

    #[test]
    fn panel_write_through_updates_the_shared_signal_and_supersedes() {
        // The zone panel's fresh per-mount read feeds the shared summary
        // (so panel and pills/KPI/badge agree in one viewport), and it
        // supersedes an older epoch fetch still in flight.
        let owner = Owner::new();
        owner.set();
        let summary = TuningSummary::new();
        provide_context(summary);
        let seen = use_tuning_report();
        let in_flight = summary.begin_fetch().expect("token");
        summary.write_fresh(report(vec![("fresh", true)], None, None));
        assert!(
            seen.get_untracked().is_some(),
            "consumers must see the panel's write-through"
        );
        summary.commit(
            in_flight,
            report(vec![("stale", true), ("x", true)], None, None),
        );
        assert_eq!(
            summary
                .report()
                .get_untracked()
                .map(|r| recommendation_count(&r)),
            Some(1),
            "the pre-write-through epoch fetch must be discarded"
        );
        assert!(recommended_slugs(&summary.report().get_untracked().unwrap()).contains("fresh"));
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
        let p = crate::components::units_fmt::UnitPrefs::default();
        // Unset current renders the real 60 minute default, not "null".
        let r = rec("max_run_minutes", serde_json::Value::Null, json!(116));
        assert_eq!(
            delta_line(&r, p),
            ("Run limit".to_string(), "60 min -> 116 min".to_string())
        );
        let r = rec("sessions_per_week", json!(2), json!(3));
        assert_eq!(
            delta_line(&r, p),
            ("Sessions per week".to_string(), "2/wk -> 3/wk".to_string())
        );
        let r = rec("soil_texture", json!("sandy_loam"), json!("loam"));
        assert_eq!(
            delta_line(&r, p),
            ("Soil texture".to_string(), "sandy loam -> loam".to_string())
        );
    }

    /// The weekly target's delta row: depths convert at the display
    /// boundary, and a null SUGGESTED value renders the real effect of
    /// applying it (the target clears and nothing caps) instead of
    /// "default", which said the opposite.
    #[test]
    fn delta_line_weekly_target_converts_and_names_the_clear() {
        let p = crate::components::units_fmt::UnitPrefs::default();
        let r = rec("weekly_budget_in", json!(1.3), json!(1.5));
        assert_eq!(
            delta_line(&r, p),
            ("Weekly target".to_string(), "1.30\" -> 1.50\"".to_string())
        );
        let metric = crate::components::units_fmt::METRIC;
        assert_eq!(
            delta_line(&r, metric),
            (
                "Weekly target".to_string(),
                "33.0 mm -> 38.1 mm".to_string()
            )
        );
        // Raise-or-clear: suggested null clears the ceiling; current
        // null stays "default" (an inferred target).
        let r = rec("weekly_budget_in", json!(1.3), serde_json::Value::Null);
        assert_eq!(
            delta_line(&r, p),
            (
                "Weekly target".to_string(),
                "1.30\" -> cleared (no ceiling)".to_string()
            )
        );
        let r = rec("weekly_budget_in", serde_json::Value::Null, json!(1.5));
        assert_eq!(
            delta_line(&r, p),
            ("Weekly target".to_string(), "default -> 1.50\"".to_string())
        );
    }
}
