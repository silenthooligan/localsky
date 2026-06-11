# Soil sensors

Wire a moisture probe to a zone and the engine stops guessing: the
probe's reading replaces the modeled bucket as the zone's gate.

**Supported paths in:**

- Ecowitt soil probes (WH51 and friends) via a LAN gateway poll: native,
  no cloud, includes temperature, conductivity, and battery per probe.
- Any Home Assistant soil sensor entity, through an HA bridge source.
- MQTT topics and HTTP webhooks for DIY probes.

**Assignment** happens in the zone's settings (Settings > Zones > pick
the zone > soil sensor). One probe per zone; the Sensors hub shows
which zones each source feeds.

**How the engine uses it:**

- Below the zone's target band: the zone is eligible; runs size to the
  deficit as usual.
- Inside the band: healthy; scheduled runs still apply unless the
  saturation threshold says otherwise.
- At or above saturation: the zone skips on its own, even when the day's
  verdict is Run, and the skip reason names the probe.

The Sensors hub and each zone's detail show the probe's live reading,
the target band, and a 7-day no-watering projection so you can sanity
check that the moisture curve actually behaves like your yard.
