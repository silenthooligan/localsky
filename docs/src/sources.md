# Data sources

Data sources decide which reading comes from where. When more than one
source can report the same value, LocalSky picks a winner per reading, and
this is where you steer that. Edit it under Settings, then Devices, then
Data sources. Changes apply to the live engine on the next reading, with
no restart.

## Per-field priority and backup chain

Each headline reading (temperature, humidity, wind, rain, pressure,
solar/UV) has an **ordered chain** of sources. The top source that is
reporting now wins; if it goes quiet the next one takes over, so a reading
is never lost.

- A reading you have not touched shows the smart region default order,
  tagged **Automatic**.
- Drag a row, or use the up/down arrows, to make your own order, tagged
  **Custom**. **Reset to automatic** drops your custom order.
- Each row is badged with the honest nature of that source for that
  reading: *your device* and *measured* and *radar measured* are real
  measurements, *real-time* is a live analysis, and *model forecast* is a
  prediction. So the same cloud service can read "real-time" for
  temperature and "model forecast" for rain.
- A live marker shows which link is *reporting now*, which are on
  *standby*, the *backstop* at the end of the chain, and any that are
  *off*.

No weather hardware? A cloud weather service can supply any reading's
current value, so the chain is where you decide which service backs up
which, even with no local station.

## Forecast source

A separate picker chooses which service drives the whole forecast: the
daily and hourly outlook, the rain expected tomorrow, and the
evapotranspiration estimate the engine waters from. "Auto (follow the
chain)" keeps Open-Meteo (free, no key) as the low-priority failover;
pick a provider to pin it to win regardless of ranking. A pinned source
that goes offline still falls back, so a pin never blanks the forecast.

## What lives elsewhere

Soil moisture is governed per zone, not as a per-reading chain, so it is
bound in the [zone editor](zones.md) via each zone's soil sensor, not
here. The underlying config keys these controls write (`field_source_chains`,
`field_source_overrides`, and `forecast_provider`) are documented in the
[configuration reference](configuration.md#per-field-source-selection).

## Attribution

Installs using the Apple WeatherKit source display weather data provided
by **Apple Weather**, and Apple's terms require that attribution plus a
link to their legal page wherever the data is shown. LocalSky carries the
credit on the WeatherKit source card; the legal page is
[weatherkit.apple.com/legal-attribution.html](https://weatherkit.apple.com/legal-attribution.html).
