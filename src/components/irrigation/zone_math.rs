// "Why this duration?", per-zone math-transparency tile. Surfaces the
// chain of inputs that produced the seconds SI shipped into IU at
// 23:30, so the operator can see *why* the run is the length it is
// (and whether the maximum_duration cap is shorting it).
//
// All values come from SI's per-zone attributes via the refresher (see
// ZoneState.math). No new math here, the formula is restated as labels
// alongside the live values, matching SI's internal compute.

use crate::components::ui::HelpHint;
use crate::ha::snapshot::{IrrigationSnapshot, ZoneState};
use leptos::prelude::*;
use leptos::tachys::view::any_view::IntoAny;

#[component]
pub fn ZoneMathPanel(snap: ReadSignal<IrrigationSnapshot>) -> impl IntoView {
    let zone0 = Signal::derive(move || snap.get().zones.first().cloned().unwrap_or_default());
    let zone1 = Signal::derive(move || snap.get().zones.get(1).cloned().unwrap_or_default());
    let zone2 = Signal::derive(move || snap.get().zones.get(2).cloned().unwrap_or_default());
    let zone3 = Signal::derive(move || snap.get().zones.get(3).cloned().unwrap_or_default());

    view! {
        <section class="zone-math">
            <h2 class="zone-math-title">
                "Why this duration?"
                <HelpHint topic="zone-math"/>
            </h2>
            <p class="zone-math-sub">
                "The engine's water-balance math per zone. Bucket is the soil-water "
                "deficit in mm; throughput is the head\u{2019}s precipitation rate "
                "(catalog default from the zone\u{2019}s sprinkler type, or your measured "
                "value). raw = ( |bucket| / throughput ) \u{00d7} 3600 \u{00d7} Kc, then "
                "capped at the safety ceiling. If the cap row turns amber, the zone is "
                "being shorted by the ceiling and needs a longer ceiling or a faster head."
            </p>
            <div class="zone-math-grid">
                {view! { <ZoneMathTile zone=zone0/> }.into_any()}
                {view! { <ZoneMathTile zone=zone1/> }.into_any()}
                {view! { <ZoneMathTile zone=zone2/> }.into_any()}
                {view! { <ZoneMathTile zone=zone3/> }.into_any()}
            </div>
        </section>
    }
}

#[component]
fn ZoneMathTile(zone: Signal<ZoneState>) -> impl IntoView {
    // Bail early if SI hasn't populated yet. Renders a faint placeholder
    // so the slot stays the same size between first-paint and live data.
    let has_math = move || zone.get().math.is_some();
    let tile_class = move || {
        let z = zone.get();
        let cap = z.math.as_ref().map(|m| m.cap_binding).unwrap_or(false);
        if cap {
            "zone-math-tile zone-math-tile-capped"
        } else {
            "zone-math-tile"
        }
    };

    view! {
        <article class=tile_class>
            <header class="zone-math-head">
                <h3 class="zone-math-name">{move || zone.get().name}</h3>
                <span class="zone-math-final">
                    {move || format_minutes(zone.get().math.as_ref().map(|m| m.scheduled_seconds).unwrap_or(0))}
                </span>
            </header>
            <Show when=has_math fallback=|| view! { <p class="zone-math-empty">"SI hasn\u{2019}t computed yet (waiting for next 23:00 calc)."</p> }>
                <MathRows zone=zone/>
            </Show>
        </article>
    }
}

#[component]
fn MathRows(zone: Signal<ZoneState>) -> impl IntoView {
    let m = Signal::derive(move || zone.get().math.unwrap_or_default());

    let fmt_bucket = move || {
        let v = m.get().bucket_mm;
        if v < 0.0 {
            format!("{:.2} mm (deficit, needs water)", v)
        } else if v > 0.0 {
            format!("+{:.2} mm (surplus)", v)
        } else {
            "0.00 mm (at field capacity)".to_string()
        }
    };
    let fmt_kc = move || {
        let m = m.get();
        let kind = if m.kc >= 1.0 { "turf" } else { "shrubs / drip" };
        format!("\u{00d7} {:.2} ({})", m.kc, kind)
    };
    let fmt_heat = move || {
        let v = m.get().heat_mult;
        let kind = if v >= 1.25 {
            "heat advisory"
        } else if v >= 1.10 {
            "warm"
        } else {
            "no boost"
        };
        format!("\u{00d7} {:.2} ({})", v, kind)
    };
    let fmt_thr = move || {
        let v = m.get().throughput_mm_hr;
        let kind = if v <= 0.0 {
            "unset"
        } else if v < 4.0 {
            "drip / low-precip rotor"
        } else if v < 10.0 {
            "rotor"
        } else if v < 20.0 {
            "R-VAN / MP"
        } else {
            "fixed spray"
        };
        format!("\u{00f7} {:.2} mm/hr ({})", v, kind)
    };
    let fmt_capture = move || format!("\u{00f7} {:.2} (capture eff.)", m.get().capture_eff);
    let fmt_raw = move || format!("= {}", format_seconds_pretty(m.get().raw_seconds));
    let fmt_cap = move || {
        let m = m.get();
        if m.cap_binding {
            format!(
                "capped at {} ({}% short)",
                format_seconds_pretty(m.max_duration_seconds),
                ((m.raw_seconds - m.max_duration_seconds) as f64 / m.raw_seconds as f64 * 100.0)
                    .round() as i64
            )
        } else {
            format!(
                "under cap ({} ceiling)",
                format_seconds_pretty(m.max_duration_seconds)
            )
        }
    };
    let fmt_final = move || {
        format!(
            "scheduled tonight: {}",
            format_seconds_pretty(m.get().scheduled_seconds)
        )
    };

    view! {
        <dl class="zone-math-rows">
            <div class="zone-math-row"><dt>"bucket deficit"</dt><dd>{fmt_bucket}</dd></div>
            <div class="zone-math-row"><dt>"crop coefficient"</dt><dd>{fmt_kc}</dd></div>
            <div class="zone-math-row"><dt>"heat multiplier"</dt><dd>{fmt_heat}</dd></div>
            <div class="zone-math-row"><dt>"throughput"</dt><dd>{fmt_thr}</dd></div>
            <div class="zone-math-row"><dt>"capture efficiency"</dt><dd>{fmt_capture}</dd></div>
            <div class="zone-math-row zone-math-row-raw"><dt>"raw need"</dt><dd>{fmt_raw}</dd></div>
            <div class="zone-math-row zone-math-row-cap"><dt>"safety ceiling"</dt><dd>{fmt_cap}</dd></div>
            <div class="zone-math-row zone-math-row-final"><dt>"final"</dt><dd>{fmt_final}</dd></div>
        </dl>
    }
}

fn format_seconds_pretty(s: u32) -> String {
    let m = s / 60;
    let r = s % 60;
    if m == 0 {
        format!("{r}s")
    } else if r == 0 {
        format!("{m}min")
    } else {
        format!("{m}min {r}s")
    }
}

fn format_minutes(s: u32) -> String {
    let m = (s as f64 / 60.0).round() as u32;
    format!("{m} min")
}
