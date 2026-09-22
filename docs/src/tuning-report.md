# Review tuning suggestions

LocalSky reviews recent runs, rain, and available probe readings to suggest zone adjustments. Suggestions use recorded evidence and deterministic calculations. They do not change settings until you apply them.

Open a zone's **Tuning** panel to inspect a suggestion and its evidence. An available report can also appear as a notification or link from irrigation.

## What a suggestion means

| Finding | What to check |
|---|---|
| Runs repeatedly reach a limit | Application rate, demand, permitted watering window, and delivery capacity. |
| Configured soil storage looks implausible | Actual soil texture and any root-depth override. |
| Probe drying differs from the model | Calibration, probe placement, root depth, and soil assumptions. |
| Estimated application rate differs from configuration | Verify with a catch-cup test or appropriate flow measurement. |

A probe samples one location. Its response can support a diagnosis, but an inferred rate is not a substitute for measuring sprinkler distribution.

## Scheduling model matters

For a weekly plan, available sessions and the weekly target affect allocation. Under the soil model, a refill responds to depletion; changing weekly session count does not solve a capped refill. An explicit weekly delivery ceiling or a restriction can impose another limit.

Do not change the soil type or reduce a plant's stated need merely to make a capacity warning disappear. A system that cannot deliver enough within its permitted window may need different hardware, scheduling, or planting.

## Apply and verify

Read the proposed value and supporting period, check it against the physical setup, and apply it only if it fits. Review subsequent runs and zone condition. Dismiss suggestions that do not represent the site.

[Zone settings](zones.md) · [Watering logic](irrigation-engine.md) · [History](history.md)
