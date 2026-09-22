# How watering decisions work

LocalSky evaluates each zone's water need, the available weather evidence, and the controls that apply before planning a run. Open **Irrigation → Watering decisions** to see the result and its reasons.

## The decision in five steps

1. **Read the evidence.** Select usable weather inputs and retain their sources and observation times.
2. **Estimate zone demand.** Combine the zone's plants, soil, roots, recent rain, and completed watering.
3. **Look ahead.** Evaluate forecast rain and whether the zone can wait until a later watering opportunity.
4. **Apply controls.** Check holds, restrictions, required data, weather protections, and zone limits.
5. **Fit the run.** Convert the required depth to time, split cycles where needed, and fit eligible zones into the watering window.

The app, scheduler, and control path use the shared decision logic. The optional AI advisor explains results; it does not choose the watering.

## The soil model

The soil model is the default for new installations. Existing installations can retain their saved model, and each zone can override the engine default.

The model tracks **depletion**: water used from the root zone. Evapotranspiration increases depletion. Effective rain and delivered irrigation reduce it. Water beyond the modeled storage capacity cannot remain available to the plant indefinitely.

The soil and plant catalogs provide starting values for:

- **Total available water (TAW):** storage between field capacity and wilting point.
- **Readily available water (RAW):** the depletion threshold used to trigger irrigation.
- Root depth, crop coefficients, and infiltration behavior.

LocalSky reconstructs the balance from up to 14 days of recorded evidence. It tests wet and dry starting states. Until the history sufficiently constrains the result, it retains an uncertainty range rather than publishing a precise deficit.

The fallback shown for a zone depends on its available evidence and configuration. A new installation must not be interpreted as having a measured full or empty soil bucket.

## Recent rain and future rain

**Observed rain** belongs to the historical water balance. **Forecast rain** belongs to the future scenario. They are kept separate.

A large storm is limited by the zone's storage and capture characteristics. Sandy soil and a shallow root zone can retain less water than a deeper, higher-capacity zone. The same rain total therefore need not produce the same watering interval everywhere.

When rain is expected, the planner evaluates whether waiting can meet the zone's need without an unacceptable dry period. Probability and coverage matter. Repeated forecasts that fail to deliver do not supply observed water.

The progressive plan carries earlier projected rain, demand, and watering into later days. Tomorrow is not assessed from an empty history.

## Weather demand

Reference evapotranspiration, **ETâ‚€**, estimates atmospheric demand. LocalSky uses FAO-56 Penman–Monteith calculations where inputs support them, with the implemented alternatives for reduced data. Crop coefficients adapt reference demand to the configured plants and season.

FAO-56 defines the reference method and the root-zone water-balance framework. LocalSky's scheduling rules, limits, and defaults are its implementation of those methods; the application itself is not a peer-reviewed field trial. [FAO reference ET](https://www.fao.org/4/x0490e/x0490e06.htm), [FAO soil water balance](https://www.fao.org/4/x0490e/x0490e0e.htm).

## Weekly scheduling

The alternative weekly model works toward a configured water target, crediting rain and applied irrigation and splitting the remainder across eligible sessions.

A soil-governed zone uses its soil need for cadence. An explicitly configured weekly target remains a delivery ceiling. Check the model shown for the zone before changing sessions per week.

[Weekly scheduling details](water-budget.md)

## Holds and overrides

Owner pauses, restrictions, unavailable required evidence, and enabled protections can hold watering. A configured missing or untrusted soil probe holds the affected zone. Automatic plans require complete next-24-hour rain evidence.

Force can bypass specified recommendations; it cannot make unavailable planning data valid or clear every hold. Manual schedules have their own explicit weather-waiver option and remain subject to protected controls.

[Rules and thresholds](skip-rules.md) · [Manual schedules](schedules.md)

## Timing and capacity

Normal watering works backward from sunrise minus 15 minutes, including cycle-and-soak time. Eligible freeze conditions can move the run to a later safe window.

A zone's run cap, watering restrictions, supply policy, and the available window limit what can be delivered. The plan reports those limits. If the requested water does not fit, changing a label or increasing a weekly target does not increase the system's physical capacity.

[Duration math](zone-math.md) · [Tuning suggestions](tuning-report.md)

## Check the outcome

Use **Today** and the Daily log for recorded automatic outcomes. Use the Run log for actual watering records. Later rain alone cannot establish that an earlier decision was wrong; evaluate the evidence that was available at dispatch.

[History](history.md) · [Plant catalog](grass-species.md) · [Soil catalog](soil-textures.md)
