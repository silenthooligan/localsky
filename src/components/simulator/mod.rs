// Simulator — the What-If lab (marquee feature 1, first cut). Sliders for
// the weather drivers (seeded from today's live readings); on change we
// POST the hypothetical to /api/irrigation/simulate, which re-runs the
// EXACT production ladder (decide_traced) on baseline vs hypothetical and
// returns both traces. We render the verdict transition + the rules whose
// outcome changed. Faithful by construction: same engine code as the real
// morning decision.

use leptos::prelude::*;

use crate::components::ui::{Button, Slider};
use crate::ha::snapshot::{IrrigationSnapshot, SimResult};

fn verdict_token(v: &str) -> &'static str {
    match v {
        "run" => "var(--verdict-run)",
        "run_extended" => "var(--verdict-extend)",
        _ => "var(--verdict-skip)",
    }
}
fn verdict_label(v: &str) -> &'static str {
    match v {
        "run" => "WATER",
        "run_extended" => "WATER +",
        _ => "SKIP",
    }
}

#[component]
pub fn SimulatorPage(snap: ReadSignal<IrrigationSnapshot>) -> impl IntoView {
    // Slider state (absolute values). Seeded from the live SkipCheck once
    // the first snapshot arrives.
    let temp = RwSignal::new(75.0);
    let humidity = RwSignal::new(55.0);
    let wind = RwSignal::new(5.0);
    let rain_today = RwSignal::new(0.0);
    let rain_tomorrow = RwSignal::new(0.0);
    let prob_tomorrow = RwSignal::new(0.0);
    let heat_3day = RwSignal::new(85.0);
    let test_script = RwSignal::new(String::new());
    let seeded = RwSignal::new(false);

    let result: RwSignal<Option<SimResult>> = RwSignal::new(None);

    let seed_from_live = move || {
        let s = snap.get().skip_check;
        temp.set(s.temp_now_f);
        humidity.set(s.humidity_now_pct);
        wind.set(s.wind_now_mph);
        rain_today.set(s.rain_today_in);
        rain_tomorrow.set(s.forecast_in);
        prob_tomorrow.set(s.rain_tomorrow_prob_pct as f64);
        heat_3day.set(s.temp_max_3day_f);
    };

    // Seed once when the live snapshot first carries a real reading.
    #[cfg(feature = "hydrate")]
    {
        Effect::new(move |_| {
            let s = snap.get();
            if !seeded.get_untracked() && s.last_refresh_epoch > 0 {
                seed_from_live();
                seeded.set(true);
            }
        });

        // Re-run the simulation whenever a slider moves (after seeding).
        Effect::new(move |_| {
            let req = crate::ha::snapshot::SimRequest {
                temp_now_f: Some(temp.get()),
                humidity_now_pct: Some(humidity.get()),
                wind_now_mph: Some(wind.get()),
                rain_today_in: Some(rain_today.get()),
                forecast_in: Some(rain_tomorrow.get()),
                rain_tomorrow_prob_pct: Some(prob_tomorrow.get() as u32),
                temp_max_3day_f: Some(heat_3day.get()),
                test_script: {
                    let s = test_script.get();
                    if s.trim().is_empty() {
                        None
                    } else {
                        Some(s)
                    }
                },
                ..Default::default()
            };
            if !seeded.get() {
                return;
            }
            let body = serde_json::to_string(&req).unwrap_or_default();
            leptos::task::spawn_local(async move {
                if let Ok(resp) = gloo_net::http::Request::post("/api/irrigation/simulate")
                    .header("Content-Type", "application/json")
                    .body(body)
                    .ok()
                    .unwrap()
                    .send()
                    .await
                {
                    if let Ok(r) = resp.json::<SimResult>().await {
                        result.set(Some(r));
                    }
                }
            });
        });
    }
    #[cfg(not(feature = "hydrate"))]
    let _ = (seed_from_live, seeded, result);

    let reset = move |_: leptos::ev::MouseEvent| seed_from_live();

    view! {
        <div class="sim-page">
            <header class="sim-page__header">
                <p class="sim-page__eyebrow">"Analyze"</p>
                <h1 class="sim-page__title">"Simulator"</h1>
                <p class="sim-page__sub">"Move a slider, see how today\u{2019}s decision would change. Same engine as the real morning run."</p>
            </header>

            <div class="sim-layout">
                <section class="sim-inputs">
                    <SimSlider label="Temp now" suffix="°F" value=temp min=20.0 max=115.0 step=1.0/>
                    <SimSlider label="Humidity" suffix="%" value=humidity min=0.0 max=100.0 step=1.0/>
                    <SimSlider label="Wind now" suffix=" mph" value=wind min=0.0 max=45.0 step=1.0/>
                    <SimSlider label="Rain today" suffix="\"" value=rain_today min=0.0 max=2.0 step=0.05 precision=2/>
                    <SimSlider label="Rain tomorrow" suffix="\"" value=rain_tomorrow min=0.0 max=2.0 step=0.05 precision=2/>
                    <SimSlider label="Tomorrow chance" suffix="%" value=prob_tomorrow min=0.0 max=100.0 step=5.0/>
                    <SimSlider label="3-day high" suffix="°F" value=heat_3day min=40.0 max=115.0 step=1.0/>

                    <div class="sim-script">
                        <label class="sim-slider__label">"Test a custom rule (Rhai)"</label>
                        <textarea
                            class="sim-script__input"
                            spellcheck="false"
                            placeholder="e.g. wind_now_mph > 12.0"
                            prop:value=move || test_script.get()
                            on:input=move |ev| test_script.set(event_target_value(&ev))
                        ></textarea>
                        <p class="sim-script__hint">
                            "Return true (or a reason string) to skip. Augment-only: a rule can add a skip, never clear a safety gate. Add it to "
                            <code>"[scripting]"</code>" once it behaves."
                        </p>
                    </div>

                    <div class="sim-inputs__reset">
                        <Button variant="ghost" icon="refresh" on_click=Callback::new(reset)>"Reset to live"</Button>
                    </div>
                </section>

                <section class="sim-result">
                    {move || match result.get() {
                        None => view! { <div class="sim-result__empty">"Adjust a slider to simulate."</div> }.into_any(),
                        Some(r) => view! { <SimVerdict r/> }.into_any(),
                    }}
                </section>
            </div>
        </div>
    }
}

#[component]
fn SimSlider(
    #[prop(into)] label: String,
    #[prop(into)] suffix: String,
    value: RwSignal<f64>,
    min: f64,
    max: f64,
    step: f64,
    #[prop(default = 0)] precision: usize,
) -> impl IntoView {
    view! {
        <div class="sim-slider">
            <label class="sim-slider__label">{label}</label>
            <Slider value min max step suffix precision/>
        </div>
    }
}

#[component]
fn SimVerdict(r: SimResult) -> impl IntoView {
    let bv = r.baseline.verdict.clone();
    let hv = r.hypothetical.verdict.clone();
    let changed = bv != hv;
    let btok = verdict_token(&bv);
    let htok = verdict_token(&hv);
    let blab = verdict_label(&bv);
    let hlab = verdict_label(&hv);
    let hreason = if r.hypothetical.reason.is_empty() {
        "All clear — no skip rule fired.".to_string()
    } else {
        r.hypothetical.reason.clone()
    };

    // Rules whose outcome differs between baseline and hypothetical.
    let base_rules = r.baseline.rules.clone();
    let diffs: Vec<(String, String, String)> = r
        .hypothetical
        .rules
        .iter()
        .filter_map(|h| {
            base_rules
                .iter()
                .find(|b| b.id == h.id)
                .filter(|b| b.outcome != h.outcome)
                .map(|b| (h.label.clone(), b.outcome.clone(), h.outcome.clone()))
        })
        .collect();

    view! {
        <div class="sim-verdict">
            <div class="sim-verdict__transition">
                <span class="sim-verdict__pill" style=format!("--v:{btok}")>{blab}</span>
                <span class="sim-verdict__arrow" class:is-changed=changed>"→"</span>
                <span class="sim-verdict__pill" style=format!("--v:{htok}")>{hlab}</span>
            </div>
            <p class="sim-verdict__reason">{hreason}</p>
            {(!diffs.is_empty()).then(|| view! {
                <div class="sim-diff">
                    <h3 class="sim-diff__title">"What changed"</h3>
                    <ul class="sim-diff__list">
                        {diffs.into_iter().map(|(label, from, to)| view! {
                            <li class="sim-diff__row">
                                <span class="sim-diff__label">{label}</span>
                                <span class="sim-diff__change">{from}" → "{to}</span>
                            </li>
                        }).collect_view()}
                    </ul>
                </div>
            })}
            {move || (changed).then(|| view! {
                <a class="sim-verdict__link" href="/rules">"See the full ladder in Rule Lab →"</a>
            })}
        </div>
    }
}
