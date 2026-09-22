# Runs, skips, and history

History separates delivered watering from the decisions that scheduled or held it.

## Run log

Choose a date range or month, then search by zone or reason. **All Months** includes the available history across months.

Watering sessions are grouped by their start date in your installation's timezone. Expand a session to see its cycle-and-soak segments and original records. Automatic and manual runs retain their sources.

A session ID identifies related records. Older records without an ID remain separate; nearby timestamps alone do not establish that they belong together.

## Daily log

Use the Daily log to answer **what happened to the normal run that morning?** It includes recorded automatic watering and skipped mornings, with zone reasons.

A live hold shown in the app is not automatically a historical skip. The log needs a recorded outcome. When that evidence is absent, LocalSky shows no record.

## Watering insights

Choose the labeled 30-day, 90-day, or one-year reporting window.

| Measure | Meaning |
|---|---|
| Watering time | Valve-open time, excluding soak waits and duplicate observations |
| Watering sessions | Recorded watering events |
| Skipped zone mornings | Recorded automatic holds by zone and local day |
| Daily and zone breakdowns | Where and when recorded watering occurred |

Duration is not measured volume. Gallons require a supported, connected flow meter and valid readings. An empty day does not prove a skip or zero use.

## Rain forecast review

Expand **Rain forecast review** below the watering insights to compare past forecasts with observed rain. Only completed days with enough evidence are scored. Today's partial rainfall cannot establish whether a forecast was correct.

This comparison measures forecast outcomes, not water saved.

## Export and recovery

**Download CSV** exports stored records. **Print** creates a report. History lives in LocalSky's database and is included in a LocalSky backup.

A failed history request is shown as unavailable. It is not converted into an empty successful report.

[Backup and restore](backup-restore.md) · [History API](api-irrigation.md)
