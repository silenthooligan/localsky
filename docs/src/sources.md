# Choose reading sources

Open **Settings → Devices → Which source provides each reading** to choose where temperature, wind, rain, and other readings come from.

A source can be useful for one field and unsuitable for another. For example, a nearby measured wind observation and a modeled wind estimate are different kinds of evidence.

## Set the order

Each reading has an ordered source chain. Use the automatic order or move sources into a custom order. **Reset to automatic** restores the default selection.

LocalSky evaluates available, fresh readings and reports the selected source. Backups can take over when a preferred source stops supplying usable data. If no suitable source remains, the value is unavailable.

Check the source's capability label and timestamp, not only its brand name. Forecast, modeled, radar-derived, and measured data are identified separately.

## Understand status

| Status | Meaning |
|---|---|
| Selected / reporting | Supplying the current reading |
| Standby | Available but another source is preferred |
| Stale or unreachable | No longer supplying usable current evidence |
| Disabled | Turned off in configuration |

A successful network poll does not necessarily mean a sensor has a fresh observation. HA passthrough preserves the entity's original report time.

Default rank is an advanced setting used by arbitration. Prefer the visible per-reading chain for routine changes.

## Choose the main forecast

The forecast picker controls the selected daily and hourly forecast. That forecast also supplies irrigation planning evidence. A preferred provider can fall back to another usable provider.

Extra forecast models serve separate comparisons and queries. They do not replace the main forecast or station observations. [Forecast sources](forecast.md).

## Rain needs the right meaning

Match the sensor's measurement interval to its mapping. A preceding-minute rainfall amount is not a daily total. HA WeatherFlow users should select **Rain last minute (accumulate today)** for that sensor.

Forecast rain remains expected rain. It is not added to the observed rainfall history.

## Soil readings

Soil probes bind to individual zones. Configure them in the zone editor rather than a global weather chain. [Soil probes](soil-sensors.md).

[Provider capabilities](provider-matrix.md) · [Device setup](devices.md) · [Configuration keys](configuration.md#per-field-source-selection)
