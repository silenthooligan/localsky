# LocalSky Irrigation Engine

The engine answers one question: **should I water tomorrow, and if so, how long?** Every dashboard tile, every notification, every controller dispatch derives from a deterministic pipeline rooted in published agronomy and meteorology. This document walks through that pipeline end to end, with citations, so anyone with a slide rule and a quiet afternoon can reproduce the math by hand.

## Pipeline overview

```
Weather sources -> MergedSnapshot -> Engine -> Verdict + per-zone runtime
                                       |
                                       +-- FAO-56 ET0 (eq. 6)
                                       +-- Species Kc (UF/IFAS)
                                       +-- Soil water balance
                                       +-- Skip rules
                                       +-- Cycle-and-soak
```

Each box is a pure function of its inputs. No hidden state, no opinionated overrides, no proprietary fudge factors.

## Inputs

Per source, per tick, LocalSky records:

- Air temperature min / max / mean (deg C internally; converted from F at the boundary)
- Relative humidity (max / min preferred, mean acceptable, dew point as fallback)
- Wind speed at 2m (or 10m if measured higher; eq. 47 corrects)
- Solar irradiance (W/m²)
- Atmospheric pressure (kPa; elevation-derived if missing)
- Rainfall (gross + intensity)
- Day-of-year + latitude + elevation

If multiple sources report the same field, the merge engine picks the winner per [merge policy](configuration.md#sources): max for rainfall (one stuck gauge can't hide actual rain), min for overnight low, highest priority for everything else.

## Reference ET₀

LocalSky implements three methods. The Auto path tries them in order and picks the first one whose inputs are present.

### 1. FAO-56 Penman-Monteith (Allen et al., 1998 eq. 6)

The gold standard. Daily ET₀ over a hypothetical reference grass surface 12 cm tall, well-watered, with albedo 0.23 and a fixed surface resistance of 70 s/m:

```
ET₀ = (0.408 * Δ * (Rn - G) + γ * (900 / (T+273)) * u₂ * (es - ea))
      / (Δ + γ * (1 + 0.34 * u₂))
```

Where:

- `Δ`  -- slope of vapor pressure curve at T_mean (kPa/°C), eq. 13
- `Rn` -- net radiation (MJ/m²/day), eq. 38 + 39 + 40
- `G`  -- soil heat flux (~0 for daily timescale over grass)
- `γ`  -- psychrometric constant (kPa/°C), eq. 8 = 0.665e-3 × P
- `T`  -- mean daily temperature (°C)
- `u₂` -- wind at 2m (m/s)
- `es` -- saturation vapor pressure (kPa), eq. 11 + 12
- `ea` -- actual vapor pressure (kPa), eq. 14-19 depending on humidity inputs

`Rn` is the trickiest term. LocalSky uses ASCE-EWRI 2005's Brunt-form longwave model:

```
Rs   = measured shortwave (or 0.16 * sqrt(Tmax-Tmin) * Ra when missing)
Rns  = (1 - 0.23) * Rs       # net shortwave with albedo
Rso  = (0.75 + 2e-5 * z) * Ra # clear-sky from extraterrestrial
Rnl  = σ * ((Tmax+273)^4 + (Tmin+273)^4)/2 * (0.34 - 0.14*sqrt(ea)) *
       (1.35 * clamp(Rs/Rso, 0.3, 1.0) - 0.35)
Rn   = Rns - Rnl
```

`Ra` (extraterrestrial radiation, MJ/m²/day) is computed analytically from latitude and day-of-year via eq. 21, with the sunset hour angle clamped to `[-1, 1]` so high-latitude polar-day cases don't NaN.

Implementation: [src/engine/et0.rs](../src/engine/et0.rs). Hand-trace tested against eq. 6 for a 50°N April day (Tmax 21.5, Tmin 12.3, RH 84/63, u₂ 2.78, Rs 22.07): ~3.51 mm/day.

### 2. ASCE-EWRI 2005 short-crop reference ET

Practically identical to FAO-56 for daily computation; the coefficients differ at sub-daily resolution where LocalSky doesn't operate. Same code path, different `et0_method` label for operators who want their dashboards to read "ASCE" instead.

### 3. Hargreaves-Samani 1985

Fallback when wind, solar, or humidity are missing:

```
ET₀ = 0.0023 * (Ra * 0.408) * (Tmean + 17.8) * sqrt(Tmax - Tmin)
```

Typical bias vs. PM is +/- 15-25% in humid subtropical climates. Acceptable when better data isn't available; LocalSky flags Hargreaves-derived values in the dashboard math tile so the operator knows.

## Crop ET (ETc)

For each zone:

```
ETc = ET₀ * Kc(species, DOY) * heat_multiplier(heat_index)
```

`Kc` (crop coefficient) is dimensionless, looked up from the [species catalog](grass-species.md) by zone's grass species and the current day-of-year. The catalog ships 12 species + ornamentals + xeriscape with monthly Kc curves; LocalSky interpolates linearly between mid-month anchors, with Dec/Jan wrap, so the curve is smooth year-over-year. Citations live inline in [src/engine/species_catalog.rs](../src/engine/species_catalog.rs).

`heat_multiplier` is the NOAA Steadman heat index applied as an ET boost from 1.00 at HI <= 85°F up to 1.30 at HI >= 105°F. Captures the empirical observation that 100°F + 70% RH dries a lawn faster than ET₀ alone predicts. Defined in [src/engine/skip_rules.rs](../src/engine/skip_rules.rs).

## Soil water balance

Per zone, LocalSky tracks one number: `depletion_mm`, the millimetres of water below field capacity. State evolves daily:

```
depletion[t+1] = clamp(depletion[t] + ETc - effective_rain - applied_water,
                       0, TAW)
```

Where:

- `effective_rain = gross_rain * capture_efficiency`. Default capture efficiency is 0.70 (operator-tunable); accounts for runoff + canopy interception + evaporation losses before water enters the root zone.
- `applied_water` is the depth (mm) of irrigation that reached the soil during this tick.
- `TAW` (Total Available Water, mm) = `(FC - WP) * root_depth_mm`. FC and WP come from the [soil texture catalog](soil-textures.md) (USDA classes, sourced from FAO-56 Table 19 and USDA NRCS Part 652).

Trigger to irrigate:

```
needs_irrigation = (depletion >= RAW)
RAW = TAW * MAD%
```

`MAD` (Management Allowed Depletion) defaults per species. St. Augustine: 50%. Bahia: 55%. Ornamental shrubs: 40%. The catalog cites UF/IFAS for the Florida-relevant species.

Implementation: [src/engine/water_balance.rs](../src/engine/water_balance.rs).

## Runtime to depth

Once the engine decides to irrigate, runtime in seconds is:

```
gross_mm_needed = depletion_mm / capture_efficiency
seconds = (gross_mm_needed / precip_rate_mm_hr) * 3600
```

`precip_rate_mm_hr` per zone comes from either a measured catch-cup calibration (preferred) or the sprinkler-type default (rotor ~10 mm/hr; spray ~38 mm/hr; MP rotator ~10 mm/hr; drip ~4 mm/hr).

Runtime is capped at `max_duration_s` so a misconfigured precip rate can't run a zone for hours.

## Cycle-and-soak

If applying the full runtime at the sprinkler's precipitation rate would exceed the soil's infiltration capacity, water runs off instead of soaking in. The splitter divides the total runtime into N cycles separated by soak gaps:

```
if precip_rate > infiltration_rate:
    max_cycle_minutes = (infiltration_rate / precip_rate) * 60
    N = ceil(total_runtime / max_cycle)
    each cycle = total_runtime / N
    insert soak_minutes (default 30) between cycles
```

`infiltration_rate` comes from the soil catalog, varying by texture and slope (flat / 3-5% / >5% bands per USDA NRCS Part 652 Table 11-3). Sand on flat ground: 50 mm/hr; clay on a steep slope: 3 mm/hr.

Worked example: clay (5 mm/hr infiltration on flat), spray head (15 mm/hr precip), 45-minute total runtime -> 3 cycles of 15 min with two 30-min soaks. Total elapsed wall-clock: 1h 45min. Total water applied: same 45 minutes worth, but it actually enters the root zone instead of running off.

Implementation: [src/engine/cycle_soak.rs](../src/engine/cycle_soak.rs).

## Skip rules

Before any zone fires, the engine runs a deterministic 17-rule ladder. First matching rule wins. Order encodes intent: explicit user overrides > paused > current-conditions safety (raining now, freeze, soil frost, wind) > soil saturation > forecast skips > heat advisory > dry-run > run.

Full enumeration in [skip-rules.md](skip-rules.md). All thresholds are typed config fields in `cfg.engine.skip_rules`; defaults match v0.1 hardcoded values exactly so upgrading doesn't change any verdict for unchanged inputs.

## Heat advisory pre-water

When the 3-day forecast shows >= 95°F + >= 60% RH and the zone has been dry for >= 2 days, the engine returns verdict `run_extended` instead of plain `run`. Dashboard surfaces this; the controller adapter receives 115% of the computed runtime. Empirically gets ahead of the heat stress before it shows in the soil moisture data. Disabled if the 3-day rain forecast covers >= half the operator's rain-skip threshold.

## 7-day forward verdict strip

Every dashboard render projects the next 7 days through the same rule ladder, using the daily forecast as synthetic Inputs. The "preview" is the actual decision the engine would make if today were that future day, with the live-only signals (wind_now, rain_intensity_now) zeroed out so they don't false-fire. Operator gets a glance-able strip showing "skip Tuesday because heavy rain forecast", "run extended Friday because heat advisory", etc.

Implementation: [src/engine/verdict_strip.rs](../src/engine/verdict_strip.rs).

## Provenance

Every field in the merged snapshot records `source_id`, `observed_at`, and an optional `method` tag. The dashboard's math tile reveals "ET₀ 5.2 mm via tempest_lan (penman_monteith)" or "wind 8 mph via open_meteo (forecast)". Operators always know which input drove which decision; no opaque "the system says so."

## Where to read further

- [Grass species catalog](grass-species.md): 12 species with monthly Kc curves and citations
- [Soil texture catalog](soil-textures.md): USDA classes with FC, WP, AW, infiltration
- [Skip rules](skip-rules.md): every rule in the ladder with its config knob
- [Configuration reference](configuration.md): every `cfg.engine.*` field and its default
