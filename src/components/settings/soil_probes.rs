use crate::config::{SoilAggregation, SoilProbeConfig};
use leptos::prelude::*;

#[component]
pub fn SoilProbeEditor(
    probes: RwSignal<Vec<SoilProbeConfig>>,
    aggregation: RwSignal<SoilAggregation>,
    options: RwSignal<Vec<(String, String, Option<f64>, String)>>,
) -> impl IntoView {
    let typed_id = RwSignal::new(String::new());
    let add = move |id: String| {
        let id = id.trim();
        if !id.is_empty() {
            probes.update(|rows| {
                if !rows.iter().any(|p| p.sensor_id == id) {
                    rows.push(SoilProbeConfig::new(id));
                }
            });
        }
    };
    view! {
        <div class="soil-probe-editor">
            <p class="settings-help">"Combine fresh, included probes. If none are usable, watering is held. Remove every binding only if you want to use the water balance without probes."</p>
            <label>"Combine readings"
                <select class="ui-input" aria-label="Combine soil readings"
                    prop:value=move || match aggregation.get() { SoilAggregation::Median => "median", SoilAggregation::Average => "average", SoilAggregation::Minimum => "minimum" }
                    on:change=move |ev| aggregation.set(match event_target_value(&ev).as_str() {
                        "average" => SoilAggregation::Average, "minimum" => SoilAggregation::Minimum, _ => SoilAggregation::Median,
                    })>
                    <option value="median">"Median"</option>
                    <option value="average">"Average"</option>
                    <option value="minimum">"Lowest"</option>
                </select>
            </label>
            <p class="settings-help">"Median uses the middle reading. Average includes every reading. Lowest lets the driest probe drive the zone."</p>
            <For each=move || { probes.get().into_iter().map(|p| p.sensor_id).collect::<Vec<_>>() } key=|id| id.clone()
                children=move |id| {
                    let excluded_id = id.clone();
                    let check_id = id.clone();
                    let timeout_id = id.clone();
                    let timeout_read_id = id.clone();
                    let remove_id = id.clone();
                    let label_id = id.clone();
                    view! {
                        <fieldset class="soil-probe-editor__row">
                            <legend>{move || options.get().into_iter().find(|(key, ..)| *key == label_id)
                                .map(|(_, label, _, _)| label).unwrap_or_else(|| label_id.strip_prefix("source:").and_then(|s| s.split_once(':'))
                                    .and_then(|(_, key)| crate::config::soil::probe_label(key)).unwrap_or_else(|| label_id.clone()))}</legend>
                            <label class="soil-probe-editor__exclude">
                                <input type="checkbox" aria-label=format!("Exclude {id} from decisions")
                                    prop:checked=move || probes.get().iter().any(|p| p.sensor_id == check_id && p.excluded_from_decisions)
                                    on:change=move |ev| { let checked = event_target_checked(&ev); probes.update(|rows| {
                                        if let Some(p) = rows.iter_mut().find(|p| p.sensor_id == excluded_id) { p.excluded_from_decisions = checked; }
                                    }); }/>
                                "Exclude from decisions"
                            </label>
                            <label>"Stale after (seconds)"
                                <input type="number" class="ui-input" min="1" max="604800" step="1"
                                    aria-label=format!("Stale timeout for {id}")
                                    prop:value=move || probes.get().iter().find(|p| p.sensor_id == timeout_read_id).map(|p| p.stale_after_s).unwrap_or(86400)
                                    on:input=move |ev| { if let Ok(value) = event_target_value(&ev).parse::<u64>() {
                                        probes.update(|rows| { if let Some(p) = rows.iter_mut().find(|p| p.sensor_id == timeout_id) { p.stale_after_s = value; } });
                                    } }/>
                            </label>
                            <crate::components::ui::Button variant="secondary" size="sm" on_click=Callback::new(move |_| probes.update(|rows| rows.retain(|p| p.sensor_id != remove_id)))>"Remove binding"</crate::components::ui::Button>
                        </fieldset>
                    }
                }/>
            <label>"Add soil probe"
                <select class="ui-input" aria-label="Add soil probe" prop:value=""
                    on:change=move |ev| add(event_target_value(&ev))>
                    <option value="">"Select a probe"</option>
                    {move || options.get().into_iter().filter(|(id, ..)| !probes.get().iter().any(|p| p.sensor_id == *id))
                        .map(|(id, label, _, _)| view! { <option value=id>{label}</option> }).collect_view()}
                </select>
            </label>
            <label>"Probe ID"
                <input type="text" class="ui-input" placeholder="ha:sensor.soil_moisture" prop:value=move || typed_id.get()
                    on:input=move |ev| typed_id.set(event_target_value(&ev))/>
            </label>
            <crate::components::ui::Button variant="secondary" size="sm" on_click=Callback::new(move |_| { add(typed_id.get()); typed_id.set(String::new()); })>"Add probe ID"</crate::components::ui::Button>
        </div>
    }
}
