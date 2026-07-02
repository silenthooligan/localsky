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
            "Watering can run even when meaningful rain is forecast within the next 4 hours.",
            false,
        ),
        (
            "tomorrow_rain",
            "Tomorrow rain",
            "Watering can run even when confidence-weighted rain tomorrow meets your skip threshold.",
            false,
        ),
        (
            "rain_3day",
            "Heavy rain (3 day)",
            "Watering can run even when the weighted 3 day rain outlook crosses the heavy rain threshold.",
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
            "Runs are never extended for hot, humid, dry stretches; planned durations stay unchanged.",
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
