# Set up zones

A zone is one planted area controlled by one valve. Its plants, soil, sprinkler rate, and controller binding tell LocalSky how to plan and deliver water.

Open **Zones > Edit zone** to edit in place. The same editor is available in Settings. **Cancel** sits beside **Save zone changes**; a failed save preserves your draft.

## Start with the physical setup

| Field | What to enter |
|---|---|
| Name | A recognizable name, such as Back yard shrubs. |
| Controller and station | The device and output that operate this valve. |
| Plant or grass species | The closest match for the planted area. |
| Soil texture | The actual soil class, not a value chosen to force a longer run. |
| Area | The area this zone covers, in the unit shown. |
| Sprinkler type | Rotor, spray, drip, or the appropriate delivery type. |
| Measured precipitation rate | A measured application rate when available; otherwise the type's default is used. |

Check the binding before enabling automatic watering. Some controllers provide a station picker; others need a station or entity ID. MQTT uses its configured command-topic map.

An **Unbound** zone cannot run. Changing the display name will not fix a binding.

## Watering settings

The installation has a scheduling model, and a zone can override it. The [soil model](irrigation-engine.md) schedules from estimated root-zone depletion. The [weekly model](water-budget.md) allocates a target after rain and earlier irrigation are accounted for.

**Max run time** limits a session. Raising it does not disable cycle and soak or other protections. If demand repeatedly exceeds capacity, check the application rate, plant and soil inputs, permitted watering time, and system design before raising limits.

**Weekly target and sessions** govern weekly scheduling. Under the soil model, an explicitly configured weekly target can serve as a rolling delivery ceiling; it is not a replacement for the depletion trigger.

## Optional soil probe

Bind a probe if it represents this zone. Set its calibration and thresholds from the probe and soil, then verify readings. A bound probe that is unavailable or untrusted can hold watering. Leaving the binding blank uses the weather/model path without a measured-soil gate.

[Connect a probe](first-soil-sensor.md) · [Calibration](soil-sensors.md)

## Names and history

The internal zone slug is created once and stays stable. History, overrides, sensor bindings, HA entities, and links refer to it. Rename the **Name** when needed; do not rename the key in raw configuration.

## Photos and verification

Upload a JPG, PNG, GIF, or WebP up to 10 MB, or enter an image URL. Uploaded photos live outside the backup bundle; copy them separately when moving an instance.

Use the short **Test run** only while you can verify the intended valve opens and closes. A saved binding and a successful API response are not physical confirmation.

[Controller setup](controllers.md) · [Watering decisions](irrigation-engine.md)
