# Connect a soil probe

Start by getting one reliable reading into LocalSky, then bind it to the correct zone.

## 1. Connect its source

| Probe is available through… | Add |
|---|---|
| Ecowitt gateway | Native gateway source |
| Home Assistant | HA passthrough source |
| MQTT | Subscription with a soil mapping |
| HTTP | Supported webhook or REST mapping |

For Ecowitt, confirm that the probe is registered on the gateway first. For HA, confirm that the entity has a valid value and unit.

## 2. Confirm readings

Open **Sensors** and find the probe. Check its value, unit, battery where available, and observation time. Wait for a real update before binding it.

For MQTT, HTTP, and other mapped channels, set **Bind to zone** on the source mapping. The channel appears after it has published a reading.

## Binding a probe to a zone

Open **Zones → Edit zone → Soil moisture sensor** and choose the channel. Save and follow any restart prompt.

Binding from a probe card can move the probe from a previous zone. Editing a zone changes that zone's binding, so review existing assignments if the same probe is selected elsewhere.

The source mapping routes the reading; the zone selection tells the engine which probe to use. Some custom paths need both.

## 3. Calibrate

Set supported dry/wet calibration values and the target band for the zone. Place the probe at representative root depth and check its response after watering.

Avoid setting another soil texture merely to make a displayed percentage look better. Texture describes the soil; calibration describes the sensor response.

## 4. Review the decision

Open the zone and Watering decisions. Confirm that the selected probe is current and that any hold names the relevant evidence.

A missing or untrusted configured probe holds its affected zone. A zone with no probe binding can use the weather and soil model.

## Common problems

| Problem | Check |
|---|---|
| Probe absent from picker | Has the source published a reading? Is the zone mapping set where required? |
| Percentage never moves | Battery, placement, gateway registration, and original timestamps |
| No temperature or EC | Whether the probe hardware supports those measurements |
| Binding affects the wrong area | Zone assignment and physical probe placement |
| Precise future percentage unavailable | Calibration and evidence coverage |

[Calibration and behavior](soil-sensors.md) · [Weather sources](sensors.md) · [Troubleshooting](troubleshooting.md)
