# Move watering from Home Assistant

LocalSky can take over irrigation planning while HA remains your automation and dashboard platform. Move one responsibility at a time and keep the previous configuration for recovery.

## 1. Run LocalSky alongside HA

Install the server and connect weather sources. Use the [companion integration](hacs.md) if you want LocalSky entities in HA.

For evaluation, use a simulated controller or keep watering paused. A dry-run plan does not validate physical valve behavior.

## 2. Choose device ownership

| Responsibility | Options |
|---|---|
| Receive weather | LocalSky station adapter, or HA passthrough |
| Plan watering | LocalSky engine |
| Reach valves | Direct controller adapter, MQTT/HTTP, or HA service calls |
| Display and automate | LocalSky app, HA entities, or API clients |

If HA is the only system that can reach a valve, use a service-call controller. If the controller has a supported local API, LocalSky can talk to it directly.

## Keeping WeatherFlow in Home Assistant

Keep the existing HA WeatherFlow integration and add **HA passthrough** in LocalSky. Map the desired entities and select HA for those readings.

Disable or remove LocalSky's Tempest UDP source so its listener releases the port. For preceding-minute rainfall, choose **Rain last minute (accumulate today)**. Keep a forecast source enabled.

[Detailed HA weather instructions](hacs.md#use-home-assistant-weather-sensors)

## 3. Recreate the zone configuration

Check each zone's plants, soil, roots, application rate, maximum duration, and controller binding. Set the intended scheduling model.

Retired HA Smart Irrigation and Irrigation Unlimited helpers do not control the current LocalSky engine. Review LocalSky's saved settings rather than assuming a helper value is still authoritative.

Do not rename zone slugs to match controller names. Use **Controller station** to bind the existing LocalSky zone.

## 4. Review the plan

Compare the current decision, zone needs, forecast coverage, and timing. A difference from the previous scheduler needs an explanation, not automatic correction to match it.

Use the Daily log for recorded automatic decisions and the Run log for delivered watering. A future projection is not an acceptance test of the controller.

## 5. Transfer scheduling

1. Save a backup of LocalSky and the previous scheduling configuration.
2. Disable the old automatic schedules, including controller-native programs that would overlap.
3. Enable the intended LocalSky schedule.
4. Supervise a short run on a known zone and verify Stop.
5. Check the next normal run and its recorded outcome.

Keep a clear owner for automatic watering. Retaining an old integration for device access is different from leaving its scheduler active.

## When Home Assistant is unavailable

Native LocalSky devices do not need HA. HA passthrough sources and HA service-call controllers do depend on it. Missing required readings or unreachable valves can hold watering or fail dispatch.

The companion integration becoming unavailable does not itself mean LocalSky stopped running.

## Clean up afterward

Remove retired automations and helpers only after the new path is verified. If you choose the HACS companion instead of MQTT discovery, remove only LocalSky's affected retained topics and entities.

[Controller guide](controllers.md) · [Backups](backup-restore.md) · [Troubleshooting](troubleshooting.md)
