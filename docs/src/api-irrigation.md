# Irrigation and history API

Use snapshots for current state and projections. Use history for recorded outcomes. All routes below are under `/api/v1/irrigation`.

## Current state

**GET /snapshot** returns the irrigation snapshot. **GET /stream** sends the same shape as snapshot events.

| Field | Use |
|---|---|
| `last_refresh_epoch` | Snapshot assembly time |
| `timezone` | Local calendar used for planning |
| `current_weather` | Selected inputs with source, observation time, and freshness limit |
| `zones` | Zone identity, planned runtime, reported state, and available soil data |
| `zone_verdicts` | Current zone decisions |
| `decision_trace` | Compared rules, reasons, and evidence |
| `water_budgets` | Zone allocation and soil-model details |
| `water_plan` | Progressive daily projections |
| `restart_required`, `restart_reasons` | Saved changes that hold watering until restart |

A zone's `running` value must be read with `running_known`. `ledger_running` records LocalSky's outstanding run tracking; it is not a substitute for controller confirmation. Nullable flow and soil fields remain unknown when unavailable.

## Future plan

Each `water_plan` day contains its local date, day offset, optional start/finish times, forecast rain, expected rain, probability, evidence completeness, and zones.

Each zone names its planned seconds, reason code, reason, water need, model, and available depletion, trigger, capacity, and demand values.

`next_run_state` can be `at`, `no_water_planned`, `no_legal_day`, `no_sunrise`, or `no_location`. Check the state before interpreting `next_run_epoch`. A zero epoch is not a scheduled run.

These are projections. They do not prove that a valve was commanded or that water was delivered.

## Runs and daily outcomes

**GET /history?days=30**

Returns `from_epoch`, `to_epoch`, `runs`, and `daily`. The default range is 30 days. `days=0` requests all retained records; positive ranges are bounded to 36,500 days.

Run records include:

- `zone`, `start_epoch`, `duration_s`
- `source`, `status`, nullable `skip_reason`
- nullable `session_id`, `controller_id`, `note`
- nullable `applied_mm`, `volume_gal`
- nullable `cycle_index`, `cycle_count`

Use session IDs to group related records. For applied-water totals, use the union of valve-open intervals per zone; overlapping command and observer records can otherwise count the same water twice. Dry-run rows are not delivered water.

Daily entries contain `date_local`, `epoch`, `kind`, and `zones`. Zone entries provide planned seconds and the recorded reason. Kinds include `scheduled`, `scheduled_legacy`, `missed_window`, and `recorded_decision`.

Daily planning evidence and actual run records have different meanings. A generic historical decision does not prove that a scheduled valve dispatch was skipped.

**GET /decisions?days=30** returns verdict transitions. Its range clamps to 1–365 days. A transition is not the daily watering journal.

## Export and review

| Endpoint | Result |
|---|---|
| `GET /export?days=365&format=csv` | Downloadable run/skip export; JSON also supported |
| `GET /accuracy?days=30` | Completed-day forecast/observed comparison |
| `GET /tuning?days=14` | Zone tuning suggestions and evidence |
| `GET /explanation` | Optional advisor explanation |
| `GET /anomalies` | Optional advisory checks |

Export ranges clamp to 1–3,650 days. Accuracy uses 1–365 days and does not score incomplete current days. Tuning uses 7–30 days.

History-dependent routes require persistent storage. An unavailable or failed history read must not be treated as an empty successful report.

## Send a command

**POST /action** accepts JSON selected by `kind`.

| Kind | Other fields |
|---|---|
| `run` | `zone`, `seconds` |
| `stop` | `zone` |
| `stop_all` | None |
| `set_pause_until` | `epoch`; zero clears |
| `clear_pause_until` | None |
| `toggle` | `key`: `irrigation_pause` or `irrigation_dry_run`; `on`: boolean |
| `set_global_override` | `mode`: `auto`, `skip`, or `run` |
| `set_zone_override` | `zone`; `mode`: `auto`, `skip`, or `run` |
| `set_override_tomorrow` | `mode`: `none`, `skip`, or `run` |
| `set_threshold` | `key`, `value` |

Use the LocalSky zone slug, not a guessed controller name. For example:

```json
{"kind": "stop", "zone": "front_lawn"}
```

Explicit run durations have a defensive ceiling of 7,200 seconds and remain subject to the applicable dispatch policy. A Force choice does not bypass every protection. See [rules and thresholds](skip-rules.md).

Threshold writes accept `max_wind_mph` (0–50), `min_temp_f` (20–70), and `rain_skip_in` (0–10). Controls are stored in LocalSky; retired HA helpers are not the control store.

Successful dispatch responses include `ok`, the dispatch target, and, where available, `confirm_within_s`. Check subsequent reported state. If a request times out, inspect state before retrying a run.

## Command failures

| Code | Meaning |
|---|---|
| `zone_unknown` | Zone binding cannot be resolved |
| `controller_auth_failed` | Controller credential rejected; distinct from LocalSky API authentication |
| `controller_rate_limited` | Controller or vendor throttled the request |
| `controller_unsupported` | Operation unsupported |
| `controller_unreachable` | Controller connection or upstream operation failed |

Preserve the response status, code, diagnostic, and request ID. Additional policy failures can hold a run. Do not translate every refusal into a retry.

**POST /simulate** evaluates a what-if scenario without dispatch. Tuning dismissals use **POST /tuning/dismiss** and **POST /tuning/undismiss**. The retired shadow routes report disabled; `run_sequence_now` is no longer an accepted action.

[Error handling](api-errors.md) · [Live streams](api-streams.md) · [History guide](history.md)
