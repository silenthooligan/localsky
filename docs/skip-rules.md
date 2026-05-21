# Skip Rules

LocalSky's irrigation skip-check is a 17-rule ladder. Every morning (or whenever the engine recomputes), inputs flow through the ladder in order. **First matching rule wins.** Order matters: explicit overrides beat safety beats current conditions beat forecast beats heat advisory beats dry-run beats run.

Source: [src/engine/skip_rules.rs](../src/engine/skip_rules.rs).

## Ladder

| # | Rule | Trigger | Threshold | Tunable? |
|---|------|---------|-----------|---------:|
| 1 | Manual override: skip tomorrow | `is_tomorrow && override_tomorrow == "skip"` | - | UI |
| 2 | Manual override: run tomorrow | `is_tomorrow && override_tomorrow == "run"` | - | UI |
| 3 | Vacation pause (timed) | `pause_until_epoch > now_epoch` | - | UI |
| 4 | Vacation pause (toggle) | `is_paused == true` | - | UI |
| 5 | Currently raining | `rain_intensity_now_in_hr > 0.01` | 0.01 in/hr | `rain_now_in_hr` |
| 6 | Freeze risk now | `temp_now_f < min_temp_f` | 38°F | `min_temp_f` |
| 7 | Overnight freeze | `temp_min_24h_f < min_temp_f` | 38°F | `min_temp_f` |
| 8 | Soil frost | `soil_temp_yard_min_f < frost_skip_soil_f` | 35°F | `frost_skip_soil_f` |
| 9 | Wind too high now | `wind_now_mph > max_wind_mph` | 10 mph | `max_wind_mph` |
| 10 | Windy day forecast | `wind_max_today_mph > max_wind_mph + 5` | +5 mph slack | `wind_forecast_slack_mph` |
| 11 | Already wet | `rain_today_in >= 0.05` | 0.05 in | `already_wet_in` |
| 12 | All zones soil-saturated | every zone's moisture % >= saturation threshold | per-zone | per-zone soil settings |
| 13 | Rain in next 4 hours | `rain_next_4h_in >= 0.10` | 0.10 in | `rain_next_4h_skip_in` |
| 14 | Tomorrow rain (confidence-weighted) | `forecast_in * prob/100 >= rain_skip_in` | 0.25 in (weighted) | `rain_skip_in` |
| 15 | 3-day rain rollup | `rain_3day_weighted_in >= 1.5 * rain_skip_in` | 1.5x multiplier | `rain_3day_factor` |
| 16 | Heat advisory (pre-water) | 3-day max >= 95°F + humidity >= 60% + 2+ dry days | composite | `heat_advisory_*` |
| 17 | Dry-run mode | `is_dry_run == true` | - | UI |
| - | Default | (no rule matched) | - | run |

## Verdict types

The ladder returns one of three verdicts:

- **`skip`** - don't irrigate. `reason` carries a human-readable explanation.
- **`run`** - proceed with the engine's computed runtime.
- **`run_extended`** - proceed at 115% of the engine's computed runtime. Used only by rule 16 (heat advisory pre-water).

## Per-rule details

### Currently raining (rule 5)

Live precipitation intensity from the Tempest hub (or merged from any source advertising `RainIntensityInHr`). 0.01 in/hr is essentially "you can see the pavement getting wet"; anything above triggers the skip.

### Freeze + soil frost (rules 6-8)

Three independent freeze checks. Air temp now blocks daytime watering on a cold front. Forecast overnight low blocks a 6 AM run when the lawn would freeze later. Soil frost is the strongest signal: cold soil + a sprinkler is how you ice a lawn.

Soil temperature comes from any source providing `soil_temp_yard_min_f`. If no source reports it (probe offline), this rule silently no-ops and the verdict surfaces "(weather rules only; soil rules offline)" instead of a false-clear.

### Wind (rules 9-10)

Two thresholds: live wind right now, and forecast peak with a 5 mph slack on the latter (forecast peaks tend to overshoot real maxes). Operators with sensitive sprinkler types (mp_rotator, drip) want max_wind_mph lower (~6); rotor heads tolerate up to 12-15 mph.

### Already wet (rule 11)

Fixed floor at 0.05 in of accumulated rain today. Configurable but rarely changed - it's a sanity check that says "I'm not going to add water to a wet lawn."

### Yard-wide soil saturation (rule 12)

Skip only when EVERY zone reports moisture >= its per-zone saturation threshold AND every zone has a current reading (no None / probe-offline). A single dry zone or a single missing reading breaks the skip. The per-zone HA automation `irrigation_per_zone_saturation_skip` still mutes individual saturated zones; this rule operates at the sequence level.

### Forecast rain (rules 13-15)

Three look-ahead windows: next 4 hours (hourly forecast), tomorrow (probability-weighted to deflate uncertain forecasts), and 3-day rollup. The 3-day uses a 1.5x multiplier on the user's rain-skip threshold to require more total rain before skipping (a wider window is a weaker signal).

### Heat advisory pre-water (rule 16)

The only rule that can fire `run_extended`. Triggers when:

- `temp_max_3day_f >= 95°F` (or operator's heat_advisory_temp_f)
- `humidity_now_pct >= 60%` (heat_advisory_humidity_pct)
- `days_since_significant_rain >= 2` (heat_advisory_dry_days)
- `rain_3day_weighted_in < 0.5 * rain_skip_in` (forecast doesn't cover it)

Empirically gets ahead of heat stress that ET-based math underestimates on multi-day spikes. Disabled in cooler climates by raising heat_advisory_temp_f.

## Tunable parameters

All thresholds live under `cfg.engine.skip_rules` in `/data/localsky.toml`. The defaults in [src/config/schema.rs](../src/config/schema.rs) match the v0.1 hardcoded constants exactly so upgrades preserve verdicts:

```toml
[engine.skip_rules]
already_wet_in           = 0.05
rain_now_in_hr           = 0.01
rain_next_4h_skip_in     = 0.10
rain_3day_factor         = 1.5
heat_advisory_temp_f     = 95.0
heat_advisory_humidity_pct = 60.0
heat_advisory_dry_days   = 2
wind_forecast_slack_mph  = 5.0
max_wind_mph             = 10.0
min_temp_f               = 38.0
rain_skip_in             = 0.25
frost_skip_soil_f        = 35.0
```

Edit via `PUT /api/config` (the settings UI does this); changes apply on the next engine tick (default 60s).

## Replay + audit

Every verdict that fires gets logged to `verdict_history` (M0005 migration) with the full Inputs blob as `inputs_json`. Operators investigating a strange decision can replay any historical row through the current engine and compare. `cargo test engine::skip_rules` includes a regression guard test that runs production verdict history through the engine and asserts 100% verdict + reason match.
