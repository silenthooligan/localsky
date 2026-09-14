# Soil sensors

Wire a moisture probe to a zone and the engine gets a measured gate: a
saturated zone skips on its own, and a measured-dry zone can override a
soft forecast-rain skip.

**Supported paths in:**

- Ecowitt soil probes (WH51 and friends) via a LAN gateway poll: native,
  no cloud, with moisture and battery for every probe. Soil temperature
  and conductivity come only from the newer EC probes; a WH51 reports
  neither.
- Any Home Assistant soil sensor entity, through an HA bridge source.
- MQTT topics and HTTP webhooks for DIY probes.

**Assignment** is one step or two, depending on how the reading gets in.

- An Ecowitt gateway channel or a Home Assistant entity is ready to
  assign as soon as the source reports. Open Settings > Zones, pick the
  zone, and choose the probe in its **Soil moisture sensor** dropdown.
  One picker lists both kinds, so there is no separate Home Assistant
  list. The probe's card under Settings > Sensors or Settings > Devices
  binds the same zone, if you are already looking at it.
- An MQTT subscription, an HTTP webhook, or any other source whose
  readings you map by hand needs the zone bound on the source first. In
  the source editor, set that subscription's or mapping's **Bind to
  zone**. That control is on MQTT soil subscriptions, on HTTP webhook
  and REST poller field mappings, and on YoLink and Tuya device
  mappings. Prometheus and InfluxDB take the same binding as a
  `zone_slug` on the query in the source's JSON, and Davis WLL and the
  Home Assistant passthrough take a `soil_zone_map` on the source
  config. The value is then recorded as that zone's own soil channel
  instead of as a global reading, which is the whole point of the field:
  an unbound MQTT soil subscription publishes as humidity and is merged
  into the general humidity reading, and a webhook or polled-API mapping
  has no soil option at all until a zone is bound. Save, then open the
  zone and pick that channel as its **Soil moisture sensor**. Both
  halves are required; binding the source alone does not gate the zone.

A zone-bound channel appears in the zone's dropdown only after the
source has published at least once, because that list is built from the
readings LocalSky has recorded. One probe per zone is structural: a zone
holds a single soil sensor. One zone per probe holds only when you bind
from the probe's card under Settings > Sensors or Settings > Devices,
which releases the probe from whatever zone had it. The zone editor
writes the zone you are editing and nothing else, so picking the same
probe there leaves the earlier zone's binding in place. The Sensors hub
shows which zones each source feeds, and the step-by-step walkthrough is
in
[Add your first soil sensor](first-soil-sensor.md#binding-a-probe-to-a-zone).

**How the engine uses it:**

- Below the zone's target band: the zone is eligible. Run length is
  unchanged; it comes from the weekly water balance.
- Inside the band: healthy; scheduled runs still apply unless the
  saturation threshold says otherwise.
- At or above saturation: the zone skips on its own, even when the day's
  verdict is Run, and the skip reason gives the measured percent and the
  saturation threshold it crossed.
- A probe that goes offline is flagged as an anomaly on the irrigation
  and zones views, but only for a zone bound to a `source:` channel (an
  Ecowitt gateway, MQTT, a webhook, an entry in the Home Assistant
  passthrough's `soil_zone_map`) whose last reading above zero is more
  than 24 hours old. A zone bound to a Home Assistant entity through the
  HA bridge is never flagged offline, because there is no local history
  to tell a flatline from a blip.
- A probe that reads as a wild outlier versus its neighbors is flagged
  the same way, once three or more zones are reporting a reading and the
  zone sits further from the yard median than the outlier threshold, 35
  percentage points by default. On a two-probe yard nothing is judged an
  outlier.

The Sensors hub and each zone's detail show the probe's live reading,
the target band, and a 7-day no-watering projection so you can sanity
check that the moisture curve actually behaves like your yard.

A probe also unlocks the [tuning report](tuning-report.md)'s two
calibration checks: the drying-drift check (does your soil dry at the
rate the configured texture and root depth predict?) and the
sprinkler-rate backout (what rate do your heads actually deliver, per
the probe's rise across waterings?). Both need LocalSky's own recorded
probe history, so they work for a zone bound to a `source:` channel (the
Ecowitt gateway poll, MQTT, webhooks, the Home Assistant passthrough's
`soil_zone_map`); a zone bound to a Home Assistant entity through the HA
bridge has no local history and reports that state honestly.
