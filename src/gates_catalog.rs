// Built-in skip-gate catalog: id, label, what disabling means, and
// whether the gate is protected (operator-control and safety gates can
// never be disabled). Plain data with no ssr-only dependencies so both
// the engine (ssr) and the Rule Lab UI (wasm) compile it; the engine's
// catalog_covers_every_traced_gate test pins it to the traced ladder.

/// Catalog of every built-in gate in the decision ladder, in evaluation
/// order: `(id, label, description, protected)`. The description is a
/// plain-language statement of what DISABLING the rule means, for the
/// settings UI. Protected entries cannot be disabled; the engine ignores
/// them if listed in `disabled_rules`.
pub fn builtin_rule_catalog() -> &'static [(&'static str, &'static str, &'static str, bool)] {
    &[
        (
            "override",
            "Manual override",
            "Your manual run or skip override for tomorrow always applies. This operator control cannot be disabled.",
            true,
        ),
        (
            "pause_until",
            "Vacation pause (timed)",
            "A timed vacation pause always blocks watering until it expires. This operator control cannot be disabled.",
            true,
        ),
        (
            "paused",
            "Vacation pause",
            "The vacation pause toggle always blocks watering while it is on. This operator control cannot be disabled.",
            true,
        ),
        (
            "restrictions",
            "Watering restrictions",
            "Configured legal or HOA watering restrictions are always enforced. This compliance gate cannot be disabled.",
            true,
        ),
        (
            "live_data",
            "Live weather availability",
            "Always on: when there is no station data and no forecast, the engine fails safe with a skip rather than deciding on fabricated values. This safety gate cannot be disabled.",
            true,
        ),
        (
            "rain_now",
            "Currently raining",
            "Watering can start while it is actively raining.",
            false,
        ),
        (
            "freeze_now",
            "Freeze risk now",
            "Watering can start even when the current temperature is below your freeze threshold.",
            false,
        ),
        (
            "overnight_freeze",
            "Overnight freeze",
            "Watering can run even when the next 24 hours are forecast to dip below your freeze threshold.",
            false,
        ),
        (
            "soil_frost",
            "Soil frost",
            "Watering can run even when the soil temperature probe reads below the frost threshold.",
            false,
        ),
        (
            "wind_now",
            "Wind too high now",
            "Watering can run even when the current wind exceeds your maximum, so spray may drift.",
            false,
        ),
        (
            "wind_forecast",
            "Windy day forecast",
            "Watering can run even when the day's peak forecast wind exceeds your maximum plus slack.",
            false,
        ),
        (
            "already_wet",
            "Already wet today",
            "Watering can run even after measurable rain has already fallen today.",
            false,
        ),
        (
            "observed_rain",
            "Observed recent rain",
            "Watering can run even after heavy measured rain has fallen over the recent window (today plus the configured past days). This sensor-independent backstop normally skips the morning after a soaking even when a soil probe is offline.",
            false,
        ),
        (
            "soil_saturation",
            "Soil saturation",
            "Watering can run even when soil moisture is at or above the saturation threshold (yard-wide and per zone).",
            false,
        ),
        (
            "rain_next_4h",
            "Rain within 4 hours",
            "Watering can run even when meaningful rain is forecast within the next 4 hours. Has no effect on Soil-model zones: they already count forecast rain against their soil deficit.",
            false,
        ),
        (
            "tomorrow_rain",
            "Tomorrow rain",
            "Watering can run even when confidence-weighted rain tomorrow meets your skip threshold. Has no effect on Soil-model zones: they already count forecast rain against their soil deficit.",
            false,
        ),
        (
            "rain_3day",
            "Heavy rain (3 day)",
            "Watering can run even when the weighted 3 day rain outlook crosses the heavy rain threshold. Has no effect on Soil-model zones: they already count forecast rain against their soil deficit.",
            false,
        ),
        (
            "soil_floor",
            "Dry-soil floor",
            "A zone measured below its minimum soil moisture waters even when a forecast-rain skip (within 4h, tomorrow, or 3-day) would otherwise apply. Disabling this returns to forecast-only skips.",
            false,
        ),
        (
            "heat_advisory",
            "Heat advisory",
            "Runs are never extended for hot, humid, dry stretches; planned durations stay unchanged. Has no effect on Soil-model zones: measured water use already charges hot days into their deficits.",
            false,
        ),
        (
            "dry_run",
            "Dry-run mode",
            "Dry-run mode always reports a skip so no real watering happens while it is on. This operator control cannot be disabled.",
            true,
        ),
    ]
}

/// What FAMILY a skip belongs to, from the gate id that decided it. Two
/// surfaces render this: the 7-day strip as a short tag, the Week page as
/// a row label and accent colour. They each carried their own list of
/// gate ids, and the lists had already drifted: a zone skipping because
/// its soil is saturated fell through the Week page's list into the plain
/// grey "Skipped" bucket, even though that page's own older prose
/// fallback put a saturated skip in the water family, where it belongs.
/// One list here, two renderings of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateFamily {
    /// A jurisdictional watering restriction.
    Restriction,
    Freeze,
    Wind,
    /// Rain, forecast rain, or soil that already holds water.
    Water,
    Pause,
    /// The soil model waters through a gate the weekly plan skips on.
    SoilModel,
    /// No live weather to decide on, so the engine fails safe. Not a
    /// weather condition: a data outage, which reads differently to an
    /// operator and is worth saying rather than showing a grey row.
    NoData,
    /// A skip with no id, or one no surface classifies.
    Other,
}

/// Classify a skip-check reason code. An empty code means an older
/// payload with no id, which callers handle by reading the sentence.
pub fn gate_family(reason_code: &str) -> GateFamily {
    match reason_code {
        "restrictions" => GateFamily::Restriction,
        "freeze_now" | "overnight_freeze" | "soil_frost" => GateFamily::Freeze,
        "wind_now" | "wind_forecast" => GateFamily::Wind,
        "rain_now" | "already_wet" | "observed_rain" | "rain_next_4h" | "tomorrow_rain"
        | "rain_3day" | "soil_saturation" => GateFamily::Water,
        "paused" | "pause_until" => GateFamily::Pause,
        "soil_model" => GateFamily::SoilModel,
        "live_data" => GateFamily::NoData,
        // Both decide a RUN rather than a skip: the dry-soil floor
        // overrides a forecast-rain skip, and the heat advisory extends a
        // run. They carry no skip family, and the coverage test exempts
        // them for that reason rather than by omission.
        "soil_floor" | "heat_advisory" => GateFamily::Other,
        _ => GateFamily::Other,
    }
}

#[cfg(test)]
mod family_tests {
    use super::*;

    /// Every gate the engine can decide on has a family, so no skip can
    /// land in the unclassified bucket and render as a grey "Skipped"
    /// with no explanation of what stopped the yard. The two surfaces
    /// that render families (the 7-day strip and the Week page) both read
    /// this function, so a gate added to the catalog without a family
    /// here fails the build rather than showing up blank on two screens.
    #[test]
    fn every_catalog_gate_has_a_family() {
        // Control gates decide a RUN, not a skip, so they are allowed to
        // be unclassified: no skip family applies to them.
        const DECIDES_A_RUN: [&str; 4] = ["override", "dry_run", "soil_floor", "heat_advisory"];
        for (id, label, _, _) in builtin_rule_catalog() {
            if DECIDES_A_RUN.contains(id) {
                continue;
            }
            assert_ne!(
                gate_family(id),
                GateFamily::Other,
                "gate {id} ({label}) has no family: a skip on it renders as an unexplained grey row"
            );
        }
    }

    /// Saturated soil belongs to the water family: the yard is skipping
    /// BECAUSE it holds water. The Week page's structured path used to
    /// miss this id and drop such a skip into the grey bucket, while its
    /// own prose fallback got it right.
    #[test]
    fn a_saturated_zone_reads_as_water_not_as_a_bare_skip() {
        assert_eq!(gate_family("soil_saturation"), GateFamily::Water);
        assert_eq!(gate_family("rain_3day"), GateFamily::Water);
        assert_eq!(gate_family(""), GateFamily::Other);
    }
}
