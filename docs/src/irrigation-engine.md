# LocalSky Irrigation Engine

The engine answers one question: **should I water tomorrow, and if so, how long?** Every dashboard tile, every notification, every controller dispatch derives from a deterministic pipeline rooted in published agronomy and meteorology. This document walks through that pipeline end to end, with citations, so anyone with a slide rule and a quiet afternoon can reproduce the math by hand.

## Pipeline overview

```
Weather sources ---------> MergedSnapshot -> Engine -> Verdict + per-zone runtime
Ecowitt GW (native poll) /                    |                |
                                              +-- FAO-56 ET0   +-> OpenSprinkler HTTP
                                              +-- Species Kc       (opensprinkler_direct)
                                              +-- Soil water balance
                                              +-- Skip rules (frost-skip uses native soil temp)
                                              +-- Cycle-and-soak
                                              |
                                              +-> Publishes results to HA
                                                  (sensor.localsky_<zone>_soil_*, valves, verdict)
```

LocalSky owns the full pipeline end to end: it polls the Ecowitt gateway directly, runs all ET and water-balance math internally, evaluates skip rules (including frost-skip against its own native soil-temperature readings), and actuates OpenSprinkler via a direct HTTP controller (`opensprinkler_direct`, targeting the controller's LAN address). Results are published back to HA for display, but HA is a consumer, not a driver: nothing LocalSky decides is read from a Home Assistant entity. As of 0.7.22 that includes the skip thresholds and the four operator controls, which used to live in `input_*` helpers and were read one last time on upgrade. What still reads a Home Assistant entity is what you pointed at one by name (a zone's soil sensor), your controller's own entities on a legacy Home-Assistant-only install, and nine legacy `sensor.open_meteo_*` forecast fallbacks. The complete list, and what changed, is in [Migrating your watering off Home Assistant](migrating-from-ha.md). No Smart Irrigation, no Irrigation Unlimited, no MQTT sidecar.

Each box is a pure function of its inputs. No hidden state, no opinionated overrides, no proprietary fudge factors.

## Inputs

Per source, per tick, LocalSky records:

- Air temperature min / max / mean (deg C internally; converted from F at the boundary)
- Relative humidity (max / min preferred, mean acceptable, dew point as fallback)
- Wind speed at 2m (or 10m if measured higher; eq. 47 corrects)
- Solar irradiance (W/m²)
- Atmospheric pressure (kPa; elevation-derived if missing)
- Rainfall (gross + intensity)
- Observed rain over the recent window (today plus prior days' measured totals, sensor-independent so a dropped soil probe or a paused source can't hide real rain that already fell)
- Day-of-year + latitude + elevation

Rainfall carries an honesty tier alongside the value: measured (a real gauge caught it), radar (a radar/QPE estimate), or model (a forecast figure). Downstream skip logic weights a measured total differently from a model guess.

Soil inputs (natively polled from the Ecowitt GW1100B gateway's LAN address):

- Per-zone soil moisture % (calibrated from raw FDR AD against dry/wet endpoints in LocalSky config)
- Per-zone soil temperature (used directly for the frost-skip rule; no HA aggregation step)
- Per-zone EC and battery state

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

Implementation: [src/engine/et0.rs](../src/engine/et0.rs). A regression uses FAO-56 chapter 4, Example 18 (Uccle, July 6): 3.88 mm/day, including the published 10 m to 2 m wind conversion.

### 2. ASCE-EWRI 2005 short-crop reference ET

Practically identical to FAO-56 for daily computation; the coefficients differ at sub-daily resolution where LocalSky doesn't operate. Same code path as FAO-56 for LocalSky's daily computation. The method actually used is chosen automatically from the inputs available; it is not selectable, and no screen reports which one ran.

### 3. Hargreaves-Samani 1985

Fallback when wind, solar, or humidity are missing:

```
ET₀ = 0.0023 * (Ra * 0.408) * (Tmean + 17.8) * sqrt(Tmax - Tmin)
```

Hargreaves is an approximation whose error depends on local climate and calibration. It is used when the inputs required for Penman-Monteith are missing. Daily peak wind and afternoon humidity are not daily means and cannot select Penman-Monteith.

## Crop ET (ETc)

For each zone:

```
ETc = ET₀ * Kc(species, DOY, latitude)
```

`Kc` (crop coefficient) is dimensionless, looked up from the [species catalog](grass-species.md) by zone's grass species and the current day-of-year. The catalog ships 12 species + ornamentals + xeriscape with monthly Kc curves; LocalSky interpolates linearly between mid-month anchors, with Dec/Jan wrap, so the curve is smooth year-over-year. Citations live inline in [src/engine/species_catalog.rs](../src/engine/species_catalog.rs).

Reference ET already accounts for weather demand. LocalSky applies no extra
heat-index or VPD multiplier to it; the legacy API multiplier fields remain
1.0. Human heat index remains a weather/advisory signal. Soil planning uses
potential crop demand, a single root-zone bucket, and configured/catalog
parameters; it does not claim to measure actual plant transpiration.

## What decides watering today

Two models are selectable, per install and per zone. `engine.scheduling_model`
picks the default: `soil`, which an install with no key set follows, or
`weekly`, which an install follows only by pinning it here. The setup wizard
writes `soil` explicitly for new installs, so the choice is recorded rather
than inherited. A zone's own `scheduling_model` field pins either model for
that zone regardless of the engine default. The Engine settings page carries
the install-wide selector and the zone editor carries the per-zone pin; both
hot-reload, so a save applies on the next scheduler tick.

- **Weekly water balance** (`weekly`): the model in the next section. A
  gross weekly target per zone, settled against rain, applied water, and a
  forecast credit; the remainder splits across the week's sessions.
- **Soil model** (`soil`): the FAO-56 depletion bucket below. Each zone
  waters when its own soil deficit crosses the trigger, and each run
  refills the deficit, so cadence follows soil texture and roots instead
  of `sessions_per_week`.

Whichever model governs, the soil model computes on every install for
every zone that has a zone config, and publishes a bucket for it once
that zone has at least three evidenced days and the replay has resolved its unknown initial moisture. The Deficit tiles on the
zone card, the zone detail and the dashboard show the replayed deficit
(negative = needs water), the zone detail's Soil model block shows what
the model waters, or would water, today and when it waters next, and the
[tuning report](tuning-report.md) carries a comparison line on every
weekly-governed zone. A zone with no agronomy config (env-var zone lists)
stays on the weekly model and its deficit reads a dash.

### The soil model

The bucket arithmetic lives in
[src/engine/water_balance.rs](../src/engine/water_balance.rs) and the
planner in [src/engine/soil_schedule.rs](../src/engine/soil_schedule.rs):

```
depletion[t+1] = clamp(depletion[t] + ETc - effective_rain - applied_water,
                       0, TAW)
needs_irrigation = (depletion >= RAW),   RAW = TAW * MAD%
```

with `TAW` from the [soil texture catalog](soil-textures.md) at the
species' default or overridden root depth, and `MAD` per species.

**Replay.** Every evaluation replays up to 14 local days, beginning with the
first evidenced day. Leading unknown days do not invent a drought. Two copies
start at opposite limits: field capacity and wilting point. A deficit becomes
available only when their final values agree within 0.1 mm and at least three
days carry evidence. Deep roots or low winter ET can leave the initial state
uncertain beyond that window; elapsed time alone never establishes it.
When the point is uncertain, the planner evaluates both interval bounds. Agreement can support a hold or a minimum evidenced refill; disagreement holds automatic watering with an uncertainty explanation. With fewer than three evidenced days, the weekly target remains the named fallback.

Each date charges its own reference ET times seasonal, hemisphere-aware Kc.
Effective rain and completed valve-open intervals refill the bucket. The ET
ladder uses recorded daily evidence, then the forecast archive, then the zone's
weekly-target-derived demand assumption for gaps within the known span.
A reported zero ET remains zero. Today's partial ET uses an entirely covered
remaining-day curve, or the elapsed fraction of that actual local day's length.

**Trigger and rain timing.** The planner projects depletion through daily plant
water use and forecast rain until the next legal watering morning. Waiting is
appropriate while reserve stays below RAW. Rain is applied after the day's
demand in this conservative arrival-time calculation; forecast probability and
local bias adjust its expected contribution. A distant storm cannot erase a
stress threshold crossed before that storm arrives.

**Sizing.** Without useful upcoming rain, a run refills the current deficit.
When rain can help but waiting would cross the trigger, the planner solves for
a smaller bridge through its arrival, even when another legal morning comes
first. Each candidate refill is replayed through the daily balance and drainage;
water applied before a filling storm cannot remain as credit after that storm.
Gross seconds are net refill divided by
capture efficiency and application rate, capped by the zone's run limit. An
explicit weekly budget remains a delivery ceiling; an inferred target does not.
If current storage and legal frequency cannot cover demand, the reason explains
that limit. Watering beyond field capacity would drain below the roots.

**Forecast misses.** Actual rain and completed watering update the next replay.
A shower that fails to arrive leaves greater depletion and a larger required
refill; the horizon does not grant arbitrary days of plant stress. Existing
morning decisions remain in history with their original reasons.

**The outlook.** The same sizing and decision passes advance through a scenario
copy of the balance, carrying modeled irrigation and legal watering days forward.
They never write forecast water into measured history. The scenario checks the
forecast's current age separately from the future day. Beyond hourly coverage,
daily rain totals provide a coarse projection; live dispatch still requires fresh,
complete hourly planning data. Current sensors, restrictions and safety checks
remain authoritative at the actual morning.

**Window admission.** When several zones trigger on the same morning,
they are admitted most-stressed-first (deficit over RAW, descending)
against the pre-sunrise window, priced by the dispatcher's own wall-time
arithmetic (cycle-soak splits, soak gaps, interleaving). The
most-stressed zone is always admitted, even alone over the window. A zone
that does not fit waters tomorrow at higher stress and says so in its
reason line; nothing is dropped silently.

**Gates.** Every safety and compliance gate still binds: wind, freeze,
rain now, already wet, the observed-rain backstop, soil-probe saturation
and quarantine, watering restrictions, pause, dry-run. Three forward-rain
gates (rain within 4 hours, tomorrow rain, 3-day rain) and the
heat-advisory extension are inert for soil-governed zones: defer by
deficit already prices forecast rain against the deficit, and measured
ET0 already charges hot days into it, so those gates would count the same
signal twice. The rule catalog names this on each row; weekly-governed
zones keep all four.

**Weekly fields under the soil model.** A hand-set `weekly_budget_in`
stays honored as the delivery ceiling above. `sessions_per_week` stops
steering: cadence is emergent, roughly RAW divided by daily ETc, which is
the same interval the tuning report's bucket check computes. Both keep
their full meaning for weekly-governed zones. `rain_credit_cap_in` keeps
its per-day meaning inside the replay, and an unset cap is emergent: the
`[0, TAW]` clamp already bounds what a single day of rain can credit.

## Weekly water balance

The model a zone follows when the install or the zone pins `weekly`. The weekly allocator sizes each zone's sessions against a true water balance in gross homeowner terms: the target is "inches per week including rain," and the week's ledger settles before any session is sized.

```
rain_cap_mm             = rain_credit_cap_in * 25.4, or unset:
                          (field_capacity - wilting_point) * root_depth_mm
credited_rain_mm        = sum over trailing days of min(day_rain_mm, rain_cap_mm)
weekly_target_gross_mm  = weekly_budget_in * 25.4
remainder = max(0, weekly_target_gross_mm
                   - credited_rain_mm
                   - irrigation_applied_trailing_mm
                   - bias_corrected_forecast_credit_mm)
session_gross_mm    = remainder / remaining_sessions
seconds_per_session = session_gross_mm / throughput_mm_hr * 3600   (capped at the run limit)
```

- The trailing window is a rolling 7 local days ending now; there is no calendar-week anchor.
- Observed rain resolves through a ladder with per-rung provenance and COVERAGE precedence: when the observations ledger holds any gauge or radar day rows for the window, the measured record wins outright, even at 0.00 in (a yard that measured a dry week is ground truth a wetter regional model must not override). Only when measured coverage is entirely absent does the forecast provider's past-day model archive supply the term, and an install with neither runs on the corrected forecast alone; the tuning report line names which rung applied. The winning rung supplies both the raw window total (which rides the wire as `observed_rain_mm`) and the per-day series the credit is computed from.
- Rain credits per day, each day capped at `rain_cap_mm`, the root zone's own capacity (TAW from the [soil texture catalog](soil-textures.md) at the species' default or overridden root depth, or the zone's explicit `rain_credit_cap_in`). Rain beyond the cap in a single day drains below the root zone and never becomes plant-available, so a 1.2 in storm day on sand credits about 0.35 in rather than settling a 1.0 in week outright. A week whose rain never exceeded the cap on any day settles exactly as the raw sum. The cap does not decay older rain by ET: the weekly target already encodes typical ET, and decaying the credit would count it twice. Each forward forecast-credit day is held to the same cap.
- Applied irrigation is the union of completed watering evidence in the window (duplicate manual/observer intervals count once; soak gaps supply no water) times the zone's precipitation rate. Gross in against a gross target: no capture factor on either side. Every completed run counts, whoever commanded it: water in the ground is water in the ground, so a manual run and a manual schedule's run shrink the remainder and move the session-spacing anchor exactly as a smart run does.
- The forecast credit covers only the days between tomorrow and the zone's next expected session, corrected by the per-month bias multiplier (below). Rain past the next session is never credited now; it will be observed rain by the time it matters. Imminent rain is handled by the 24-hour defer gate, not the credit.
- The 24-hour defer gate compares the next 24 forecast hours against `engine.session_rain_defer_in` (default 0.10 in), weighting each hour by its precipitation probability, the same weighting the forward credit uses. An hour with no reported probability weights at full value. Before 0.7.22 the gate summed the raw model depth and read a compile-time constant instead of the configured threshold, so a low-probability drizzle could zero every zone almost daily and raising the documented knob changed nothing. Soil-governed zones replace this fixed depth with defer by deficit (above).
- `remaining_sessions` is `sessions_per_week` minus the completed events in the window, floor 1. A full nominal session earns `floor(7 / sessions_per_week)` local days of spacing. When its delivered depth is known, a partial session earns that interval times its fraction of nominal session depth, rounded down with a one-day minimum. Unknown historical depth retains configured spacing.

Fixed in 0.7.17: the previous formula multiplied delivery by the heat multiplier and divided by capture efficiency (0.70), inflating session length by up to about 1.9x against a target that already reads as gross, and it credited only forward forecast rain: rain that had already fallen and water already applied never counted, so a soaked week could still schedule full sessions. In 0.9.0, ETc is reference ET times seasonal Kc, without an additional heat multiplier. The soil balance honors its capture-efficiency assumption separately from this gross weekly target. Relative probe calibration does not establish volumetric water content, so those readings no longer produce an unsupported future percentage curve.

Implementation: [src/engine/budget.rs](../src/engine/budget.rs) (the one pure implementation; the refresher assembles its inputs).

## Cycle-and-soak

If applying the full runtime at the sprinkler's precipitation rate would exceed the soil's infiltration capacity, water runs off instead of soaking in. The splitter divides the total runtime into N cycles separated by soak gaps:

```
if precip_rate > infiltration_rate:
    max_cycle_minutes = (infiltration_rate / precip_rate) * 60
    N = ceil(total_runtime / max_cycle)
    each cycle = total_runtime / N
    ponded_mm = (precip_rate - infiltration_rate) * cycle_hours
    soak after every cycle but the last, in minutes =
        max((ponded_mm / infiltration_rate) * 60, texture_floor_min, soak_minutes)
```

`infiltration_rate` comes from the soil catalog, varying by texture and slope (flat / 3-5% / >5% bands per USDA NRCS Part 652 Table 11-3). Sand on flat ground: 50 mm/hr; clay on a steep slope: 3 mm/hr.

The soak is derived per zone, not configured. A cycle leaves `(precip_rate - infiltration_rate) * cycle_length` of water standing when the head shuts off, and that depth drains at the soil's own infiltration rate, so the soak is however long clearing it takes. Clay waits longer than sand without being told to, and a zone whose head applies water slower than the soil takes it never soaks at all.

Two floors sit under the derived figure. The first is per texture: 5 minutes on sand and loamy sand, 10 on the sandy loam / loam / silt loam family, 15 on clay loam and clay. The drain arithmetic assumes a steady intake rate, but real surfaces start fast and slow down, water moves sideways under the canopy, and a head's pattern is never uniform, so a soak computed at a few seconds is not a pause the soil would recognize. The second is `engine.soak_minutes`, your own floor for a surface you know needs longer than the catalog says. It defaults to 5, takes 5 to 120 minutes on the Engine settings page, and is a floor only: it raises a short soak and never shortens a long one. It used to be the soak itself, one number for every soil, and its old default of 30 was clay's answer applied to sand. `soak_minutes` hot-reloads with the rest of the watering policy, so a change applies on the next evaluation, no restart needed.

Worked example: clay (5 mm/hr infiltration on flat), MP rotator heads at the catalog's 14 mm/hr, 45-minute total runtime -> 3 cycles of 15 min with two 27-min soaks. Those 27 minutes are derived, not a default: each 15-minute cycle puts down 3.5 mm, the clay takes 1.25 mm of it while the head runs, and the 2.25 mm left standing clears in 27 minutes at 5 mm/hr. Total elapsed wall-clock: 1h 39min. Total water applied: same 45 minutes worth, but it actually enters the root zone instead of running off.

Implementation: [src/engine/cycle_soak.rs](../src/engine/cycle_soak.rs).

### Cycle interleaving

With `interleave_cycles = false` the morning sequence is strictly serial: a zone runs every one of its cycles, idling through each soak, before the next zone starts. In the worked example above that is 1h 39min of wall clock to apply 45 minutes of water, and every other zone waits behind it.

`interleave_cycles = true` in the `[engine]` table (the default, and the toggle on the Engine settings page) interleaves instead: during one zone's soak pause, another zone's cycle runs, the way dedicated irrigation controllers handle cycle-and-soak. The planner lays every zone's cycles on a single valve timeline, dispatching whichever zone can start earliest. The rules it never breaks:

- One valve at a time, always. Interleaving never opens two zones together, regardless of what the controller hardware could do.
- Every soak is a minimum, not an exact gap. A soak stretches when another zone's cycle is still running as it expires; it never shrinks.
- Each zone's cycles run in order, and the sequence never takes longer than the serial plan.

Worked example, continued: add a rotor zone that needs one 20-minute pass. Serial, the sequence takes 1h 39min for the clay zone plus 20 minutes for the rotor, about 1h 59min. Interleaved, the rotor pass runs inside the clay zone's first 27-minute soak and the whole sequence finishes in the clay zone's own 1h 39min.

The default is on: with a municipal or otherwise pressurized supply, the shorter sequence is strictly better. Turn it off on installs fed by a well or a low-recovery pump, where the serial plan's idle soak gaps double as recovery time for the water source and interleaving would fill that idle time with more pumping. The setup wizard's water-supply question sets this for you; the toggle lives on the Engine settings page. The setting hot-reloads with the rest of the watering policy, so a change applies on the next scheduler tick (the next morning's plan), no restart needed.

Either way, the scheduler works the sequence's true wall time (runs, soaks, and preambles) backwards from its sunrise finish target, so a cycle-and-soak morning still ends about 15 minutes before sunrise.

Implementation: [src/engine/interleave.rs](../src/engine/interleave.rs).

## Seasonal water budget

Three things size a run: the scheduling model, which decides how much water a zone is owed; the cycle-and-soak pacing above, which decides how that water gets delivered; and the seasonal water budget, a single dial over the top of both. The thresholds that skip a run outright are a different mechanism, enumerated in [skip-rules.md](skip-rules.md).

`engine.seasonal_adjust_pct` is that dial: a percentage of the computed run depth, 100 by default, which means every run waters exactly what the math above produced. Dial it down in a wet, cool stretch and up in a heat wave, the same control a commercial controller labels seasonal adjust. It lives on the Engine settings page as a 50 to 150% slider, and the multiplier is clamped to that same range in code, so a hand-edited config still cannot triple a run or dry out a yard. A dial of 0 reads as "never set" and changes nothing, rather than watering nothing.

The order the dial is applied in is load-bearing. The dial scales the raw budget first, then the per-zone maximum duration clamps the scaled figure, then the force-run floor gives a zone you deliberately forced a bounded default if the result came out zero, and only after all of that does a condition rule's multiplier layer on and get re-clamped to the same ceiling. Backwards, a dial above 100% would take an already-capped run and push it back over the ceiling, and that ceiling is the tighter of your zone's own limit and any duration cap an active [watering restriction](restrictions.md) imposes. Getting the order wrong is not merely untidy: it can dispatch past a legal limit.

Because the dial is applied while the snapshot is assembled rather than at dispatch time, the planned minutes on the zone card and in the math panel already reflect it, so display and dispatch agree. A run that lands on its ceiling because the dial pushed it there reports the cap as the binding constraint, the same way a run the weekly allocator capped does.

The dial applies whichever scheduling model governs the zone: a soil row's seconds run through it exactly as a weekly row's do, including when the pre-sunrise window prices which zones fit. It hot-reloads with the rest of the watering policy, so a change applies on the next evaluation, no restart needed.

Implementation: [src/engine/sizing.rs](../src/engine/sizing.rs) (`seasonal_multiplier`, `seasonal_capped`), applied in [src/assembly/mod.rs](../src/assembly/mod.rs).

## Skip rules

Before any zone fires, the engine runs one deterministic ladder in that zone's scope. First matching applicable gate wins. Operator holds, applicable watering restrictions, unavailable weather, and enabled freeze, soil-frost, and wind protections remain binding. A confirmed Force can set aside rain and soil recommendations; it cannot silently waive those protections. Ordinary runs also pass through the owner's condition rules, and Rhai script holds apply to every runnable decision. Dispatch consumes these completed zone verdicts, and the yard headline summarizes them, including mornings when only some zones can run.

Observed recent rain (measured and sensor-independent) is checked before both the soil and forecast gates: if enough real rain has already fallen over the recent window, the zone skips regardless of what a probe or a forecast says. A soft forecast-rain skip is not the last word, though: when a zone reads measured-dry, the engine can demote that forecast skip back to a run so soil truth wins over an uncertain forecast (the soil floor moat). And an offline or outlier soil probe does not silently break a zone: its state is inferred from trustworthy neighboring probes (soil quarantine) so one bad reading can't force a skip or a needless run.

Full enumeration in [skip-rules.md](skip-rules.md). Thresholds are typed config fields in `cfg.engine.skip_rules`. The 0.9.0 fixes can change a decision that previously missed a hold or disagreed with its zone card; existing configured thresholds are preserved.

## Heat advisory pre-water

When the configured heat, humidity, and recent-dryness thresholds are met, the weekly model can return `run_extended` instead of plain `run`. This labels the decision; it does not add a separate 15% runtime multiplier. The water balance and zone sizing determine the minutes. Soil-model zones already account for hot weather through modeled water use and do not get a second heat adjustment. The advisory stays inactive when forecast rain sufficiently covers demand.

## 7-day forward verdict strip

Every dashboard render projects the next 7 days through the same rule ladder, using the daily forecast as synthetic Inputs. The "preview" is the actual decision the engine would make if today were that future day, with the live-only signals (wind_now, rain_intensity_now) zeroed out so they don't false-fire. Operator gets a glance-able strip showing "skip Tuesday because heavy rain forecast", "run extended Friday because heat advisory", etc.

Implementation: [src/engine/verdict_strip.rs](../src/engine/verdict_strip.rs).

## Provenance

Every field in the merged snapshot records `source_id`, `observed_at`, and an optional `method` tag. The dashboard's math tile reveals "ET₀ 5.2 mm via tempest_lan (penman_monteith)" or "wind 8 mph via open_meteo (forecast)". Operators always know which input drove which decision; no opaque "the system says so."

## Forecast bias correction

Open-Meteo, NWS, and every other regional forecast source carries systematic bias in any given microclimate. A bowl behind a hill that sees consistent overprediction in summer afternoons doesn't need the operator to hand-tune their rain-skip threshold every season; LocalSky learns the bias from observed data and folds it out.

### How it works

Every refresh, LocalSky records one row per local calendar day in `forecast_observations`:

| column | source |
|---|---|
| `predicted_in` | The first available forecast total for that local day (`forecast.daily[].precip_sum_in`). A missing prediction retains the existing internal `-1` marker, excluded from bias fitting; a later valid prediction can replace that marker. A real prediction, including zero, is then held fixed. |
| `observed_in` | The day's observed rain from the merge-contested daily total. Day-max: the recorded value only ever rises within a day, so a gauge going stale mid-storm cannot reset the total. |
| `observed_source` | Which kind of source supplied the day's max: `gauge` or `radar` for measured day totals, or `none` (a placeholder 0.0, excluded from the bias fit, the dryness counters, and the scorecard). A model-nature rain owner also records the placeholder: its "rain today" is the whole day's forecast, including hours that have not happened, and the day-max semantics would make phantom rain permanent. Rows written before 0.7.17 read `legacy` and count as gauge-quality only on installs with a station source. |
| `month` | 1..12, denormalized so the bias query indexes by month-of-year. |

Measured rain continues to be recorded even when a forecast is unavailable. The first valid prediction is held fixed; later refreshes refine the observation. The hard recent-rain gate accepts only explicitly tagged gauge/radar history, including measured zero; model and legacy rows cannot establish that measured backstop. Historical bias fitting retains its separate compatibility policy for legacy rows. Once `MIN_OBSERVATIONS` (currently 5) usable days exist in a given month within the rolling 90-day window, the engine computes a per-month bias multiplier:

```
multiplier = median(observed_in / predicted_in)   over the month bucket
multiplier = clamp(multiplier, 0.5, 1.5)
```

Multiplicative not additive: rain bias is the same shape at 0.2 inch and 2.0 inch. Median not mean: a single 2-inch surprise storm shouldn't tank the model.

### Where it surfaces

- **API:** `GET /api/v1/forecast/bias` returns the current-month multiplier plus the full 12-month table with sample counts.
- **Pure module:** `engine::forecast_bias::BiasModel::from_observations(observations, today, window)` is callable from anywhere; ideal for backtests and replay against historical verdict logs.
- **The weekly balance:** the balance's forward credit is the multiplier's first engine consumer: `credit = forecast_rain * precip_weight * multiplier_for(month)` over the days until the zone's next session. Under-trained months multiply by 1.0 by design, and the tuning report states the sample count instead of implying a correction. The multiplier applies only to forward forecast, never to observed terms. Rows whose observed side had no rain-capable source (`observed_source = 'none'`) are excluded from the fit, so gauge-less installs cannot train the floor on fabricated dry days.
- **Skip rules:** the multiplier is not yet folded into the rain inputs going into the skip ladder; wiring `corrected_rain = raw_rain * multiplier` upstream of `skip_rules::evaluate` remains planned. The same observation rows already do decision work elsewhere: the observed-rain backstop reads them live, and the [tuning report](tuning-report.md)'s forecast-skip scorecard judges every rain-family skip against them.

### Defaults and bounds

| Constant | Value | Why |
|---|---|---|
| `MIN_OBSERVATIONS` | 5 | Below this, a single outlier dominates. Multiplier stays at 1.0. |
| `BIAS_FLOOR` | 0.5 | Real bias rarely halves a forecast; below this is almost certainly a broken pipeline. |
| `BIAS_CEIL` | 1.5 | Same intuition on the other side. |
| `DEFAULT_WINDOW_DAYS` | 90 | One season. Tracks microclimate shifts without dragging in last year's summer into this year's. |
| `NOISE_FLOOR_IN` | 0.02 | Below this in both columns, the day is "dry" and not informative for a multiplicative model. |

Implementation: [src/engine/forecast_bias.rs](../src/engine/forecast_bias.rs) (pure functions + 11 unit tests).

## Results-based tuning

The engine's per-zone parameters (texture, root depth, sprinkler rate, weekly budget) start as informed guesses. The tuning report closes the loop: it reads a window of recorded outcomes and emits at most one deterministic recommendation per zone. The user-facing walkthrough is [Tuning report](tuning-report.md); this section covers the machinery.

### How it works

Four checks run per zone over a 7 to 30 day window (default 14), each a pure function over persisted rows:

| check | reads | flags when |
|---|---|---|
| cap clamp | live run-duration math + run days in the window | the duration cap chronically trims the model's desired session |
| interval plausibility | soil catalog RAW / forward mean daily ETc | one filling of the root zone lasts under 1.5 or over 21 days |
| drying drift | probe series slope across dry stretches vs `mean_daily_etc / TAW` | the measured drying rate is outside 0.6x to 1.6x the modeled rate |
| rate backout | probe rise bracketing each watering event | the backed-out precipitation rate differs from the configured one by over 30% |

The install-wide forecast-skip scorecard builds on the existing accuracy scoreboard above (same `forecast_observations` rows, same WET/SIG thresholds as `assess_day`), extended with window-aware confirmation: a tomorrow-rain skip is judged against the NEXT day's observed total and a 3-day-rain skip against the following 3-day sum, where the accuracy scoreboard judges same-day only. Only forecast-driven skips enter the tally (rain expected within 4 hours, tomorrow rain, 3-day rain); reactive skips (rain now, observed rain, already wet) are triggered by rain that already happened, so scoring them against observed rain would be self-confirming, and they are counted on a separate unscored line instead.

### Where it surfaces

- **API:** `GET /api/v1/irrigation/tuning` returns the full report; `POST /api/v1/config/zones/apply` writes one recommendation through the validated config path.
- **Pure module:** `engine::tuning` holds every rule (slope estimator, event clustering, backout math, scorecard scoring, ranking) with unit tests; the store assembly is a thin layer above it.
- **UI:** the zone detail's Tuning panel and the irrigation page's strip.

### Defaults and bounds

| Constant | Value | Why |
|---|---|---|
| window | 7..=30 days, default 14 | Enough mornings to call a pattern chronic; short enough to track the season. |
| dry stretch | >= 48h, >= 2 stretches, >= 8 readings each | One quiet weekend is not a drying signal. |
| drift band | 0.6x to 1.6x | Probe percent is a relative scale; only a large, repeated ratio is trustworthy. |
| backout events | >= 3 clean events, median rate | A single rise can be rain residue or a probe artifact. |
| rate tolerance | 30% | Catalog rates are honest to roughly this band anyway. |
| scorecard minimum | 3 scored days | Below this the tally would be noise; the report says so instead. |

One recommendation per zone at most, ranked cap clamp > drying drift > rate backout > interval plausibility, and the backout check is only consulted when drift did not flag the zone in the same report. Counts with no data behind them are null on the wire, never zero.

Implementation: [src/engine/tuning.rs](../src/engine/tuning.rs) (pure rules + unit tests) and [src/tuning/mod.rs](../src/tuning/mod.rs) (store assembly).

## Where to read further

- [Grass species catalog](grass-species.md): 12 species with monthly Kc curves and citations
- [Soil texture catalog](soil-textures.md): USDA classes with FC, WP, AW, infiltration
- [Skip rules](skip-rules.md): every rule in the ladder with its config knob
- [Configuration reference](configuration.md): every `cfg.engine.*` field and its default
