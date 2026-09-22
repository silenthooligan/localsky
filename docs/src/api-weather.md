# Weather and forecast API

All paths below use `/api/v1`. Responses use field-named units, regardless of display preferences.

## Current weather

**GET /snapshot** returns the current weather snapshot. Its name predates the source bus; readings can come from sources other than Tempest.

The legacy numeric fields require their validity context. Check `air_temp_live_epoch`, `wind_live_epoch`, `rh_live_epoch`, and `rain_live_epoch` where applicable. `last_packet_epoch` alone does not establish freshness for every field.

The irrigation snapshot also exposes `current_weather` for selected temperature, humidity, and wind inputs. Each available sample includes its source, observation time, maximum age, whether it is measured, and the selection reason.

**GET /stream** sends weather snapshot events. [SSE guide](api-streams.md).

## Selected forecast

**GET /forecast/snapshot** returns the selected forecast:

| Field | Meaning |
|---|---|
| `last_refresh_epoch` | Original successful forecast fetch time |
| `source_label` | Selected provider |
| `source_reachable` | Provider reachability state |
| `source_is_backup` | Whether a backup is serving |
| `timezone` | Forecast calendar timezone |
| `daily`, `past_daily`, `hourly` | Available entries |

Critical temperature, humidity, wind, rain, and probability fields can be null. Extended advisory fields have their own validity rules; do not treat every legacy zero as a measurement.

**GET /forecast/stream** sends forecast snapshots. **GET /forecast/bias** returns the learned forecast bias when enough observations exist.

## Forecast windows

**GET /forecast/window?track=merged&from=EPOCH&to=EPOCH**

| Parameter | Requirement |
|---|---|
| `track` | Defaults to `merged`; otherwise a configured extra-model ID |
| `from`, `to` | Ordered UTC epoch seconds |
| Range | At most 48 hours between the bounds |

Both hourly timestamps are included. Rain at timestamp T covers **[T, T + 1 hour)**. To read three hours beginning at 13:00, select the timestamps 13:00 through 15:00.

The response includes provider/model identity, `fetched_at`, `age_s`, range, `hours`, `expected_hours`, `complete`, and these coverage counts:

- `precipitation_hours`
- `probability_hours`
- `temperature_hours`

Each `hourly` row has `time_epoch`, nullable `temp_f`, nullable `precip_in`, and nullable `precip_probability`.

Summaries are `pop_max_pct`, `precip_max_in`, `precip_sum_in`, `temp_max_f`, and `temp_min_f`. Each summary is null if its required measurements or hourly coverage are incomplete.

A known track with no data returns 200 with zero hours and unavailable summaries. An unknown track returns 404. Invalid ranges return 400. Cached forecasts retain their original age.

## Extra models

**GET /forecast/tracks** lists configured extra models, fetch age, serving tier, and errors. Up to four extra models can be configured.

These tracks serve queries and comparisons. They do not replace current observations or the irrigation forecast. `merged` identifies the forecast selected for the main app.

[Configure forecast sources](forecast.md)

## Forecast archive

**GET /forecast/archive?track=merged&from=EPOCH&to=EPOCH**

Returns `{"rows": [...], "next_cursor": null}`. Rows identify `track`, `provider`, nullable `model`, `target_epoch`, `lead_h`, nullable `pop_pct`, nullable `precip_in`, and `fetched_at`.

| Parameter | Behavior |
|---|---|
| `track` | Defaults to `merged` |
| `from`, `to` | Inclusive target-hour bounds; at most 400 days |
| `lead_h` | Optional, 0 through 47 |
| `limit` | Default 1,000; range 1 through 5,000 |
| `cursor` | Returned cursor; keep the other query parameters unchanged |

Rows are ordered by target hour and fetch time. With `Accept: text/csv`, missing numbers are blank and the next cursor is in `X-Next-Cursor`.

The archive retains received forecast issuances for 400 days. It cannot reconstruct forecasts from before recording began. Forecast rows are never measured rainfall.

[Quick start](api-quickstart.md) · [History and irrigation](api-irrigation.md)
