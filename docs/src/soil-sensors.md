# Calibrate soil probes

A soil probe adds evidence about a particular zone. It can support a saturation hold or show that the zone is dry enough to reconsider a soft forecast recommendation.

## Bind the reading

For an Ecowitt channel or discovered HA sensor, add the probe under **Zone → Soil probes**. A zone can use multiple probes.

For MQTT, HTTP, and other manually mapped channels, first set **Bind to zone** in the source mapping. Wait for a reading, then select that channel in the zone editor. A source mapping and the zone's selected probe serve different purposes.

Verify the channel on the Sensors page before relying on it. A gateway being online does not prove a particular probe is current.

[First-probe walkthrough](first-soil-sensor.md)

## Set calibration and targets

Use the supported dry/wet calibration values and the zone's target band. Place the probe where it represents the root zone, away from an isolated emitter or a consistently unwatered edge.

A relative probe percentage is not automatically volumetric water content. Without suitable calibration, the app can show the reading without claiming a precise future soil-percentage curve.

## How readings affect watering

- A sufficiently wet zone can be held independently of neighboring zones.
- Reliable dry evidence can demote supported soft forecast-rain recommendations.
- Stale, invalid, and excluded probes do not contribute to the combined value.
- A configured zone is held when none of its probes are usable.
- A zone with no probe binding can use the weather and soil model.

A true zero reading is not the same as missing data. Freshness and fault checks matter alongside the number.

## Combine probes and handle failures

Choose **Median**, **Average**, or **Lowest** in the zone editor. Median is the
default and uses the middle value, or the mean of the two middle values for an
even count. Average gives every usable probe equal weight. Lowest lets the
driest included probe drive the zone. Different placements can produce valid
different values; review calibration and placement before choosing a policy.

Each probe has a **Stale after (seconds)** setting, from 1 to 604800 seconds.
The default stays at 86400 seconds for compatibility. Set it above the expected
reporting interval, allowing for missed reports. For example, a five-minute
probe can use 900 seconds if three missed intervals should make it stale.
Check the individual probe status in Settings, Sensors; a healthy peer does
not refresh another probe's age.

**Exclude from decisions** retains the binding, measurements, and history.
The other usable probes continue to supply the zone reading. If every probe
is excluded or unavailable, watering stays held. Removing every binding is a
separate action that returns the zone to its weather and soil model.

MQTT soil mappings require one exact topic per probe. Weather mappings still
support wildcards. Retained messages received after subscribing or reconnecting
do not refresh soil measurements. LocalSky waits for a live publication.
Existing mapped channels receive independent IDs on upgrade; their old mixed
history is retained without being attributed to a particular probe. From 1.1.1
the old shared zone readings are no longer listed under Devices or a source's
live readings, and each probe is labeled by its topic, entity or query.

## Temperature, conductivity, and battery

Available fields depend on the hardware. WH51 moisture probes do not provide soil temperature or conductivity. Those values appear only when the device supports and reports them.

Check batteries and placement when readings flatline or disagree with the zone's condition. Do not change a soil texture just to silence a probe fault.

## Tuning

Recorded probe trends can support drying-rate and sprinkler-rate suggestions when enough valid observations exist. Suggestions need evidence and remain reviewable; a single watering event is not enough to establish a new application rate.

[Zone setup](zones.md) · [Tuning report](tuning-report.md) · [Sensor sources](sensors.md)
