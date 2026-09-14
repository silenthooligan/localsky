# Weekly water budget

LocalSky tracks how much water each zone has received over the trailing
week, from every counted source, against what the engine thinks the week
should deliver. Rain counts per day, and each day is capped at what the
zone's root zone can hold: rain beyond that in a single day drains past
the roots and never reaches the plant, so it does not count against the
week.

There is no budget screen yet. What you see of it in the app is the hold
line on a zone card and on zone detail, which names the gate that zeroed
the zone for the day. The full per-zone rows (target, applied, observed
rain, forecast credit, remainder, remaining sessions) are on the API at
`/api/v1/irrigation/snapshot` under `water_budgets`.

**Counted in:**

- Irrigation runs (recorded per zone, per second of runtime, converted
  through the zone's precipitation rate). Applied water counts in full;
  the per-day cap below is a rain rule only.
- Measured rainfall (from your station or gateway), credited per day:
  each day counts up to the zone's per-day cap, which is the root zone's
  own capacity (field capacity minus wilting point, times root depth)
  unless you set **Rain the soil can bank per day** in the zone editor.
  The forecast credit takes the same per-day cap.

**The target** is a flat weekly depth per zone, in inches including rain.
It comes from the zone's `weekly_budget_in` setting, or, when you have not
set one, from a default set by the species you declared on the zone: its
peak crop coefficient against a reference peak of 1.00, so warm-season
turf starts at 0.85 in over 2 sessions and established shrubs at 0.55 in
over 1. A zone with no zone config declares no species, so its default
comes from the zone's name instead: 0.50 in over 1 session when the slug
carries shrub, garden or bed, and 1.00 in over 2 otherwise. The target
does not move with the season, and nothing recomputes it from ET0
or the species coefficient. ET0, Kc and ETc are computed and displayed, and
the [tuning report](tuning-report.md) will suggest a different
`weekly_budget_in` when the zone cannot deliver the one it has, but the
target itself changes only when you change it.

## Sandy soil

The per-day cap is why a sandy yard no longer skips a whole week after
one storm. Sand at default turf roots (150 mm) holds about 0.35 inches;
a 1.2 inch storm day credits 0.35 inches and the rest drains past the
roots, so the balance resumes watering mid-week instead of counting
water the lawn never kept. A loam yard holds 0.89 inches a day and only
notices the cap in storms bigger than that.

Derived caps at default turf roots (150 mm), per soil texture:

| Texture | Cap (mm/day) | Cap (in/day) |
|---|---|---|
| Sand | 9.0 | 0.35 |
| Loamy sand | 12.0 | 0.47 |
| Sandy loam | 19.5 | 0.77 |
| Loam | 22.5 | 0.89 |
| Silt loam | 25.5 | 1.00 |
| Clay loam | 24.0 | 0.94 |
| Clay | 21.0 | 0.83 |

A deeper root depth (a species default or the zone's own override)
raises the cap proportionally. Set **Rain the soil can bank per day** in
the zone editor to override the derived value; the field's placeholder
shows the number in effect. The cap does not decay older rain by ET:
the weekly target already accounts for typical ET, and decaying the
credit would count it twice.

**When a zone looks off plan:** persistently dry means runs are being
skipped or are too short, and the zone's "Why this duration?" panel shows
the throughput and whether the run hit its cap. Persistently soggy means
rain is doing the work and the engine should be skipping more, or the
precipitation rate is set too low.

This budget is what decides watering for weekly-governed zones. Its
per-zone remainder sizes each session, and when the remainder reaches zero,
or forecast rain is imminent, or the session spacing has not elapsed, the
zone plans zero seconds for the day and says which of those it was. A zone
in that state reads ON HOLD on the zone card and detail with the reason
beside it.

The session length that remainder buys is not the last word. The seasonal
dial, `engine.seasonal_adjust_pct`, scales it before the zone's maximum run
time clamps the result, so the planned minutes you see already carry the
dial. At 100% you get the depth this ledger computed, and you turn the dial
down for a wet stretch or up for a heat wave instead of re-editing every
weekly target; the full treatment is under
[the seasonal water budget](irrigation-engine.md#seasonal-water-budget).

The [soil model](irrigation-engine.md#the-soil-model) is the other
scheduling model, and the shipped default: a zone it governs waters when
its own soil deficit crosses the trigger and each run refills the deficit,
so both the trigger and the size come from the soil instead of this weekly
ledger. `engine.scheduling_model` picks the default and the zone editor
pins it per zone, so a weekly-governed zone is one you pinned to weekly,
one on an install whose `engine.scheduling_model` is weekly that carries
no per-zone pin, or one with no zone config at all: an env-var install, or
a controller zone you never configured in LocalSky. That last kind has no
texture and no species to size a bucket from, so it stays on this ledger
whatever the engine default says. Under the soil model, a Weekly target
you set by hand stays honored as a rolling-7-day delivery ceiling, and
Sessions per week stops steering because cadence
follows soil texture and roots. The soil deficit computes for a zone that
has a zone config, whichever model governs it, once its trailing window
carries at least three evidenced days (a resolved ET0 day, a rain day, or
a completed run); below that the zone publishes no soil fields and the
weekly ledger sizes it, which is where a fresh install starts. A zone
with no zone config gets no soil fields at all, and no zone gets them on
a tick where the runs read failed. The zone detail's Soil model block
shows what it plans.
