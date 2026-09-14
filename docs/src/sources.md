# Data sources

Data sources decide which reading comes from where. When more than one
source can report the same value, LocalSky picks a winner per reading, and
this is where you steer that. Edit it under Settings, then Devices, in
the section titled **Which source provides each reading**. Changes apply
to the live engine on the next reading, with no restart.

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

Each source also carries one number, **Default rank (advanced)**, on the
Behavior panel of the source editor. You normally never touch it, because
the chain above is the real control: for a reading you have put in your
own order, that order is the priority.

The rank still decides two things. It sets the **Automatic** order for a
reading you have not reordered, which is that reading's sources sorted by
rank, highest first, so the order on screen is the order LocalSky
arbitrates by. And it is the backstop when every source in a custom chain
has gone quiet: rather than blank the reading, LocalSky falls back to
comparing ranks. There is one exception. While a stale chain's last owner
was a cloud service and some cloud is still reporting that reading, a
local station is held off until the cloud tier is exhausted, so the
reading does not flip tiers partway through an outage. That second case
is why the number is worth leaving sane even if you never set one by
hand.

The box is a whole-number field. The arrows and the browser's own hint
stay inside -100 to 200, but a value outside that range is accepted and
saved, so treat it as a guide rather than a limit. Anything you add in
the source editor starts at the schema's default of 50 and stays there,
a LAN station and a gateway you adopt from a network scan included. A
cloud service is re-ranked once, on its first save, to the researched
default for your region, so the regional authority sits above the
keyless backstop without you arranging it. The per-region numbers are in
the [configuration reference](configuration.md#sources).

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
here. Sources whose data arrives on its own instead of being polled for
(the Ecowitt LAN push receiver, an HTTP webhook, an MQTT subscription)
go through the same source editor as every other kind, reached either
from **Add a data source** on the [Sensors page](sensors.md) or from the
**Weather source** button under **Add a device** in Settings, then
Devices. The Sensors page shows a receiver's readings the moment they
land, so that is where you confirm the wiring worked. The underlying
config keys these controls write (`field_source_chains`,
`field_source_overrides`, and `forecast_provider`) are documented in the
[configuration reference](configuration.md#per-field-source-selection).

## Attribution

Installs using the Apple WeatherKit source display weather data provided
by **Apple Weather**, and Apple's terms require that attribution plus a
link to their legal page wherever the data is shown. LocalSky carries the
credit on the WeatherKit source card; the legal page is
[weatherkit.apple.com/legal-attribution.html](https://weatherkit.apple.com/legal-attribution.html).
