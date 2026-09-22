# Manual schedules

Use a manual schedule when a zone needs a chosen start time and duration. Configure it under **Settings → Manual schedules**.

Each schedule targets one zone, selected weekdays, a local start time, and a duration. Schedule changes are read while LocalSky runs; check that the save succeeded.

## Choose a mode

| Mode | Effect on smart scheduling |
|---|---|
| Override | Suppresses the zone's smart plan on days covered by the enabled schedule |
| Floor | Keeps the fixed schedule and allows additional smart watering when eligible |

These modes describe how the schedules coexist. They do not guarantee watering through a hold.

Floor watering counts toward applied irrigation. Under weekly scheduling it also resets session spacing, so a frequent Floor schedule may leave no need or eligible day for a smart run.

To let LocalSky choose the zone's timing and amount, disable the manual schedule and review the automatic plan.

## Days, time, and limits

Times use the installation's timezone. A schedule with no selected weekdays never fires. Multiple schedules can target the same zone, but any enabled Override schedule for that day suppresses the zone's smart plan.

Run limits and applicable restriction caps still apply. The accepted duration can be shorter than the requested duration.

## Weather and protected holds

By default, manual schedules are evaluated against the applicable weather and control policy.

The explicit **Ignores weather** option can waive supported weather checks, including missing live-weather evidence. The saved schedule identifies that choice, and dispatch records bypassed gates.

The waiver does not clear owner holds, watering restrictions, the restart hold, or the affected zone's missing/untrusted configured probe hold. Review this option as a deliberate change in behavior.

## Before enabling

Confirm the zone binding and remove overlapping schedules in the controller or other software. Check the intended weekdays and timezone, then supervise an initial run.

Use the Daily and Run logs to see what was scheduled, held, or delivered. A failed dispatch is not a completed watering.

[Restrictions](restrictions.md) · [Decision rules](skip-rules.md) · [History](history.md)
