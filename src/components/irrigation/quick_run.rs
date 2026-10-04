//! The same one-time run picker on Irrigation and Zones. Page-owned state
//! survives snapshot updates; the server owns every watering timer.
use crate::{
    components::ui::{Button, Icon, Sheet},
    model::{quick_run::*, IrrigationSnapshot},
};
use leptos::prelude::*;

#[derive(Clone, PartialEq, Eq)]
struct Pick {
    zone: String,
    name: String,
    minutes: u32,
    max_minutes: u32,
    selected: bool,
}

fn total(picks: &[Pick]) -> (usize, u32) {
    let selected: Vec<_> = picks.iter().filter(|p| p.selected).collect();
    (selected.len(), selected.iter().map(|p| p.minutes).sum())
}

#[component]
pub fn QuickRun(snap: ReadSignal<IrrigationSnapshot>) -> impl IntoView {
    let open = RwSignal::new(false);
    let client = expect_context::<super::quick_run_client::QuickRunClient>();
    let state = client.state;
    let picks = RwSignal::new(Vec::<Pick>::new());
    let pending = client.pending;
    let error = client.error;
    let offline = client.offline;
    let revision = client.revision;
    let request_id = RwSignal::new(String::new());
    let editing = RwSignal::new(true);
    let clock = client.clock;
    let active = Memo::new(move |_| {
        state
            .get()
            .and_then(|s| s.run)
            .is_some_and(|r| r.phase.active() || r.stop_unconfirmed)
    });
    let stop_unconfirmed = Memo::new(move |_| {
        state
            .get()
            .and_then(|s| s.run)
            .is_some_and(|r| r.stop_unconfirmed)
    });

    Effect::new(move |_| client.watching.set(open.get()));
    on_cleanup(move || client.watching.set(false));

    let reset = Callback::new(move |()| {
        picks.set(
            state
                .get_untracked()
                .map(|s| {
                    s.zones
                        .into_iter()
                        .map(|z| Pick {
                            zone: z.zone,
                            name: z.name,
                            minutes: 5.min(z.max_seconds / 60),
                            max_minutes: z.max_seconds / 60,
                            selected: false,
                        })
                        .collect()
                })
                .unwrap_or_default(),
        );
        request_id.set(String::new());
        error.set(String::new());
        editing.set(true);
    });
    let show = Callback::new(move |_| {
        if active.get_untracked()
            || state.get_untracked().and_then(|s| s.run).is_some_and(|r| {
                matches!(r.phase, QuickRunPhase::Failed | QuickRunPhase::Interrupted)
            })
        {
            editing.set(false);
        } else {
            reset.run(());
        }
        open.set(true);
    });
    let mutate = Callback::new(move |stop: bool| {
        if pending.get_untracked() {
            return;
        }
        let run = state.get_untracked().and_then(|s| s.run);
        if stop
            && run
                .as_ref()
                .is_none_or(|r| !r.phase.active() && !r.stop_unconfirmed)
        {
            return;
        }
        #[cfg(feature = "hydrate")]
        if !stop && request_id.get_untracked().is_empty() {
            request_id.set(format!(
                "quick-{}-{}",
                js_sys::Date::now(),
                js_sys::Math::random()
            ));
        }
        let body = if stop {
            serde_json::json!({"id": run.unwrap().id})
        } else {
            let zones: Vec<_> = picks
                .get_untracked()
                .into_iter()
                .filter(|p| p.selected)
                .map(|p| QuickRunChoice {
                    zone: p.zone,
                    seconds: p.minutes * 60,
                })
                .collect();
            serde_json::json!(QuickRunRequest {
                request_id: request_id.get_untracked(),
                zones
            })
        };
        pending.set(true);
        revision.update(|v| *v += 1);
        error.set(String::new());
        send(stop, body, client.receive);
    });
    Effect::new(move |_| {
        if state
            .get()
            .and_then(|s| s.run)
            .is_some_and(|r| r.request_id == request_id.get_untracked() && !r.request_id.is_empty())
        {
            editing.set(false);
        }
    });
    // A changed selection is a new request; retrying an unchanged selection
    // keeps its ID so an uncertain network response cannot duplicate watering.
    Effect::new(move |_| {
        picks.track();
        request_id.set(String::new());
    });
    let invalid = Signal::derive(move || {
        let selected = picks.get();
        let (count, minutes) = total(&selected);
        count == 0
            || minutes > 360
            || selected
                .iter()
                .any(|p| p.selected && (p.minutes == 0 || p.minutes > p.max_minutes))
            || offline.get()
            || active.get()
    });

    view! {
        <Show when=move || !snap.get().zones.is_empty()>
            <section class="quick-run-entry" class:quick-run-entry--active=move || active.get() class:quick-run-entry--attention=move || stop_unconfirmed.get() aria-label="Quick Run">
                <span class="quick-run-entry__icon" aria-hidden="true"><Icon name="play" size=22/></span>
                <div class="quick-run-entry__copy">
                    <h2>{move || if stop_unconfirmed.get() { "Check Quick Run" } else if active.get() { "Quick Run in progress" } else { "A little extra water?" }}</h2>
                    <p>{move || if stop_unconfirmed.get() { "Stop wasn’t confirmed. Open Quick Run to retry." } else if active.get() { "See what’s running or stop the remaining zones." } else { "Run any zone, a few, or the whole yard." }}</p>
                </div>
                <Button icon="play" on_click=show disabled=Signal::derive(move || state.get().is_none() && !offline.get())>
                    {move || if active.get() { "View Quick Run" } else { "Quick Run" }}
                </Button>
            </section>
        </Show>
        <Sheet open title="Quick Run" id="quick-run-dialog">
            <div class="quick-run">
                <Show when=move || offline.get()>
                    <p class="quick-run__error" role="status">"Could not refresh Quick Run. Check your connection; any active run continues on the server."</p>
                </Show>
                <Show when=move || !error.get().is_empty()>
                    <p class="quick-run__error" role="alert">{move || error.get()}</p>
                </Show>
                {move || state.get().filter(|s| !s.available).map(|s| view! {
                    <p class="quick-run__note">{s.reason.unwrap_or_else(|| "Quick Run is unavailable.".into())}</p>
                    <a class="btn btn--secondary" href=crate::base::url("/settings?section=devices")>"Controller settings"</a>
                })}
                <Show when=move || state.get().is_some_and(|s| s.available) && editing.get() && !active.get()>
                    <p class="quick-run__intro">"Choose zones and run times. Your schedule stays the same."</p>
                    <fieldset class="quick-run__presets" disabled=move || pending.get()>
                        <legend>"Minutes per zone"</legend>
                        <div>{[1u32, 5, 10, 15].into_iter().map(|minutes| view! {
                            <button type="button" class="quick-run__preset"
                                aria-pressed=move || picks.get().iter().all(|p| p.minutes == minutes.min(p.max_minutes)).to_string()
                                on:click=move |_| picks.update(|rows| for row in rows { row.minutes = minutes.min(row.max_minutes); })>
                                {minutes}<span>" min"</span>
                            </button>
                        }).collect_view()}</div>
                    </fieldset>
                    <div class="quick-run__list-heading">
                        <h3>"Zones"</h3>
                        <button type="button" class="btn btn--ghost btn--sm" disabled=move || pending.get()
                            on:click=move |_| picks.update(|rows| {
                                let select = !rows.iter().filter(|r| r.max_minutes > 0).all(|r| r.selected);
                                for row in rows { row.selected = select && row.max_minutes > 0; }
                            })>{move || if !picks.get().is_empty() && picks.get().iter().filter(|r| r.max_minutes > 0).all(|r| r.selected) { "Clear selection" } else { "Select all" }}</button>
                    </div>
                    <div class="quick-run__zones">
                        <For each={move || picks.get().into_iter().map(|p| p.zone).collect::<Vec<_>>()} key=|zone| zone.clone()
                            children={move |zone| view! { <QuickRunRow zone picks pending/> }}/>
                    </div>
                    <p class="quick-run__note">"Runs one zone at a time, even during a rain delay or weather skip. Watering limits still apply."</p>
                    <div class="quick-run__footer">
                        <div><strong>{move || { let (_, minutes) = total(&picks.get()); format!("{minutes} min total") }}</strong>
                            <span>{move || { let (count, _) = total(&picks.get()); format!("{count} {} selected", if count == 1 { "zone" } else { "zones" }) }}</span>
                        </div>
                        <Button icon="play" loading=Signal::derive(move || pending.get()) disabled=invalid on_click=Callback::new(move |_| mutate.run(false))>"Start Quick Run"</Button>
                    </div>
                    <Show when={move || total(&picks.get()).1 > 360}><p class="quick-run__error">"Choose no more than 6 hours total."</p></Show>
                </Show>
                <Show when=move || !editing.get() || active.get()>
                    {move || state.get().and_then(|s| s.run).map(|run| {
                        let phase = run.phase;
                        let current = run.current;
                        let completed = run.completed;
                        let ends = run.current_ends_epoch;
                        view! {
                            <div class="quick-run__progress" class:quick-run__progress--attention=matches!(phase, QuickRunPhase::Failed | QuickRunPhase::Interrupted)>
                                <p class="quick-run__eyebrow">{format!("{} of {} zones finished", completed, run.zones.len())}</p>
                                <h3 aria-live="polite">{phase.label()}</h3>
                                <p>{run.message}</p>
                                <Show when=move || phase == QuickRunPhase::Running && ends.is_some()>
                                    <p class="quick-run__remaining">{move || {
                                        let remaining = (ends.unwrap_or_default() - clock.get()).max(0);
                                        if remaining == 0 { "Finishing this zone…".into() } else { format!("About {} min left in this zone", (remaining + 59) / 60) }
                                    }}</p>
                                </Show>
                            </div>
                            <ol class="quick-run__queue">{run.zones.into_iter().zip(run.names).enumerate().map(|(index, (zone, name))| {
                                let label = if index < completed { "Finished" } else if Some(index) == current && phase.active() { "Current" } else if phase.active() { "Next" } else { "Not completed" };
                                view! { <li class:quick-run__queue-current=Some(index) == current && phase.active()>
                                    <span class="quick-run__order">{index + 1}</span><span><strong>{name}</strong><small>{label}</small></span><span class="quick-run__duration">{format!("{} min", zone.seconds.div_ceil(60))}</span>
                                </li> }
                            }).collect_view()}</ol>
                        }
                    })}
                    <div class="quick-run__actions">
                        <Show when=move || active.get() fallback=move || view! {
                            <Button icon="play" on_click=Callback::new(move |_| reset.run(()))>"New Quick Run"</Button>
                            <a class="btn btn--secondary" href=crate::base::url("/history?view=runs")>"View History"</a>
                        }>
                            <Button variant="danger" icon="stop" loading=Signal::derive(move || pending.get()) on_click=Callback::new(move |_| mutate.run(true))>
                                {move || if state.get().and_then(|s| s.run).is_some_and(|r| r.stop_unconfirmed) { "Stop all watering" } else { "Stop Quick Run" }}
                            </Button>
                        </Show>
                        <Button variant="ghost" on_click=Callback::new(move |_| open.set(false))>"Close"</Button>
                    </div>
                </Show>
            </div>
        </Sheet>
    }
}

#[component]
fn QuickRunRow(zone: String, picks: RwSignal<Vec<Pick>>, pending: RwSignal<bool>) -> impl IntoView {
    let zone = StoredValue::new(zone);
    let row = Memo::new(move |_| picks.get().into_iter().find(|p| p.zone == zone.get_value()));
    let name = row.get_untracked().map(|p| p.name).unwrap_or_default();
    let duration_label = format!("Minutes for {name}");
    let id = format!("quick-zone-{}", zone.get_value());
    let limit_id = format!("{id}-limit");
    let invalid_time = Memo::new(move |_| {
        row.get()
            .is_some_and(|r| r.minutes == 0 || r.minutes > r.max_minutes)
    });
    let change = Callback::new(move |minutes: u32| {
        picks.update(|rows| {
            if let Some(row) = rows.iter_mut().find(|p| p.zone == zone.get_value()) {
                row.minutes = minutes;
            }
        })
    });
    view! {
        <div class="quick-run__zone" class:quick-run__zone--selected=move || row.get().is_some_and(|r| r.selected)>
            <label for=id.clone()>
                <input type="checkbox" id=id.clone() prop:checked=move || row.get().is_some_and(|r| r.selected)
                    disabled=move || pending.get() || row.get().is_none_or(|r| r.max_minutes == 0)
                    on:change=move |ev| picks.update(|rows| { if let Some(row) = rows.iter_mut().find(|p| p.zone == zone.get_value()) { row.selected = event_target_checked(&ev); } })/>
                <span>{name}</span>
            </label>
            <div class="quick-run__time" role="group" aria-label=duration_label.clone()>
                <button type="button" aria-label=format!("Decrease {duration_label}") disabled=move || pending.get() || row.get().is_none_or(|r| r.minutes <= 1)
                    on:click=move |_| { if let Some(row) = row.get_untracked() { change.run(row.minutes.saturating_sub(1).max(1)); } }><Icon name="minus" size=16/></button>
                <label><input type="number" min="1" max=move || row.get().map(|r| r.max_minutes) inputmode="numeric" aria-label=duration_label.clone()
                    aria-invalid=move || invalid_time.get().to_string() aria-describedby=limit_id.clone()
                    prop:value=move || row.get().map(|r| r.minutes.to_string()).unwrap_or_default() disabled=move || pending.get()
                    on:input=move |ev| change.run(event_target_value(&ev).parse::<u32>().unwrap_or(0).min(121))/><span>"min"</span></label>
                <button type="button" aria-label=move || format!("Increase minutes for {}", row.get().map(|r| r.name).unwrap_or_default()) disabled=move || pending.get() || row.get().is_none_or(|r| r.minutes >= r.max_minutes)
                    on:click=move |_| { if let Some(row) = row.get_untracked() { change.run((row.minutes + 1).min(row.max_minutes)); } }><Icon name="plus" size=16/></button>
            </div>
            <p id=limit_id.clone() class="quick-run__time-error" hidden=move || !invalid_time.get()>
                {move || row.get().map(|r| format!("Choose 1 to {} minutes for this zone.", r.max_minutes))}
            </p>
        </div>
    }
}

#[cfg(feature = "hydrate")]
pub(super) fn send(
    stop: bool,
    body: serde_json::Value,
    done: Callback<Result<QuickRunStatus, String>>,
) {
    leptos::task::spawn_local(async move {
        let result = async {
            let path = if stop { "/api/v1/irrigation/quick-run/stop" } else { "/api/v1/irrigation/quick-run" };
            let request = gloo_net::http::Request::post(&crate::base::url(path)).json(&body).map_err(|_| "Could not prepare Quick Run.".to_string())?;
            let response = request.send().await.map_err(|_| "Connection lost. Check Quick Run status before retrying; the run may have started.".to_string())?;
            let status = response.status();
            let text = response.text().await.map_err(|_| "Could not read Quick Run status.".to_string())?;
            if !response.ok() { return Err(crate::components::request_error::RequestError::response(status, &text).message); }
            serde_json::from_str(&text).map_err(|_| "Could not read Quick Run status.".to_string())
        }.await;
        let _ = done.try_run(result);
    });
}
#[cfg(not(feature = "hydrate"))]
pub(super) fn send(
    _stop: bool,
    _body: serde_json::Value,
    _done: Callback<Result<QuickRunStatus, String>>,
) {
}
