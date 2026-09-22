# Skip Rules

LocalSky checks each zone through the same decision ladder every morning and whenever conditions change. **First matching applicable gate wins.** Active owner holds, watering restrictions, unavailable weather or configured probes, and enabled freeze, frost, and wind protections remain binding when Force is selected. Rain, reliable soil readings, and structured condition recommendations can be bypassed by an explicitly confirmed Force choice. Script holds remain binding.

The catalog contains {{LOCALSKY_SKIP_RULES}} built-in rules. Source: `src/engine/skip_rules.rs`.

## Ladder

| # | Rule | Trigger | Threshold | Tunable? |
|---|------|---------|-----------|---------:|
| 0 | Restart required | saved configuration changed a startup-only dependency | until process restart | none |
| 1 | Manual override: skip tomorrow | `is_tomorrow && override_tomorrow == "skip"` | none | UI |
| 2 | Manual override: run tomorrow | `is_tomorrow && override_tomorrow == "run"`, after safety and hold checks | none | UI |
| 3 | Vacation pause (timed) | `pause_until_epoch > now_epoch` | none | UI |
| 4 | Vacation pause (toggle) | `is_paused == true` | none | UI |
| 4b | Watering restrictions | a configured restriction blocks this day, date, or hour | your restriction rules | UI |
| 4c | Live weather unavailable | `live_readings == Unavailable` (no station data, no forecast) | none | none |
| 4d | Configured soil probe unavailable | this zone's bound probe is missing or untrusted | a reliable reading must return | none |
| 4e | Watering-plan rain unavailable | this zone's automatic plan lacks rain evidence for the next 24 hours | complete interval coverage | none |
| 5 | Currently raining | `rain_intensity_now_in_hr > 0.01` | 0.01 in/hr (0.25 mm/hr) | `rain_now_in_hr` |
| 6 | Freeze risk now | `temp_now_f < min_temp_f` | 38°F (3.3°C) | `min_temp_f` |
| 7 | Overnight freeze | `temp_min_24h_f < min_temp_f` | 38°F (3.3°C) | `min_temp_f` |
| 8 | Soil frost | `soil_temp_yard_min_f < frost_skip_soil_f` | 35°F (1.7°C) | `frost_skip_soil_f` |
| 9 | Wind too high now | `wind_now_mph > max_wind_mph` | 10 mph (16 km/h) | `max_wind_mph` |
| 10 | Windy day forecast | `wind_max_today_mph > max_wind_mph + 5` | +5 mph (8 km/h) slack | `wind_forecast_slack_mph` |
| 11 | Already wet | `rain_today_in >= 0.05` | 0.05 in (1.3 mm) | `already_wet_in` |
| 11a | Rain forecast today | expected rain meets the wet-day threshold | 0.05 in (1.3 mm), forecast rather than measured | `already_wet_in` |
| 11b | Observed rain recently | `rain_observed_recent_in >= rain_skip_in` | 0.25 in (6.4 mm) over the recent window | `rain_skip_in`, `rain_observed_window_days` |
| 12 | Zone soil-saturated | this zone's effective moisture % >= saturation threshold | per-zone | per-zone soil settings |
| 13 | Rain in next 4 hours | `rain_next_4h_in >= 0.10` | 0.10 in (2.5 mm) | `rain_next_4h_skip_in` |
| 14 | Tomorrow rain (confidence-weighted) | `forecast_in * prob/100 >= rain_skip_in` | 0.25 in (6.4 mm), weighted | `rain_skip_in` |
| 15 | 3-day rain rollup | `rain_3day_weighted_in >= 1.5 * rain_skip_in` | 1.5x multiplier | `rain_3day_factor` |
| 15b | Measured dry-soil exception | a soft forecast-rain skip meets a zone measured below its dry floor | per-zone `target_min_pct_soil` | per-zone soil settings |
| 16 | Heat advisory (pre-water) | 3-day max >= 95°F (35°C) + humidity >= 60% + 2+ dry days | composite | `heat_advisory_*` |
| 17 | Dry-run mode | `is_dry_run == true` | none | UI |
| - | Default | (no rule matched) | none | run |

Rules 4b, 4c, 4d and 4e have no off switch, for the reasons in [Disabling a gate](#disabling-a-gate) below. Rule 15b demotes a soft forecast-rain skip for a zone that is measurably dry, and it gets its own section under [Measured dry-soil exception](#measured-dry-soil-exception).

A missing or untrusted configured probe holds its own zone, even when a neighboring probe reads dry. A zone with no probe binding uses its weather and soil model normally. Manual schedule weather waivers cannot bypass this data hold.

Both weekly and soil automatic plans require complete next-24-hour rain evidence. Missing amounts or gaps hold the affected zone with `planning_forecast`, including under convenience Force; Force cannot turn an unavailable plan into a minimum-duration run. Explicit manual durations use the separate manual-run and schedule-waiver policy.

The `restart_required` gate holds watering after a saved change to a startup-only dependency, such as controller bindings or location. It blocks Force and manual runs as well as scheduled watering, and only a process restart clears it. Later settings saves cannot clear the hold. Threshold-only tuning still applies live. The persistent restart notice explains the pending changes and offers a restart; Stop remains available.

The **Force** control changes scheduled decisions; it does not start a valve immediately. Its confirmation explains the risk of overwatering and wasting water. A global or per-zone Force remains selected until you choose Auto. A global Skip still holds a zone whose own control is set to Force. Configured safety protections and restrictions, rain delay, vacation pause, and hold-all remain in effect. The separate, consequence-confirmed weather waiver on a manual schedule is described in [Manual schedules](schedules.md).

## Disabling a gate

These gates are deterministic and they decide first: the same inputs give the same verdict every time, and nothing you write yourself is consulted until they are done.

Weather and soil recommendation gates can be switched off, which is most of the table. Availability checks remain protected. Rule Lab lists each configurable gate with a plain sentence about what turning it off costs you ("watering can start while it is actively raining"), and the switch makes you confirm that sentence before it saves. Re-enabling is the same switch and asks nothing.

Control, legal, and availability gates have no switch. The restart-required hold, manual override, both vacation pauses, dry-run mode, the watering restrictions you configured, live-weather availability, configured-probe availability, and automatic-plan rain availability are enforced whatever the disable list says. Naming one of them in that list by hand does nothing: the list is filtered before the ladder reads it.

Disabling is config, not code. The switch adds the gate's id to `engine.skip_rules.disabled_rules` and taking it back out re-enables the gate; every config write snapshots the previous file first, so the state before you touched anything is still on disk and can be rolled back. A disabled gate keeps its row in the decision trace, marked "disabled by operator" rather than disappearing, so a verdict that surprises you still shows the gate you switched off months ago.

## Verdict types

The ladder returns one of three verdicts:

- **`skip`**: don't irrigate. `reason` carries a human-readable explanation.
- **`run`**: proceed with the engine's computed runtime.
- **`run_extended`**: water, and mark the run as extended. Rule 16 (heat advisory pre-water) fires it, and so does a [condition rule](#condition-rules) whose action is extend. The verdict is a label on the decision, not a percentage applied to the dispatched seconds: what sets the minutes is [zone math](zone-math.md), and the only rule action that moves them is a scale factor.

## Per-rule details

### Currently raining (rule 5)

Live precipitation intensity from the Tempest hub (or merged from any source advertising `RainIntensityInHr`). 0.01 in/hr (0.25 mm/hr) is essentially "you can see the pavement getting wet"; anything above triggers the skip.

A hard "currently raining" skip only applies when the rain source is observation-grade: a local gauge, an NWS observation, or NOAA MRMS radar. A model forecast rain rate is treated as a soft skip that a measured-dry zone can demote to a run (see [Measured dry-soil exception](#measured-dry-soil-exception)).

### Freeze + soil frost (rules 6-8)

Three independent freeze checks. Air temp now blocks daytime watering on a cold front. Forecast overnight low blocks a 6 AM run when the lawn would freeze later. Soil frost is the strongest signal: cold soil + a sprinkler is how you ice a lawn.

Soil temperature comes from any source providing `soil_temp_yard_min_f`. If no source reports it (probe offline), this rule silently no-ops and the verdict surfaces "(weather rules only; soil rules offline)" instead of a false-clear.

### Wind (rules 9-10)

Two thresholds: live wind right now, and forecast peak with a 5 mph (8 km/h) slack on the latter (forecast peaks tend to overshoot real maxes). Operators with sensitive sprinkler types (mp_rotator, drip) want max_wind_mph lower (~6 mph / 10 km/h); rotor heads tolerate up to 12-15 mph (19-24 km/h).

### Already wet (rule 11)

Fixed floor at 0.05 in (1.3 mm) of accumulated rain today. Configurable but rarely changed, it's a sanity check that says "I'm not going to add water to a wet lawn."

Only observation-grade rain can establish this fact. Forecast rain has its own softer rule and explanation; a model archive cannot be presented as measured rainfall in this gate or the recent observed-rain backstop.

### Observed rain recently (rule 11b)

The sensor-independent backstop. `rain_observed_recent_in` sums today's measured rain plus the past `rain_observed_window_days` (default 1) of measured daily rain totals, and skips watering on its own when that sum reaches `rain_skip_in` (default 0.25 in / 6.4 mm). This is what makes a real afternoon rain suppress the NEXT morning's run: it carries measured rain forward independent of any soil probe or forecast. Because it reads PAST observed rain rather than a forecast, it is not gated on forecast staleness. It is a hard skip that binds every zone (the soil-floor moat below never demotes it).

### Yard-wide soil saturation (rule 12)

Each zone's saturation gate uses its own effective moisture reading and threshold. A saturated zone skips while an eligible dry neighbor may run. When every zone is saturated, the yard reports a skip as well. LocalSky makes and dispatches these decisions itself; no Home Assistant automation is required. Missing readings remain unknown, or are explicitly inferred by the quarantine policy below.

### Forecast rain (rules 13-15)

Three look-ahead windows: next 4 hours (hourly forecast), tomorrow (probability-weighted to deflate uncertain forecasts), and 3-day rollup. The 3-day uses a 1.5x multiplier on the user's rain-skip threshold to require more total rain before skipping (a wider window is a weaker signal).

Missing rain is unknown, including when a provider's weather request succeeds but its precipitation request fails. An enabled forecast-rain gate holds on missing amount evidence and explains what is unavailable; reported zero over a fully covered interval means dry. The measured-dry soil exception below still applies to soft forecast recommendations, while the separate automatic-plan availability gate remains protected. Unknown rain also cannot justify a heat-advisory extension or a forecast deferral beyond its evidence.

### Measured dry-soil exception

A soft, forecast-based rain skip (next 4 hours, tomorrow, or the 3-day rollup) may be demoted to a run when a zone is measured healthy-dry: its soil percent is below its per-zone dry floor, `target_min_pct_soil`, with a present probe reading above zero. This honors measured soil truth over an uncertain forecast. Hard skips (measured rain now, observed recent rain, freeze, wind, soil saturation) are never demotable, and observation-grade rain (a real gauge or MRMS radar) never demotes.

### Bad or offline soil probes (quarantine)

When `soil_quarantine_enabled` is true (the default), a probe that is offline or reads as a wild outlier versus its siblings (beyond `soil_outlier_threshold_pct`, default 35 pp) is distrusted, and that zone's effective soil for the saturation and dry-floor gates is inferred from the trustworthy sibling readings. This stops a single bad-spot probe from driving a saturated zone to water, while a genuinely saturated zone still skips. Set `soil_quarantine_enabled` to false to restore the exact pre-quarantine behavior.

### Heat advisory pre-water (rule 16)

The only built-in rule that can fire `run_extended`; one of your own condition rules can too. Triggers when:

- `temp_max_3day_f >= 95°F` (35°C; or operator's heat_advisory_temp_f)
- `humidity_now_pct >= 60%` (heat_advisory_humidity_pct)
- `days_since_significant_rain >= 2` (heat_advisory_dry_days)
- `rain_3day_weighted_in < 0.5 * rain_skip_in` (forecast doesn't cover it)

Disabled in cooler climates by raising heat_advisory_temp_f.

## Condition rules

The ladder is fixed. On top of it you can build your own rules in Rule Lab, each one a scope (every zone, or a named few), a condition tree over the weather and per-zone soil metrics, and a single action: skip the zone, mark its run extended, or scale its run by a factor. Only the scale factor moves the dispatched minutes; extend labels the verdict and leaves the run length alone.

Those three actions are the whole list, on purpose. A rule can add a skip, tag a run as extended, or resize one; it can never do the opposite. There is no action that clears a freeze, a wind gate, a watering restriction, or a rain skip, and none that forces a run. A scale factor is clamped to 0.5-1.5 no matter what the config file says, and the scaled run is re-capped at [the zone's maximum run time](zones.md#watering-settings), so scaling up cannot push a run past its ceiling.

Condition rules run for every otherwise eligible ordinary zone, including a measured-dry zone that demoted a soft forecast skip, a soil-model zone that already credited forecast rain, and a zone exempt from a restriction. They can add a hold but cannot clear an existing hold. Explicit Force bypasses these structured condition recommendations; enabled safety gates and restrictions still apply. Rhai script rules remain additional holds on every runnable decision, including Force, and the completed per-zone verdict is the one dispatch reads.

Rhai forecast-rain inputs are `()` when their evidence is missing. Scripts must handle that absence explicitly; an arithmetic or comparison error holds watering with the script's name instead of quietly treating missing rain as zero.

Rules run top to bottom in the order they are listed, and the first skip wins. The arrows beside each rule move it earlier or later, which is how you set priority.

This is a different rule from the ladder's "first matching rule wins" above. Up there the first gate to fire ends the decision and the rest are never reached. Here every enabled, in-scope rule is walked: the skips settle on the first one, which supplies the reason, and any scale factors multiply together before the product is clamped. None of that walk reaches the decision trace. A condition rule that decides a zone shows up only as `condition` in that zone's verdict source and reason code; the rules that merely looked, and the ones after the first skip, leave no row behind. To see how a rule reads against the conditions on hand, use the "Would fire now" line beside it in Rule Lab, which re-evaluates live rather than replaying the morning.

Zone soil percent, the 24-hour forecast low, next-four-hour rain, and the weighted three-day rain total can be absent. Comparisons against missing evidence evaluate as unknown, and an expression that still depends on that unknown never fires a condition rule, including through `NOT`. Protected availability gates decide whether the zone can run. Tomorrow's rain probability deliberately reads as 100 percent when unreported, matching the engine's full-weight treatment of a known rain amount; this does not supply a missing amount.

Each rule has its own on/off switch, so silencing one does not mean deleting it. Rules live under `conditions.rules` in `/data/localsky.toml`, separate from the thresholds below.

## Tunable parameters

All thresholds live under `cfg.engine.skip_rules` in `/data/localsky.toml`. The defaults in `src/config/schema.rs` match the v0.1 hardcoded constants exactly so upgrades preserve verdicts:

```toml
[engine.skip_rules]
already_wet_in           = 0.05   # 1.3 mm
rain_now_in_hr           = 0.01   # 0.25 mm/hr
rain_next_4h_skip_in     = 0.10   # 2.5 mm
rain_3day_factor         = 1.5
heat_advisory_temp_f     = 95.0   # 35 C
heat_advisory_humidity_pct = 60.0
heat_advisory_dry_days   = 2
wind_forecast_slack_mph  = 5.0    # 8 km/h
max_wind_mph             = 10.0   # 16 km/h
min_temp_f               = 38.0   # 3.3 C
rain_skip_in             = 0.25   # 6.4 mm
frost_skip_soil_f        = 35.0   # 1.7 C
rain_observed_window_days = 1     # today + N past days of measured rain
soil_quarantine_enabled  = true   # distrust offline / outlier probes
soil_outlier_threshold_pct = 35.0 # pp from sibling median before distrust
```

Edit via `PUT /api/config` (the settings UI does this); changes apply on the next engine tick (default 60s).

## Replay + audit

Every verdict that fires gets logged to `verdict_history` (M0005 migration) with the full Inputs blob as `inputs_json`. Operators investigating a strange decision can replay any historical row through the current engine and compare. `cargo test engine::skip_rules` includes a regression guard test that runs production verdict history through the engine and asserts 100% verdict + reason match.
