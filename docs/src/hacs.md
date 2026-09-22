# Connect Home Assistant

The LocalSky companion integration adds your server's weather, sensors, valves, and actions to Home Assistant. It uses REST for setup and live SSE streams for updates.

You need a running LocalSky server. Install it [with Docker](getting-started.md) or as a [Home Assistant OS app](home-assistant-app.md).

## Install and pair

1. In HACS, search for **LocalSky**, install it, and restart Home Assistant.
2. Open **Settings → Devices & services**.
3. Add the discovered LocalSky instance, or choose **Add integration → LocalSky** and enter its address and port.
4. If authentication is required, create an API token under **LocalSky → Settings → Account** and enter it in the pairing flow.

[![Open LocalSky in HACS](https://my.home-assistant.io/badges/hacs_repository.svg)](https://my.home-assistant.io/redirect/hacs_repository/?owner=silenthooligan&repository=localsky-ha&category=integration)

The companion requires HA 2024.11 or newer and LocalSky API 1.12.0 through 2.x. Use matching server and companion releases when updating.

Discovery uses mDNS. If it cannot cross a subnet or container network, pair manually with a reachable address. LocalSky verifies the server and instance identity before adopting a discovered address change.

## Entities

| Group | What it provides |
|---|---|
| Weather | Current conditions and daily forecasts |
| Station | Available temperature, humidity, wind, rain, pressure, solar, and lightning readings |
| Irrigation | Decision and reason, pause control, and supported threshold controls |
| Zones | Valve controls, planned watering, and available soil readings |

The server's entity manifest determines what is available. A missing measurement remains unavailable rather than becoming zero. Multiple LocalSky instances can be paired separately.

## Watering actions

The integration provides `localsky.run_zone`, `stop_zone`, `stop_all`, `pause`, `resume`, `set_override`, and `set_zone_override`.

Choose actions and their fields in **Developer tools → Actions**. Runs and overrides use LocalSky's control path. A successful request does not by itself prove the valve is open or closed; check reported zone state.

An override does not remove all protections. Owner holds, unavailable required data, restrictions, and applicable safety checks can still prevent watering. [Rules and thresholds](skip-rules.md).

## Forecast-window action

`localsky.get_forecast_window` requires server API **2.3.0 or newer**. It returns a selected forecast over an interval:

```yaml
action: localsky.get_forecast_window
data:
  track: merged
  start: "{{ now().replace(minute=0, second=0, microsecond=0).isoformat() }}"
  end: "{{ (now().replace(minute=0, second=0, microsecond=0) + timedelta(hours=2)).isoformat() }}"
response_variable: forecast
```

Both timestamps are included. This example selects three hourly rows; each row's rainfall covers the following hour. Use `entry_id` when multiple instances are loaded.

`merged` is LocalSky's selected forecast. A configured extra model ID queries that model instead.

Before an automation uses the result:

- Check `complete` for hourly coverage.
- Check `age_s` against a freshness limit appropriate to the automation.
- Check the required summary values for null. Complete timestamps do not guarantee every measurement is present.
- Use the returned units: inches and Fahrenheit, regardless of app display settings.

[Forecast-window API](api-weather.md#forecast-windows)

## Use Home Assistant weather sensors

This is the reverse direction: **HA → LocalSky**.

In **LocalSky → Settings → Devices**, add **HA passthrough**, provide the HA connection, and map the desired entities. Select HA in the source chain for those readings. The HAOS app can use its Supervisor connection.

For an existing HA WeatherFlow setup:

1. Disable or remove LocalSky's **Tempest UDP** source to release its listener.
2. Map the WeatherFlow sensor entities through HA passthrough.
3. For preceding-minute precipitation, choose **Rain last minute (accumulate today)**. Use the daily-total mapping only for an actual daily-total sensor.
4. Keep a forecast provider enabled.
5. Check the source, values, and original observation times in LocalSky.

The minute-rain accumulator restores recorded totals after restart but cannot reconstruct minutes missed while offline. Polling an old HA state does not make the measurement fresh.

## Connection problems

| Symptom | Check |
|---|---|
| Not discovered | Pair manually; confirm HA can reach the server address. |
| Reauthentication requested | Create a replacement LocalSky API token and complete HA's reauth flow. |
| Login page instead of API JSON | Check the proxy route and authentication arrangement. |
| Duplicate MQTT and companion entities | Choose the publishing path you want. Remove only the affected LocalSky entities or retained topics. |
| Bulk HA read returns 500 | In 0.9.2, LocalSky attempts individual mapped-entity reads. The HA/proxy log is needed to diagnose the original server error. |

Never clear the broker's entire discovery tree to remove LocalSky duplicates; it can remove other integrations' retained discovery messages.

If HA is unavailable, native LocalSky devices can continue independently. Sources or controllers that rely on HA remain dependent on it, and missing required readings can hold watering.

[Companion repository](https://github.com/silenthooligan/localsky-ha) · [Troubleshooting](troubleshooting.md) · [API guide](developers.md)
