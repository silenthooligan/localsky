// SettingsDataSources. Per-field PRIORITY + BACKUP-CHAIN editor: each headline
// reading (temperature, humidity, wind, rain, pressure, solar/UV) has an ORDERED
// chain of sources; the top source that is reporting now wins, and if it goes
// quiet the next takes over, so a reading is never lost.
//
// Reads two things:
//   * GET /api/config/field_sources -> the user-facing fields, every enabled
//     source (with its region_priority + honest nature), the saved custom chains,
//     and the deployment region label.
//   * GET /api/v1/irrigation/snapshot -> `field_sources`, the LIVE owner of
//     each field right now (so exactly one chain row reads "reporting now").
//
// Each field renders an ordered list of its candidate sources. A field with no
// saved chain shows the region-DEFAULT order ("Automatic", candidates by
// region_priority DESC); a reordered field shows its saved order ("Custom"). The
// user reorders by DRAG-AND-DROP (HTML5 drag carrying the source index) or the
// up/down arrows (keyboard a11y). On any reorder / reset the new order is written
// into `field_source_chains` and PUT back, exactly like settings/location.rs
// (GET -> splice -> PUT). The PUT handler RE-APPLIES the chain map to the live
// merge (runtime::apply_runtime_config), so it takes effect on the LIVE running
// engine on the next reading with no restart. A one-element chain is the old
// single pin; the legacy `field_source_overrides` map is folded in on load and
// cleared on save so the chain editor is the ONE home for per-field ownership.
// A whole-chain-stale field falls through to the priority merge, so a chain never
// blanks a reading.

use leptos::prelude::*;

use crate::components::settings_ui::SettingsResult;
use crate::components::ui::{Button, Panel};

/// One configured source the chain editor can rank for a field.
#[derive(Clone, Debug, Default)]
struct SourceCandidate {
    id: String,
    label: String,
    /// Source-tier id from the candidate API: "device" (a local physical sensor
    /// on the network), "cloud" (a cloud weather service supplying a CURRENT
    /// value for the field), or "forecast" (forecast-only for the field). Drives
    /// the tier chip + the plain-language descriptor.
    tier: String,
    /// Canonical source kind string (open_meteo / nws / ...), so the picker can
    /// look up the shared cloud-service descriptor at the point of choice.
    kind: String,
    /// Canonical WeatherField names this source provides.
    fields: Vec<String>,
    /// The region-default merge priority this source's kind seeds at the
    /// deployment location (higher wins). Used to sort a field's candidates into
    /// the region-DEFAULT chain order ("Automatic") before the user reorders it.
    region_priority: i32,
    /// The SOURCE-LEVEL honest data nature (a single headline value for the whole
    /// source), one of "device" / "observation" / "radar_qpe" / "nowcast" /
    /// "forecast". A FALLBACK: the badge is PER FIELD (`field_natures`), so Pirate
    /// reads "real-time" under Temperature but "model forecast" under Rain. This
    /// flat value is used only when a per-field entry is absent for the row's field.
    nature: String,
    /// The honest PER-FIELD data nature, `field_name -> nature`, one entry per field
    /// this source can own. Each nature is the same wire string as `nature`
    /// (device / observation / radar_qpe / nowcast / forecast). The chain row badges
    /// itself by looking up ITS field here (so a cloud source is badged measured-vs-
    /// model per reading), falling back to the source-level `nature` when its field
    /// is absent. Empty for an old endpoint that predates this field, in which case
    /// every row falls back to `nature` exactly as before.
    field_natures: std::collections::BTreeMap<String, String>,
}

/// One forecast-capable source the "Forecast source" picker can pin.
#[derive(Clone, Debug, Default)]
struct ForecastCandidate {
    id: String,
    label: String,
    /// Pretty kind label (e.g. "Open-Meteo forecast") for display.
    kind_label: String,
}

/// Everything the page renders against, loaded from /api/config/field_sources.
#[derive(Clone, Debug, Default)]
struct FieldSourcesData {
    /// (field_name, display label) in display order.
    user_fields: Vec<(String, String)>,
    sources: Vec<SourceCandidate>,
    /// field_name -> source id (the saved single pin). A pin is the special case
    /// of a one-element chain; when a field has no saved `field_source_chains`
    /// entry but has a pin here, the editor seeds its chain from the pin. Read in
    /// the hydrate build to seed the initial chains; ssr constructs but never
    /// reads it.
    #[cfg_attr(not(feature = "hydrate"), allow(dead_code))]
    overrides: std::collections::BTreeMap<String, String>,
    /// field_name -> ORDERED list of source ids (the saved custom chain). A field
    /// present here renders exactly this order ("Custom"); a field absent here (and
    /// absent from `overrides`) has no user chain and renders the region-default
    /// order ("Automatic"). Read in hydrate to seed the editable chains; ssr
    /// constructs but never reads it.
    #[cfg_attr(not(feature = "hydrate"), allow(dead_code))]
    field_source_chains: std::collections::BTreeMap<String, Vec<String>>,
    /// Enabled forecast-capable sources (the forecast-source picker options).
    forecast_candidates: Vec<ForecastCandidate>,
    /// Saved `forecast_provider` pin (a source id) or None for Auto (priority).
    /// Read in hydrate to seed the picker; ssr constructs but never reads it.
    #[cfg_attr(not(feature = "hydrate"), allow(dead_code))]
    forecast_provider: Option<String>,
    /// Short region label ("US" / "Europe" / "Global") for the deployment, used
    /// to tag an un-edited field "Automatic (<region> default)".
    region_label: String,
}

#[component]
pub fn SettingsDataSources(
    /// When true this is rendered INSIDE another settings pane (the Devices hub,
    /// design #4): drop the standalone page chrome (the `.settings-page` wrapper,
    /// the "← Settings" back-link, and the big page header) so it reads as a
    /// co-located section of its host rather than a nested page-within-a-page.
    /// Defaults false so the legacy standalone route (and its redirect target)
    /// still renders the full page.
    #[prop(optional)]
    embedded: bool,
) -> impl IntoView {
    // Loaded dataset (fields + candidate sources + saved chains).
    let data = RwSignal::new(FieldSourcesData::default());
    // field_name -> the currently-AUTHORED ordered chain of source ids. A field
    // ABSENT from this map has no user chain and renders the region-default order
    // ("Automatic"); a field PRESENT here (even a one-element chain) renders this
    // exact order ("Custom order"). Reordering / reset mutate this map, then PUT.
    let chains: RwSignal<std::collections::BTreeMap<String, Vec<String>>> =
        RwSignal::new(std::collections::BTreeMap::new());
    // field_name -> live owner label (from the irrigation snapshot).
    let live_owners: RwSignal<std::collections::BTreeMap<String, String>> =
        RwSignal::new(std::collections::BTreeMap::new());
    // The currently-PICKED forecast provider id ("" = Auto/priority).
    let forecast_pick: RwSignal<String> = RwSignal::new(String::new());
    // The live forecast-source label (from the irrigation snapshot's
    // forecast.forecast_source_label), so the page shows what supplies it now.
    let live_forecast: RwSignal<String> = RwSignal::new(String::new());
    // Snapshot freshness, used to qualify the empty "no source reporting yet"
    // captions: a field with a configured source but no live owner is "warming
    // up" with the last-refresh time, vs a field with no configured source
    // ("not assigned"). `live_refresh_epoch` is the snapshot's
    // last_refresh_epoch (0 = never); `live_tz` its IANA timezone (for timefmt).
    let live_refresh_epoch: RwSignal<i64> = RwSignal::new(0);
    let live_tz: RwSignal<String> = RwSignal::new(String::new());

    let loaded = RwSignal::new(false);
    let saving = RwSignal::new(false);
    // Set when a reorder/reset arrives WHILE a save is in flight; the in-flight
    // save's loop picks it up and re-PUTs the latest order, so a rapid reorder is
    // never silently lost (the mutation already landed in `chains`).
    let dirty = RwSignal::new(false);
    // Keyboard reorder re-renders the whole chain list, dropping focus. A move via
    // the up/down buttons records "field|new_index" + a bump counter here; the
    // effect below re-focuses that row once the DOM settles, so a keyboard user can
    // press the arrow repeatedly to walk a source through the chain. The counter
    // makes two moves to the same index distinct (so the effect re-runs) and, since
    // the effect never writes this signal, there is no reactive cycle.
    let focus_row: RwSignal<(String, u32)> = RwSignal::new((String::new(), 0));
    #[cfg(feature = "hydrate")]
    Effect::new(move |_| {
        let (sel, n) = focus_row.get();
        if n == 0 || sel.is_empty() {
            return;
        }
        wasm_bindgen_futures::spawn_local(async move {
            // Defer past the list re-render, then focus the row now at that index.
            gloo_timers::future::TimeoutFuture::new(0).await;
            if let Some(doc) = web_sys::window().and_then(|w| w.document()) {
                if let Ok(Some(el)) = doc.query_selector(&format!("[data-frow=\"{sel}\"]")) {
                    use wasm_bindgen::JsCast;
                    if let Some(h) = el.dyn_ref::<web_sys::HtmlElement>() {
                        let _ = h.focus();
                    }
                }
            }
        });
    });
    let result_msg = RwSignal::new(String::new());
    let result_ok = RwSignal::new(false);
    // Persistent, dismissible restart-required banner. Populated only when a
    // save returns restart_required=true (e.g. the spliced config also wired a
    // new source connection); per-field/forecast picks hot-reload and leave it
    // empty so a routine tunable save never raises it. `restart_dismissed`
    // hides it after the user acknowledges.
    let restart_reasons: RwSignal<Vec<String>> = RwSignal::new(Vec::new());
    let restart_dismissed = RwSignal::new(false);

    #[cfg(feature = "hydrate")]
    {
        Effect::new(move |_| {
            wasm_bindgen_futures::spawn_local(async move {
                if let Ok(d) = fetch_field_sources().await {
                    // Seed the editable chains from what is saved: a field with a
                    // saved custom chain uses it verbatim; a field with only a
                    // single pin (the legacy override) seeds a one-element chain
                    // (a pin IS a one-item chain); a field with neither stays
                    // ABSENT so it renders the region-default "Automatic" order.
                    let mut seed: std::collections::BTreeMap<String, Vec<String>> =
                        d.field_source_chains.clone();
                    for (field, id) in &d.overrides {
                        if !id.is_empty() {
                            seed.entry(field.clone())
                                .or_insert_with(|| vec![id.clone()]);
                        }
                    }
                    chains.set(seed);
                    forecast_pick.set(d.forecast_provider.clone().unwrap_or_default());
                    data.set(d);
                    loaded.set(true);
                }
                let live = fetch_live_owners().await;
                live_owners.set(live.owners);
                live_forecast.set(live.forecast_label);
                live_refresh_epoch.set(live.refresh_epoch);
                live_tz.set(live.tz);
            });
        });
    }

    // Sources that PROVIDE a given field (for that field's dropdown).
    let sources_for = move |field: &str| -> Vec<SourceCandidate> {
        let field = field.to_string();
        data.with(|d| {
            d.sources
                .iter()
                .filter(|s| s.fields.contains(&field))
                .cloned()
                .collect()
        })
    };

    // Persist the CURRENT authored chains + forecast pin to the live config. A
    // reorder / reset / forecast change all funnel through here (GET -> splice
    // `field_source_chains` + `forecast_provider` -> PUT), so the write path is
    // one thing. A `Copy` closure (it captures only `Copy` RwSignals), so the
    // per-row drag/arrow/reset handlers can each call it directly. Returns early
    // while a save is already in flight so a rapid re-drop never double-PUTs.
    let persist = move || {
        // A reorder/reset while a save is in flight cannot double-PUT; instead it
        // marks the state dirty and the running save's loop re-PUTs the latest
        // order, so the mutation (already applied to `chains`) is never lost.
        if saving.get() {
            dirty.set(true);
            return;
        }
        saving.set(true);
        result_msg.set(String::new());
        #[cfg(feature = "hydrate")]
        {
            wasm_bindgen_futures::spawn_local(async move {
                // Coalesce loop: each pass re-reads the LATEST authored chains, so a
                // reorder that arrived during the previous PUT is included. Break
                // when a PUT completes with no new reorder pending (or on error).
                // Assigned on every loop pass before the break (the loop always
                // runs at least once), so there is no unread initializer.
                let mut last: Result<Vec<String>, String>;
                loop {
                    dirty.set(false);
                    // Snapshot the chains; drop empty vecs so an emptied chain
                    // reverts the field to Automatic rather than persisting blank.
                    let chosen: std::collections::BTreeMap<String, Vec<String>> = chains
                        .get()
                        .into_iter()
                        .filter(|(_, v)| !v.is_empty())
                        .collect();
                    // "" (Auto by priority) -> None so we clear any saved pin.
                    let forecast_choice: Option<String> = {
                        let v = forecast_pick.get();
                        if v.is_empty() {
                            None
                        } else {
                            Some(v)
                        }
                    };
                    last = patch_field_chains(chosen, forecast_choice).await;
                    if last.is_err() || !dirty.get() {
                        break;
                    }
                }
                match last {
                    Ok(reasons) => {
                        // Keep the toast consistent with the banner: only claim
                        // "(no restart)" when the change actually hot-reloaded.
                        // When the spliced config also touched something a boot
                        // must wire (non-empty reasons), defer to the banner
                        // below instead of contradicting it.
                        let msg = if reasons.is_empty() {
                            "Saved. Applied to the live engine; the new source order \
                             takes effect on the next reading (no restart)."
                        } else {
                            "Saved. The source order applied to the live engine; one \
                             change also needs a restart (see below)."
                        };
                        crate::components::settings_ui::toast_saved(result_msg, result_ok, msg);
                        // A restart is needed only when the saved config also
                        // touched something a boot must wire (rare here). Raise
                        // the dismissible banner with the server's reasons; an
                        // empty list (the tunable-only path) clears it.
                        restart_dismissed.set(false);
                        restart_reasons.set(reasons);
                        // Refresh the live-owner + forecast labels so the page
                        // reflects the new ownership as it takes effect.
                        let live = fetch_live_owners().await;
                        live_owners.set(live.owners);
                        live_forecast.set(live.forecast_label);
                        live_refresh_epoch.set(live.refresh_epoch);
                        live_tz.set(live.tz);
                    }
                    Err(e) => {
                        result_ok.set(false);
                        result_msg.set(e);
                    }
                }
                saving.set(false);
            });
        }
        #[cfg(not(feature = "hydrate"))]
        {
            saving.set(false);
            let _ = (chains, forecast_pick);
        }
    };
    let on_save = move |_| persist();

    // Forecast-source picker: which forecast provider drives the whole
    // forecast (daily/hourly, ET0, rain-tomorrow). Empty when no forecast
    // source is configured (rare: the default synthesizes Open-Meteo).
    let has_forecast = move || data.with(|d| !d.forecast_candidates.is_empty());
    let forecast_caption = move || {
        let live = live_forecast.get();
        if !live.is_empty() {
            return format!("Currently: {live}");
        }
        // No forecast label yet: warming up (with the last-refresh time when we
        // have one) rather than a bare "nothing here".
        let epoch = live_refresh_epoch.get();
        if epoch > 0 {
            let tz = live_tz.get();
            format!(
                "Warming up (no forecast yet, as of {})",
                crate::timefmt::format_hm(epoch, &tz)
            )
        } else {
            "Warming up (no forecast yet)".to_string()
        }
    };
    let forecast_options = move || {
        let cands = data.with(|d| d.forecast_candidates.clone());
        cands
            .into_iter()
            .map(|c| {
                let id = c.id.clone();
                let id_for_sel = c.id.clone();
                let sel = move || forecast_pick.get() == id_for_sel;
                let label = if c.kind_label.is_empty() {
                    c.label.clone()
                } else {
                    format!("{} ({})", c.label, c.kind_label)
                };
                view! {
                    <option value=id.clone() selected=sel>{label}</option>
                }
            })
            .collect_view()
    };

    // Resolve a raw owner id / writer label to a friendly display name at this
    // The ORDERED chain of source ids to render for a field. A field PRESENT in
    // `chains` renders that saved order verbatim ("Custom"); a field ABSENT
    // renders the region-DEFAULT order ("Automatic") = its candidates sorted by
    // region_priority DESC (higher wins), stable on ties, so an un-edited field
    // shows exactly the order the merge would arbitrate by (never blank).
    // The region-DEFAULT order: every candidate for the field, region_priority
    // DESC (higher wins), stable on ties. This is what an un-edited field shows.
    let default_order = move |field: &str| -> Vec<String> {
        let mut cands = sources_for(field);
        cands.sort_by_key(|c| std::cmp::Reverse(c.region_priority));
        cands.into_iter().map(|c| c.id).collect()
    };
    // The order to RENDER for a field. CRITICAL: this ALWAYS returns EVERY
    // candidate source, so the full backup chain is visible + orderable (the bug
    // was returning only the saved chain, hiding NOAA/NWS/etc. behind the one
    // pinned source). A saved chain sets the order for the sources it names; any
    // remaining candidate is appended in region-priority order.
    let effective_order = move |field: &str| -> Vec<String> {
        let by_priority = default_order(field);
        match chains.get().get(field) {
            Some(saved) => {
                // Keep EVERY saved id, including one not in the enabled-candidate
                // set: a DISABLED source pinned in the chain must still render
                // (struck-through "off, re-enable to use"), not silently vanish.
                // Then append any remaining candidate in region-priority order.
                let mut out: Vec<String> = saved.clone();
                for id in by_priority {
                    if !out.contains(&id) {
                        out.push(id);
                    }
                }
                out
            }
            None => by_priority,
        }
    };

    // Move a chain entry from `from` to `to` and persist. Materializes the
    // region-default order into `chains` first (so a first reorder of an
    // Automatic field turns it Custom), then splices + PUTs via `persist`.
    let move_to = move |field: String, from: usize, to: usize| {
        if from == to {
            return;
        }
        let order = effective_order(&field);
        if from >= order.len() || to >= order.len() {
            return;
        }
        let mut order = order;
        let moved = order.remove(from);
        order.insert(to, moved);
        chains.update(|m| {
            m.insert(field.clone(), order);
        });
        persist();
    };

    // Reset a field to its region-default order: drop its `chains` entry so it
    // renders "Automatic" again, then persist (clears the saved chain).
    let reset_field = move |field: String| {
        chains.update(|m| {
            m.remove(&field);
        });
        persist();
    };

    // Render one field row: the reading name + Automatic/Custom tag, then its
    // ordered chain of candidate sources (drag handle, ordinal, name, nature
    // badge, live-now marker, up/down keyboard controls).
    let field_rows = move || {
        let fields = data.with(|d| d.user_fields.clone());
        if fields.is_empty() {
            return ().into_any();
        }
        fields
            .into_iter()
            .map(|(field, label)| {
                let candidates = sources_for(&field);
                // Does any configured source provide this field at all? Drives
                // the empty-caption wording + the "no source" note.
                let has_candidate = !candidates.is_empty();
                let has_device = candidates.iter().any(|c| c.tier == "device");
                let has_cloud = candidates.iter().any(|c| c.tier == "cloud");
                let label_lower = label.to_lowercase();
                // Is this field on a user-authored order ("Custom") or the
                // region default ("Automatic")? A field absent from `chains` is
                // Automatic; present (even a one-item chain) is Custom.
                let field_for_tag_cls = field.clone();
                let field_for_reset_show = field.clone();
                let region = data.with(|d| d.region_label.clone());
                let field_for_tag2 = field.clone();
                // ONE predicate for "the user actually customized this field's
                // order": present in `chains` AND the effective order differs from
                // the region default (so a legacy pin that matches the top source
                // is NOT mislabeled Custom). Param-based so it stays Copy and the
                // tag TEXT, the tag CSS class, and the Reset control all agree.
                let is_custom = move |f: &str| -> bool {
                    chains.get().contains_key(f) && effective_order(f) != default_order(f)
                };
                let order_tag = move || {
                    if is_custom(&field_for_tag2) {
                        "Custom order".to_string()
                    } else if region.is_empty() {
                        "Automatic (region default)".to_string()
                    } else {
                        format!("Automatic ({region} default)")
                    }
                };
                // The live owner as its RAW writer label (the snapshot field_sources
                // value: a config id for a cloud source, the "Tempest" constant for
                // a Tempest station). chain_row matches this against each row's OWN
                // writer label, so exactly the right row marks "reporting now" even
                // for a station (whose display name differs from its label) or two
                // same-kind cloud sources (whose display names collide).
                let field_for_owner = field.clone();
                let live_owner_raw = move || live_owners.get().get(&field_for_owner).cloned();
                // No local device for this field but a cloud service can cover
                // it: an inviting affordance, not a dead end.
                let cloud_invite = (!has_device && has_cloud).then(|| {
                    let lbl = label_lower.clone();
                    view! {
                        <p class="data-source-row__cloud-invite">
                            {format!(
                                "No {lbl} sensor? A cloud weather service can be your \
                                 {lbl} source, no {lbl} hardware needed. Add one under \
                                 Devices and it joins this chain."
                            )}
                        </p>
                    }
                });
                // The ordered chain rows for this field.
                let field_for_chain = field.clone();
                let label_for_chain = label.clone();
                let chain_rows = move || {
                    let order = effective_order(&field_for_chain);
                    if order.is_empty() {
                        return view! {
                            <p class="data-source-chain__empty">
                                "No source reports this reading yet. Add one under Devices \
                                 and it joins this chain."
                            </p>
                        }
                        .into_any();
                    }
                    let owner = live_owner_raw();
                    let len = order.len();
                    let rows: Vec<_> = order
                        .iter()
                        .cloned()
                        .enumerate()
                        .map(|(idx, id)| {
                            let cand = candidates.iter().find(|c| c.id == id).cloned();
                            chain_row(
                                field_for_chain.clone(),
                                label_for_chain.clone(),
                                id,
                                cand,
                                idx,
                                len,
                                owner.clone(),
                                move_to,
                                focus_row,
                            )
                        })
                        .collect();
                    view! { <ol class="data-source-chain">{rows}</ol> }.into_any()
                };
                let field_for_reset = field.clone();
                view! {
                    <div class="data-source-row data-source-row--chain">
                        <div class="data-source-chain-head">
                            <div class="data-source-chain-head__title">
                                <span class="data-source-row__field">{label.clone()}</span>
                                <span
                                    class=move || if is_custom(&field_for_tag_cls) {
                                        "data-source-chain__tag data-source-chain__tag--custom"
                                    } else {
                                        "data-source-chain__tag"
                                    }
                                >
                                    {order_tag}
                                </span>
                            </div>
                            <Show when=move || is_custom(&field_for_reset_show)>
                                <button
                                    type="button"
                                    class="data-source-chain__reset"
                                    on:click={
                                        let field = field_for_reset.clone();
                                        move |_| reset_field(field.clone())
                                    }
                                >
                                    "Reset to automatic"
                                </button>
                            </Show>
                        </div>
                        {chain_rows}
                        <p class="data-source-chain__caption">
                            "Top source that is reporting wins; if it goes quiet the next "
                            "takes over. Drag a row (or use the up/down arrows) to reorder."
                        </p>
                        {(!has_candidate).then(|| view! {
                            <p class="data-source-chain__empty">
                                "No configured source reports this reading. Add one under Devices."
                            </p>
                        })}
                        <div class="data-source-row__explain">
                            {cloud_invite}
                        </div>
                    </div>
                }
            })
            .collect_view()
            .into_any()
    };

    // Empty-state when no sources are configured yet.
    let no_sources = move || data.with(|d| d.sources.is_empty());

    // Standalone page chrome (header + back-link). Suppressed when embedded in
    // the Devices hub, where the host already owns the page header and the
    // "Per-field sources + priority" section title.
    let header = (!embedded).then(|| {
        view! {
            <header class="settings-page__header">
                <a class="settings-page__back" href="/settings">"← Settings"</a>
                <h1 class="settings-page__title">"Data sources"</h1>
                <p class="settings-page__subtitle">
                    "Each reading has an ordered chain of sources: the top one that "
                    "is reporting now wins, and if it goes quiet the next takes over. "
                    "LocalSky sets a smart default order for your region; drag to make "
                    "your own. No weather hardware? A cloud weather service can supply "
                    "any reading's current value, and you decide which service backs "
                    "up which. A source only leads while it is reporting, so a reading "
                    "is never lost."
                </p>
            </header>
        }
    });
    // Outer wrapper: a full page when standalone, a plain block when embedded
    // (the host's `.settings-page` already provides the width + padding).
    let wrap_class = if embedded {
        "data-sources-embedded"
    } else {
        "settings-page"
    };

    view! {
        <div class=wrap_class>
            {header}

            <RestartBanner reasons=restart_reasons dismissed=restart_dismissed/>

            <Panel title="Forecast source".to_string() help_topic="sources">
                <p class="settings-page__subtitle">
                    "Pick which cloud service supplies your forecast: the daily and "
                    "hourly outlook, rain expected tomorrow, and the evapotranspiration "
                    "estimate the engine waters from. This is the main control if you "
                    "have no local hardware. \"Auto (follow the chain)\" follows the "
                    "chain of your forecast sources; Open-Meteo (free, no key) is the "
                    "built-in default at the end of the chain."
                </p>
                <Show
                    when=move || has_forecast()
                    fallback=|| view! {
                        <p class="settings-page__subtitle">
                            "No forecast source is configured. Add Open-Meteo (free, no "
                            "key) under Devices to get a forecast."
                        </p>
                    }
                >
                    <div class="data-source-row">
                        <div class="data-source-row__label">
                            <span class="data-source-row__field">"Forecast provider"</span>
                            <span class="data-source-row__owner">{forecast_caption}</span>
                        </div>
                        <select
                            class="ui-input data-source-row__picker"
                            on:change=move |ev| {
                                forecast_pick.set(event_target_value(&ev));
                            }
                        >
                            <option
                                value=""
                                selected=move || forecast_pick.get().is_empty()
                            >
                                "Auto (follow the chain)"
                            </option>
                            {forecast_options}
                        </select>
                    </div>
                </Show>
            </Panel>

            <Panel title="Per-field priority and backup chain".to_string() help_topic="sources">
                <p class="settings-page__subtitle">
                    "Each reading has an ORDERED chain of sources. The top source that is "
                    "reporting now wins; if it goes quiet the next takes over, so a reading "
                    "is never lost. LocalSky sets a smart default order for your region "
                    "(shown as \"Automatic\") and you can drag to make your own order "
                    "(\"Custom\"). This is the main control whether or not you have local "
                    "hardware: with cloud only, this is where you decide which service backs "
                    "up which."
                </p>
                // The full per-field chains ALWAYS render, even with no local
                // station: a cloud-only config is exactly where the order matters
                // most (which service backs up which). A field with no configured
                // source shows an inline "Add one under Devices" note rather than
                // suppressing the whole section.
                <div class="data-source-list">{field_rows}</div>
                // Soil is governed on a DIFFERENT axis (per zone, not a per-reading
                // chain), so it is out of scope here: cross-link to where it lives.
                <p class="data-source-chain__soil-note">
                    "Soil moisture is bound per zone in the "
                    <a href="/settings/zones" class="data-source-chain__soil-link">
                        "zone editor"
                    </a>
                    ", not as a per-reading chain here."
                </p>
            </Panel>

            // Advanced disclosure: the raw mechanics behind the friendly pickers
            // above (the per-field override map + the forecast-provider pin, both
            // as source ids, and a note that per-source priority lives on each
            // source's editor). Demoted out of the main flow so a customer never
            // has to read a priority number or an override map to use this page;
            // the per-field cloud override stays first-class above. Read-only,
            // for the curious + for support.
            <details class="settings-section-fold data-source-advanced">
                <summary class="settings-section-fold__summary">
                    "Advanced: the raw chain"
                    <span class="settings-section-fold__hint">"most people never open this"</span>
                </summary>
                <div class="settings-section-fold__body">
                    <p class="settings-page__subtitle">
                        "The region default order (\"Automatic\") comes from each source's "
                        "per-region priority. The chains below are the explicit custom "
                        "orders this page writes; a field not listed follows the default."
                    </p>
                    <dl class="data-source-advanced__kvs">
                        <div class="settings-kv">
                            <dt class="settings-kv__label">"field_source_chains"</dt>
                            <dd class="settings-kv__value">{move || {
                                let m = chains.get();
                                let pairs: Vec<String> = m
                                    .iter()
                                    .filter(|(_, v)| !v.is_empty())
                                    .map(|(k, v)| format!("{k} = [{}]", v.join(", ")))
                                    .collect();
                                if pairs.is_empty() {
                                    "(none, every field follows the region default)".to_string()
                                } else {
                                    pairs.join("; ")
                                }
                            }}</dd>
                        </div>
                        <div class="settings-kv">
                            <dt class="settings-kv__label">"forecast_provider"</dt>
                            <dd class="settings-kv__value">{move || {
                                let f = forecast_pick.get();
                                if f.is_empty() {
                                    "(auto, follow the chain)".to_string()
                                } else {
                                    f
                                }
                            }}</dd>
                        </div>
                    </dl>
                </div>
            </details>

            <div class="settings-actions">
                <Button
                    variant="primary"
                    disabled=Signal::derive(move || saving.get() || (no_sources() && !has_forecast()))
                    on_click=Callback::new(on_save)
                >
                    {move || if saving.get() { "Saving…" } else { "Save changes" }}
                </Button>
            </div>

            <SettingsResult result_msg=result_msg result_ok=result_ok/>

            <Show when=move || !loaded.get()>
                <p class="settings-page__subtitle" style="margin-top: 1rem">
                    "Loading sources from /api/config..."
                </p>
            </Show>
        </div>
    }
}

/// Humanize a raw source id/label for display when no friendly mapping exists:
/// underscores + hyphens to spaces, each word title-cased ("backyard_ow" ->
/// "Backyard Ow"). NEVER returns the raw slug, so an unmapped custom id never
/// leaks into a caption or chain row.
fn humanize_id(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for (i, word) in raw
        .split(['_', '-', ' '])
        .filter(|w| !w.is_empty())
        .enumerate()
    {
        if i > 0 {
            out.push(' ');
        }
        let mut chars = word.chars();
        if let Some(first) = chars.next() {
            out.extend(first.to_uppercase());
            out.push_str(&chars.as_str().to_lowercase());
        }
    }
    if out.is_empty() {
        raw.to_string()
    } else {
        out
    }
}

/// The measured-vs-model nature badge for a chain row, from the candidate's
/// `nature`: device / observation / radar_qpe are MEASURED (real readings),
/// forecast is a MODEL, nowcast is a real-time analysis. Colored + labeled so the
/// honest distinction reads at the point of ordering.
fn nature_badge(nature: &str) -> impl IntoView {
    let (cls, text) = match nature {
        "device" => ("data-source-chain__nature--measured", "your device"),
        "observation" => ("data-source-chain__nature--measured", "measured"),
        "radar_qpe" => ("data-source-chain__nature--measured", "radar measured"),
        "nowcast" => ("data-source-chain__nature--nowcast", "real-time"),
        "forecast" => ("data-source-chain__nature--model", "model forecast"),
        _ => ("data-source-chain__nature--measured", "measured"),
    };
    view! {
        <span class=format!("data-source-chain__nature {cls}")>{text}</span>
    }
}

/// Render ONE ordered-chain row: drag handle, ordinal, friendly source name,
/// nature badge, live-now marker, and up/down keyboard controls. `cand` is `None`
/// when the id names a source that is DISABLED or removed (the endpoint only lists
/// enabled sources): the row renders STRUCK-THROUGH "off, re-enable to use" rather
/// than silently dropping it, with an extra warning when it is the chain PRIMARY.
/// `owner` is the live owner's RAW writer label (a config id, or the "Tempest"
/// constant), matched against each row's own writer label so exactly one row reads
/// "reporting now" (a station and same-kind clouds all resolve correctly).
/// `move_to(field, from, to)` reorders + persists; the drag/drop + arrow handlers
/// all call it. `focus_row` re-focuses a keyboard-moved row after the re-render.
#[allow(clippy::too_many_arguments)]
fn chain_row<F>(
    field: String,
    label: String,
    id: String,
    cand: Option<SourceCandidate>,
    idx: usize,
    len: usize,
    owner: Option<String>,
    move_to: F,
    focus_row: RwSignal<(String, u32)>,
) -> impl IntoView
where
    F: Fn(String, usize, usize) + Copy + 'static,
{
    let ordinal = idx + 1;
    let is_first = idx == 0;
    let is_last = idx + 1 == len;

    // Friendly name + nature + whether the source is currently enabled/present.
    let (name, nature, present, descriptor) = match &cand {
        Some(c) => {
            let name = if c.tier == "cloud" {
                crate::components::sources_form::cloud_service_name(&c.kind).to_string()
            } else {
                // A device / station: prefer the friendly kind name, fall back to
                // a humanized id so a custom id never leaks raw.
                let friendly = crate::components::sources_form::friendly_source_name(&c.kind);
                if friendly == c.kind || friendly.is_empty() {
                    humanize_id(&c.label)
                } else {
                    friendly
                }
            };
            let desc =
                crate::components::sources_form::candidate_descriptor(&c.tier, &c.kind, &label);
            // PER-FIELD nature (deferred #10): badge THIS row by the nature for the
            // field it renders, so Pirate under Rain reads "model forecast" while
            // Pirate under Temperature reads "real-time". Fall back to the source-
            // level headline `nature` when no per-field entry exists (an old
            // endpoint, or a field the source has no specific nature for).
            let field_nature = c
                .field_natures
                .get(&field)
                .cloned()
                .unwrap_or_else(|| c.nature.clone());
            (name, field_nature, true, desc)
        }
        // Disabled / removed: humanize the saved id so it still reads as a name.
        None => (
            humanize_id(&id),
            String::new(),
            false,
            "This source is turned off. Re-enable it under Devices to use it.".to_string(),
        ),
    };

    // The live-now marker: the row whose WRITER LABEL matches the live owner is
    // "reporting now". Match by label (not display name): the snapshot owner is a
    // config id (cloud) or the "Tempest" constant (station), so a station row
    // (display name "Tempest UDP (LAN)") and two same-kind cloud rows (colliding
    // display names) all resolve correctly. A disabled row is "off"; the terminal
    // link of a multi-link chain is the "backstop"; everything else is "standby".
    // Mirrors the server-side crate::tempest::state::TEMPEST_LABEL ("Tempest"),
    // inlined because that merge-engine constant is ssr-only (not compiled into the
    // wasm/hydrate build). A Tempest station writes this literal into the snapshot
    // field_sources; every other source writes its config id.
    let writer_label = match cand.as_ref().map(|c| c.kind.as_str()) {
        Some("tempest_udp") | Some("tempest_ws") => "Tempest".to_string(),
        _ => id.clone(),
    };
    let is_owner = present && owner.as_deref() == Some(writer_label.as_str());
    let (marker_cls, marker_text) = if !present {
        ("data-source-chain__live--off", "off")
    } else if is_owner {
        ("data-source-chain__live--now", "reporting now")
    } else if is_last && len > 1 {
        ("data-source-chain__live--standby", "backstop")
    } else {
        ("data-source-chain__live--standby", "standby")
    };

    // Row classes: struck-through when off; owner-highlighted when reporting.
    let mut row_cls = String::from("data-source-chain__row entity-stripe entity-stripe--source");
    if !present {
        row_cls.push_str(" data-source-chain__row--off");
    }
    if is_owner {
        row_cls.push_str(" data-source-chain__row--owner");
    }

    // Handlers: drag carries the source index; drop reorders to this row's index.
    let field_ds = field.clone();
    let field_drop = field.clone();
    let field_up = field.clone();
    let field_down = field.clone();

    // A primary (index 0) that is OFF is the loudest problem: the reading it is
    // supposed to lead has silently fallen to the next link.
    let primary_off = (!present && is_first).then(|| {
        view! {
            <span class="data-source-chain__warn">
                "Primary is off, this reading is falling through to the next source"
            </span>
        }
    });

    view! {
        <li
            class=row_cls
            tabindex="-1"
            data-frow=format!("{field}\u{7c}{idx}")
            draggable="true"
            on:dragstart=move |ev| {
                let _ = &field_ds;
                #[cfg(feature = "hydrate")]
                {
                    if let Some(dt) = ev.data_transfer() {
                        let _ = dt.set_data("text/plain", &idx.to_string());
                        dt.set_effect_allowed("move");
                    }
                }
                #[cfg(not(feature = "hydrate"))]
                let _ = &ev;
            }
            on:dragover=move |ev| {
                // Required so the row is a valid drop target.
                ev.prevent_default();
            }
            on:drop=move |ev| {
                ev.prevent_default();
                let _ = &field_drop;
                #[cfg(feature = "hydrate")]
                {
                    if let Some(dt) = ev.data_transfer() {
                        if let Ok(from_s) = dt.get_data("text/plain") {
                            if let Ok(from) = from_s.parse::<usize>() {
                                // Direction-correct so a drop ALWAYS lands before
                                // the target row: dragging DOWN, removing the source
                                // first shifts the target up by one, so target at
                                // idx-1; dragging UP the target index is unchanged.
                                let to = if from < idx { idx.saturating_sub(1) } else { idx };
                                move_to(field_drop.clone(), from, to);
                            }
                        }
                    }
                }
            }
        >
            <span class="data-source-chain__handle" aria-hidden="true" title="Drag to reorder">
                "\u{283f}"
            </span>
            <span class="data-source-chain__ordinal">{ordinal}</span>
            <span class="data-source-chain__name" title=descriptor>{name.clone()}</span>
            {(!nature.is_empty()).then(|| nature_badge(&nature))}
            <span class=format!("data-source-chain__live {marker_cls}")>{marker_text}</span>
            {(!present).then(|| view! {
                <span class="data-source-chain__off-note">"off, re-enable to use"</span>
            })}
            {primary_off}
            <span class="data-source-chain__moves">
                <button
                    type="button"
                    class="data-source-chain__move"
                    aria-label=format!("Move {name} up")
                    disabled=is_first
                    on:click=move |_| {
                        if !is_first {
                            move_to(field_up.clone(), idx, idx - 1);
                            // Re-focus the row at its new index after the re-render.
                            focus_row.update(|(s, n)| {
                                *s = format!("{}\u{7c}{}", field_up, idx - 1);
                                *n += 1;
                            });
                        }
                    }
                >
                    "\u{25b2}"
                </button>
                <button
                    type="button"
                    class="data-source-chain__move"
                    aria-label=format!("Move {name} down")
                    disabled=is_last
                    on:click=move |_| {
                        if !is_last {
                            move_to(field_down.clone(), idx, idx + 1);
                            // Re-focus the row at its new index after the re-render.
                            focus_row.update(|(s, n)| {
                                *s = format!("{}\u{7c}{}", field_down, idx + 1);
                                *n += 1;
                            });
                        }
                    }
                >
                    "\u{25bc}"
                </button>
            </span>
        </li>
    }
}

/// Persistent, dismissible "Restart required to apply" banner. Shown after a
/// config save whose response carried restart_required=true; the `reasons` list
/// is the server's restart_reasons (each a human-readable line). Tunable changes
/// hot-reload and report restart_required=false, so callers pass an empty
/// `reasons` for those and the banner stays hidden.
///
/// Cross-component contract: any settings page that PUTs config can render this
/// by owning a `reasons: RwSignal<Vec<String>>` (empty = hidden) plus a
/// `dismissed: RwSignal<bool>` (reset to false on each save so a fresh
/// restart-required save re-shows it).
#[component]
pub fn RestartBanner(
    /// The server's restart_reasons. Empty keeps the banner hidden.
    reasons: RwSignal<Vec<String>>,
    /// Set true when the user dismisses; keeps the banner hidden until the next
    /// restart-required save resets it.
    dismissed: RwSignal<bool>,
) -> impl IntoView {
    let show = move || !reasons.get().is_empty() && !dismissed.get();
    view! {
        <Show when=show>
            <div
                class="setup-result"
                role="status"
                style="margin: 0 0 1rem; display: flex; gap: 0.75rem; \
                       align-items: flex-start; \
                       background: color-mix(in oklab, var(--accent-warn) 12%, transparent); \
                       color: var(--accent-warn); \
                       border: 1px solid var(--accent-warn);"
            >
                <div style="flex: 1 1 auto; min-width: 0;">
                    <strong>"Restart required to apply"</strong>
                    <p style="margin: 0.35rem 0 0; color: var(--text); font-size: 0.92rem;">
                        "Your change is saved, but it needs a container restart "
                        "to take effect. Everything else applied live."
                    </p>
                    <ul style="margin: 0.5rem 0 0; padding-left: 1.1rem; \
                               color: var(--text); font-size: 0.92rem;">
                        {move || reasons.get()
                            .into_iter()
                            .map(|r| view! { <li>{r}</li> })
                            .collect_view()}
                    </ul>
                </div>
                <button
                    type="button"
                    aria-label="Dismiss restart-required notice"
                    style="flex: 0 0 auto; align-self: flex-start; padding: 0.2rem 0.7rem; \
                           cursor: pointer; background: transparent; font: inherit; \
                           color: var(--accent-warn); border: 1px solid var(--accent-warn); \
                           border-radius: var(--radius-sm);"
                    on:click=move |_| dismissed.set(true)
                >
                    "Dismiss"
                </button>
            </div>
        </Show>
    }
}

#[cfg(feature = "hydrate")]
async fn fetch_field_sources() -> Result<FieldSourcesData, String> {
    use gloo_net::http::Request;
    let resp = Request::get("/api/config/field_sources")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("HTTP {}", resp.status()));
    }
    let v: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    let user_fields = v
        .get("user_fields")
        .and_then(|a| a.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|pair| {
                    let p = pair.as_array()?;
                    Some((
                        p.first()?.as_str()?.to_string(),
                        p.get(1)?.as_str()?.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    let sources = v
        .get("sources")
        .and_then(|a| a.as_array())
        .map(|arr| {
            arr.iter()
                .map(|s| SourceCandidate {
                    id: s
                        .get("id")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string(),
                    label: s
                        .get("label")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string(),
                    tier: s
                        .get("tier")
                        .and_then(|x| x.as_str())
                        .unwrap_or("cloud")
                        .to_string(),
                    kind: s
                        .get("kind")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string(),
                    fields: s
                        .get("fields")
                        .and_then(|x| x.as_array())
                        .map(|fa| {
                            fa.iter()
                                .filter_map(|f| f.as_str().map(str::to_string))
                                .collect()
                        })
                        .unwrap_or_default(),
                    region_priority: s
                        .get("region_priority")
                        .and_then(|x| x.as_i64())
                        .unwrap_or(0) as i32,
                    nature: s
                        .get("nature")
                        .and_then(|x| x.as_str())
                        .unwrap_or("device")
                        .to_string(),
                    // Per-field natures: an array of [field, nature] two-tuples ->
                    // field_name -> nature. Absent on an old endpoint, in which case
                    // the map is empty and every row falls back to `nature`.
                    field_natures: s
                        .get("field_natures")
                        .and_then(|x| x.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|pair| {
                                    let p = pair.as_array()?;
                                    Some((
                                        p.first()?.as_str()?.to_string(),
                                        p.get(1)?.as_str()?.to_string(),
                                    ))
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                })
                .collect()
        })
        .unwrap_or_default();
    let overrides = v
        .get("overrides")
        .and_then(|o| o.as_object())
        .map(|m| {
            m.iter()
                .filter_map(|(k, val)| Some((k.clone(), val.as_str()?.to_string())))
                .collect()
        })
        .unwrap_or_default();
    let field_source_chains = v
        .get("field_source_chains")
        .and_then(|o| o.as_object())
        .map(|m| {
            m.iter()
                .filter_map(|(k, val)| {
                    let ids: Vec<String> = val
                        .as_array()?
                        .iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect();
                    Some((k.clone(), ids))
                })
                .collect()
        })
        .unwrap_or_default();
    let region_label = v
        .get("region_label")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let forecast_candidates = v
        .get("forecast_candidates")
        .and_then(|a| a.as_array())
        .map(|arr| {
            arr.iter()
                .map(|s| {
                    let kind = s.get("kind").and_then(|x| x.as_str()).unwrap_or("");
                    ForecastCandidate {
                        id: s
                            .get("id")
                            .and_then(|x| x.as_str())
                            .unwrap_or("")
                            .to_string(),
                        label: s
                            .get("label")
                            .and_then(|x| x.as_str())
                            .unwrap_or("")
                            .to_string(),
                        kind_label: crate::components::sources_form::kind_pretty(kind).to_string(),
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    let forecast_provider = v
        .get("forecast_provider")
        .and_then(|x| x.as_str())
        .map(str::to_string);
    Ok(FieldSourcesData {
        user_fields,
        sources,
        overrides,
        field_source_chains,
        forecast_candidates,
        forecast_provider,
        region_label,
    })
}

/// Live-ownership read off the irrigation snapshot, plus the freshness context
/// the empty captions need to tell "warming up" from "not assigned".
#[cfg(feature = "hydrate")]
#[derive(Default)]
struct LiveOwners {
    /// field_name -> the source label currently driving that reading.
    owners: std::collections::BTreeMap<String, String>,
    /// Live forecast-source label (forecast.forecast_source_label).
    forecast_label: String,
    /// UTC epoch of the most recent successful poll (last_refresh_epoch); 0 if
    /// LocalSky has never refreshed (cold start), which reads as "warming up".
    refresh_epoch: i64,
    /// Deployment IANA timezone, for formatting refresh_epoch via timefmt.
    tz: String,
}

/// Read the live per-field owner labels off the irrigation snapshot's
/// `field_sources` map, the live forecast-source label off
/// `forecast.forecast_source_label`, and the snapshot's freshness
/// (last_refresh_epoch + timezone). Best-effort: defaults on any failure just
/// mean the page shows "Warming up" rather than erroring.
#[cfg(feature = "hydrate")]
async fn fetch_live_owners() -> LiveOwners {
    use gloo_net::http::Request;
    let Ok(resp) = Request::get("/api/v1/irrigation/snapshot").send().await else {
        return Default::default();
    };
    let Ok(v) = resp.json::<serde_json::Value>().await else {
        return Default::default();
    };
    let owners = v
        .get("field_sources")
        .and_then(|o| o.as_object())
        .map(|m| {
            m.iter()
                .filter_map(|(k, val)| Some((k.clone(), val.as_str()?.to_string())))
                .collect()
        })
        .unwrap_or_default();
    let forecast_label = v
        .get("forecast")
        .and_then(|f| f.get("forecast_source_label"))
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let refresh_epoch = v
        .get("last_refresh_epoch")
        .and_then(|x| x.as_i64())
        .unwrap_or(0);
    let tz = v
        .get("timezone")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    LiveOwners {
        owners,
        forecast_label,
        refresh_epoch,
        tz,
    }
}

/// GET the live config, splice `field_source_chains` + `forecast_provider`, PUT
/// it back. Same round-trip as settings/location.rs so untouched config + secrets
/// survive. `chosen` is the field -> ORDERED source-id list; an empty map clears
/// every custom chain (all fields revert to the region default). `forecast_choice`
/// None = Auto (clears any saved pin).
///
/// The legacy single-pin map `field_source_overrides` is CLEARED here: the chain
/// editor now fully owns per-field ownership (a pin is just a one-element chain,
/// and the editor already seeds its chains from any saved pin on load), so writing
/// chains and clearing the old pins keeps ONE representation and avoids a stale
/// pin silently overriding a newly-cleared chain.
///
/// Returns the restart_reasons the PUT response carried (empty when the change
/// hot-reloaded). Per-field chains + forecast pins re-apply to the live engine
/// with no restart, so this is normally empty; it is non-empty only when the
/// spliced config also touched something a boot must wire (e.g. a new source
/// connection), in which case the caller raises the restart-required banner.
#[cfg(feature = "hydrate")]
async fn patch_field_chains(
    chosen: std::collections::BTreeMap<String, Vec<String>>,
    forecast_choice: Option<String>,
) -> Result<Vec<String>, String> {
    use gloo_net::http::Request;
    let cur = Request::get("/api/config")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let mut cfg: serde_json::Value = cur.json().await.map_err(|e| e.to_string())?;
    if let Some(obj) = cfg.as_object_mut() {
        obj.insert(
            "field_source_chains".into(),
            serde_json::to_value(&chosen).unwrap_or(serde_json::json!({})),
        );
        // The chain editor is the single home for per-field ownership now, so
        // clear the legacy single-pin map (its saved pins were already folded
        // into `chosen` as one-element chains on load).
        obj.insert("field_source_overrides".into(), serde_json::json!({}));
        // None -> JSON null so the Option<String> deserializes back to None
        // (Auto by priority); Some(id) pins that provider.
        obj.insert(
            "forecast_provider".into(),
            match &forecast_choice {
                Some(id) => serde_json::Value::String(id.clone()),
                None => serde_json::Value::Null,
            },
        );
    }
    let resp = Request::put("/api/config")
        .json(&cfg)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("HTTP {}: {body}", resp.status()));
    }
    // restart_required + restart_reasons: a tunable change (the common case
    // here) hot-reloads and reports restart_required=false; only a change a
    // boot must wire flags it. Best-effort parse: a missing/old field reads as
    // "no restart", which is the safe default.
    let reasons = resp
        .json::<serde_json::Value>()
        .await
        .ok()
        .filter(|v| {
            v.get("restart_required")
                .and_then(|r| r.as_bool())
                .unwrap_or(false)
        })
        .and_then(|v| {
            v.get("restart_reasons")
                .and_then(|r| r.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect()
                })
        })
        .unwrap_or_default();
    Ok(reasons)
}
