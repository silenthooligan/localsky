# Weekly water budget

LocalSky tracks how much water each zone has received over the trailing
week, from every counted source, against what the engine thinks the week
should deliver.

There is no budget screen yet. What you see of it in the app is the hold
line on a zone card and on zone detail, which names the gate that zeroed
the zone for the day. The full per-zone rows (target, applied, observed
rain, forecast credit, remainder, remaining sessions) are on the API at
`/api/v1/irrigation/snapshot` under `water_budgets`.

**Counted in:**

- Irrigation runs (recorded per zone, per second of runtime, converted
  through the zone's precipitation rate).
- Measured rainfall (from your station or gateway).

**The target** is a flat weekly depth per zone, in inches including rain.
It comes from the zone's `weekly_budget_in` setting, or, when you have not
set one, from a default by zone type: 1.00 in over 2 sessions for turf,
0.50 in over 1 session for a zone whose name marks it as shrub, garden or
bed. It does not move with the season, and nothing recomputes it from ET0
or the species coefficient. ET0, Kc and ETc are computed and displayed, and
the [tuning report](tuning-report.md) will suggest a different
`weekly_budget_in` when the zone cannot deliver the one it has, but the
target itself changes only when you change it.

**When a zone looks off plan:** persistently dry means runs are being
skipped or are too short, and the zone's "Why this duration?" panel shows
the throughput and whether the run hit its cap. Persistently soggy means
rain is doing the work and the engine should be skipping more, or the
precipitation rate is set too low.

This budget is what decides watering. Its per-zone remainder sizes each
session, and when the remainder reaches zero, or forecast rain is imminent,
or the session spacing has not elapsed, the zone plans zero seconds for the
day and says which of those it was. A zone in that state reads ON HOLD on
the zone card and detail with the reason beside it.

A per-zone soil depletion bucket is coming and would then govern both the
trigger and the size. It does not run today, and no setting turns it on.
