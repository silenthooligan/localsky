# Zone water needs and duration

Open a zone's detail view to see its governing model, water need, planned minutes, and limiting factors.

## From depth to time

At its simplest:

```text
runtime in minutes = required gross depth in mm / application rate in mm/hour × 60
```

The gross depth accounts for the applicable capture efficiency. The final runtime also reflects configured adjustments, zone caps, restrictions, and scheduling capacity.

For example, 5 mm applied at 10 mm/hour needs 30 minutes before other limits. A wrong application rate produces a wrong duration even when the soil model is otherwise correct.

Measure the rate with catch cups where practical. A catalog head rate is a starting estimate.

## What sets the required depth?

| Model | Basis |
|---|---|
| Soil | The zone's reconstructed water need and refill plan |
| Weekly | Remaining target after credited rain and applied water, spread across eligible sessions |
| Explicit manual run | Requested duration, subject to the manual dispatch policy |

Check the model on the zone. Sessions per week affects weekly scheduling; it is not the soil model's primary trigger.

## Without a soil sensor

The deficit is modeled water missing from the root zone, not a measured moisture
percentage. LocalSky replays recent weather and watering history:

```text
deficit = previous deficit + plant water use - effective rain - retained irrigation
plant water use = reference ET × the plant's seasonal crop coefficient
retained irrigation = valve-open hours × application rate × retained fraction
```

The result stays between zero and the soil's available storage. Soil texture
and root depth set that storage; the plant's allowed depletion sets the watering
trigger. Uncertain starting conditions remain a range until the evidence resolves
them. Forecast rain and the next permitted watering opportunity shape the refill.

**Area does not multiply runtime.** Application rate is already a water depth
across the zone. **Sun/shade is saved but does not currently apply a separate
soil-demand adjustment.** A sensor can supply moisture checks; the modeled
deficit should still be read as an estimate.

A 10 mm/hour rotor with 70% retained water supplies only 7 mm to the roots in an
hour. A 38 mm/hour spray with the same setting supplies 26.6 mm in an hour.
A long capped run can therefore leave a rotor zone with a remaining deficit.
Check actual application rate with catch cups and verify soil and plant settings
before treating a modeled deficit as proof that the yard needs more water.

## Why minutes can be capped

A requested refill may exceed the zone's run limit, a restriction's cap, or the available watering window. The displayed plan reflects the limit.

Before raising a cap, check application rate, soil, roots, supply recovery, and available time. A longer run can create runoff or exceed a well's recovery capacity.

Cycle-and-soak divides valve-open time into segments and adds soak waits. Total elapsed time can therefore exceed the watering minutes.

## Read uncertain values

A missing deficit is not zero water need. The soil reconstruction may lack enough evidence or retain a range of possible starting conditions.

A probe percentage is not automatically an absolute measurement of plant-available water. Without calibration, LocalSky should not present a precise future percentage curve.

The separate no-watering soil outlook illustrates drying without future irrigation; the progressive water plan includes projected watering and rain. They answer different questions.

## Seasonal water budget

The engine's seasonal adjustment scales the plan before the final limits. At 100%, it leaves that adjustment unchanged. Check the effective result after changing it; a cap can prevent an increase from producing longer watering.

[Watering decisions](irrigation-engine.md) · [Weekly scheduling](water-budget.md) · [Tuning](tuning-report.md)
