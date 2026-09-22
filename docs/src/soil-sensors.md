# Calibrate soil probes

A soil probe adds evidence about a particular zone. It can support a saturation hold or show that the zone is dry enough to reconsider a soft forecast recommendation.

## Bind the reading

For an Ecowitt channel or discovered HA sensor, select the probe under **Zone → Soil moisture sensor**.

For MQTT, HTTP, and other manually mapped channels, first set **Bind to zone** in the source mapping. Wait for a reading, then select that channel in the zone editor. A source mapping and the zone's selected probe serve different purposes.

Verify the channel on the Sensors page before relying on it. A gateway being online does not prove a particular probe is current.

[First-probe walkthrough](first-soil-sensor.md)

## Set calibration and targets

Use the supported dry/wet calibration values and the zone's target band. Place the probe where it represents the root zone, away from an isolated emitter or a consistently unwatered edge.

A relative probe percentage is not automatically volumetric water content. Without suitable calibration, the app can show the reading without claiming a precise future soil-percentage curve.

## How readings affect watering

- A sufficiently wet zone can be held independently of neighboring zones.
- Reliable dry evidence can demote supported soft forecast-rain recommendations.
- A configured missing or untrusted probe holds its affected zone.
- A zone with no probe binding can use the weather and soil model.

A true zero reading is not the same as missing data. Freshness and fault checks matter alongside the number.

## Temperature, conductivity, and battery

Available fields depend on the hardware. WH51 moisture probes do not provide soil temperature or conductivity. Those values appear only when the device supports and reports them.

Check batteries and placement when readings flatline or disagree with the zone's condition. Do not change a soil texture just to silence a probe fault.

## Tuning

Recorded probe trends can support drying-rate and sprinkler-rate suggestions when enough valid observations exist. Suggestions need evidence and remain reviewable; a single watering event is not enough to establish a new application rate.

[Zone setup](zones.md) · [Tuning report](tuning-report.md) · [Sensor sources](sensors.md)
