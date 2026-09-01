# Soil sensors

Wire a moisture probe to a zone and the engine gets a measured gate: a
saturated zone skips on its own, and a measured-dry zone can override a
soft forecast-rain skip.

**Supported paths in:**

- Ecowitt soil probes (WH51 and friends) via a LAN gateway poll: native,
  no cloud, includes temperature, conductivity, and battery per probe.
- Any Home Assistant soil sensor entity, through an HA bridge source.
- MQTT topics and HTTP webhooks for DIY probes.

**Assignment** happens in the zone's settings (Settings > Zones > pick
the zone > soil sensor). One probe per zone; the Sensors hub shows
which zones each source feeds.

**How the engine uses it:**

- Below the zone's target band: the zone is eligible. Run length is
  unchanged; it comes from the weekly water balance.
- Inside the band: healthy; scheduled runs still apply unless the
  saturation threshold says otherwise.
- At or above saturation: the zone skips on its own, even when the day's
  verdict is Run, and the skip reason names the probe.
- If a probe goes offline, or reads as a wild outlier versus its
  neighbours, it is flagged as an anomaly on the irrigation and zones
  views.

The Sensors hub and each zone's detail show the probe's live reading,
the target band, and a 7-day no-watering projection so you can sanity
check that the moisture curve actually behaves like your yard.

A probe also unlocks the [tuning report](tuning-report.md)'s two
calibration checks: the drying-drift check (does your soil dry at the
rate the configured texture and root depth predict?) and the
sprinkler-rate backout (what rate do your heads actually deliver, per
the probe's rise across waterings?). Both need LocalSky's own recorded
probe history, so they work for probes connected through a source
(the Ecowitt gateway poll, MQTT, webhooks); a zone bound to a live Home
Assistant entity has no local history and reports that state honestly.
