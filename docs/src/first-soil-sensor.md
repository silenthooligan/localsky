# Add your first soil sensor

This is the plain-language walkthrough for getting one soil-moisture reading into LocalSky and using it to gate a zone. No YAML, no terminal. If you have never wired a sensor before, start here; if you are a pro, skip to the path that matches your hardware.

## The model: controllers, gateways, sensors

LocalSky talks about hardware in three tiers. Keeping them straight is the one thing that makes the rest obvious:

- **Controllers** open and close valves. Your OpenSprinkler (or the Home Assistant service that fronts your valves) is a controller. This is what actually waters.
- **Sources** (also called gateways) bring data in. A weather station, an Ecowitt gateway on your LAN, a forecast provider, an MQTT broker, a Home Assistant bridge: each is a source. A source is a pipe, not a probe.
- **Sensors** are the individual probes and meters those sources carry. A soil-moisture probe paired to an Ecowitt gateway is a sensor on that gateway. A flow meter wired to your OpenSprinkler is a sensor on that controller.

So a soil probe never connects to LocalSky directly. It rides in through a source: an Ecowitt gateway polls it, an MQTT topic carries it, or Home Assistant already owns it and LocalSky reads it from there. Add the source first, and its sensors show up underneath it, ready to bind to a zone.

You manage all of this in two places. **Settings, Devices** is the hub where you add sources and controllers and see every device LocalSky knows about. **Settings, Sensors** is the lens that lists just the probes and meters, grouped by the source or controller they arrive through, with the control to bind each one to a zone.

## The three ways in

There are three supported paths for a soil probe. Pick the one that matches what you have:

1. **Ecowitt gateway on your LAN** (recommended, and the cheapest). Native, no cloud, no broker.
2. **Any MQTT-published probe** (ESPHome, Tasmota, Zigbee2MQTT, a DIY ESP32). Needs an MQTT broker.
3. **Any Home Assistant soil entity** (a Zigbee or Z-Wave probe HA already knows about). Needs an HA bridge.

### Path 1: Ecowitt gateway (recommended)

An Ecowitt WH51 (or WH52) soil probe is battery-powered, costs a few dollars, and pairs wirelessly to an Ecowitt gateway (GW1100, GW2000, and similar). LocalSky polls the gateway directly over your LAN and reads every soil channel natively: moisture, temperature, conductivity, and battery, per probe.

How the pieces fit: the probe pairs to the gateway (in the gateway's own WS View app), and the gateway sits on your LAN. LocalSky polls the gateway, not the probe. So the probe is a sensor, the gateway is the source, and you only ever add the gateway to LocalSky.

Steps:

1. **Find the gateway's IP.** Open the Ecowitt WS View app (the one you used to set up the gateway), or check your router's client list. It looks like `10.0.0.50`.
2. **Pair your soil probes to the gateway** if you have not already, again in WS View. Each probe claims a soil channel (1, 2, 3...).
3. In LocalSky, go to **Settings, Devices** and click **Scan network**. LocalSky finds Ecowitt gateways broadcasting on your LAN and offers an **Adopt as source** button. (No gateway found? Click **Weather source**, pick `Ecowitt gateway (poll)`, and type the IP into the `host` field by hand.)
4. Save. LocalSky starts polling the gateway every 30 seconds.
5. Go to **Settings, Sensors**. Each soil channel the gateway reports now appears as a card under the gateway, with its live moisture reading.
6. **Bind a probe to a zone** with the "Bound zone" dropdown on that card. See [Binding a probe to a zone](#binding-a-probe-to-a-zone) below.

Calibration is optional. The gateway reports a moisture percentage on its own, but if you want LocalSky to compute it from the probe's raw reading (more accurate for your specific soil), the source editor has a **Soil channel calibration** form: pull the probe out and read its dry value, soak it and read its wet value, enter both, and LocalSky maps everything in between to 0 to 100 percent.

### Path 2: any MQTT probe

If your probe (or a hub like Zigbee2MQTT) publishes to an MQTT broker, LocalSky can subscribe. This covers DIY ESP32 capacitive probes on ESPHome, Tasmota devices, and anything on Zigbee2MQTT.

You need an MQTT broker on your network. Mosquitto is free and tiny; if you already run Home Assistant you almost certainly already have one.

Steps:

1. **Publish soil moisture to a topic** from your probe, for example `zigbee2mqtt/garden_soil` or `esp/soil/back`. Note whether the payload is a bare number or a JSON object.
2. In LocalSky, go to **Settings, Devices**, click **Weather source**, and pick `MQTT`.
3. Fill in the broker host, port, and credentials.
4. In the **Soil subscriptions** form, click **+ Add soil subscription** and set:
   - **MQTT topic**: the topic your probe publishes to (wildcards `+` and `#` are allowed).
   - **JSON field**: if the payload is a JSON object, the field that holds the moisture value (for example `soil_moisture`). Leave blank if the payload is just a number.
   - **Bind to zone**: the zone this probe measures. This records the topic as that zone's own soil channel so it is not merged into general humidity.
   - Leave **Reading** on "Soil moisture" unless this same topic also carries temperature or another reading you want.
5. Save. The subscription starts immediately.
6. Go to **Settings, Sensors** to confirm the probe is reading, then finish wiring it in the zone (the MQTT form binds the topic to the zone; the zone editor picks that channel as its soil sensor). See [Binding a probe to a zone](#binding-a-probe-to-a-zone).

### Path 3: any Home Assistant soil entity

If Home Assistant already owns a soil probe (a Zigbee probe on ZHA, a Z-Wave probe, anything that shows up as a `sensor.*` moisture entity in HA), LocalSky can read it through an HA bridge. Nothing re-pairs; HA stays the owner.

Steps:

1. **Create a long-lived access token in HA**: your HA profile, Security, Long-lived access tokens, Create token. Copy it.
2. In LocalSky, go to **Settings, Devices**, click **Weather source**, and pick `HA passthrough`.
3. In the **Connection** form, fill the **Home Assistant URL** field with your HA address (for example `http://10.0.0.10:8123`) and the **Long-lived token** field with the token you just copied. Save. That bridge is now a source.
4. Open **Settings, Zones**, pick the zone, and in the **Soil moisture sensor** dropdown choose your HA probe. It appears in the list as `ha:sensor.<your_entity>` (the picker reads HA's entity list using the credentials from step 3).

That is it: an HA soil entity is bound straight from the zone editor, because the HA bridge is the source and HA already enumerates the probe for you.

## Binding a probe to a zone

A probe does nothing until it is bound to a zone. Binding tells the engine "this reading is the truth for this zone's moisture," and the engine stops guessing from the weather model alone for that zone.

Where you bind depends on the source:

- **Ecowitt and HA probes** bind in one step. Use either **Settings, Sensors** (each probe card has a **Bound zone** dropdown) or **Settings, Zones** (open the zone, pick the probe in its **Soil moisture sensor** dropdown). The binding saves immediately and the engine uses it on the next tick.
- **MQTT probes** are a two-step. First, in the source's **Soil subscriptions** form, set the subscription's **Bind to zone**: that registers the topic as that zone's own soil channel. Then go to **Settings, Zones**, open the zone, and confirm that channel in its **Soil moisture sensor** dropdown.

One probe maps to one zone. Re-binding a probe to a different zone releases it from the old one automatically.

How the engine uses a bound probe:

- **Below the zone's target band**: the zone is eligible to water; the run sizes to the deficit as usual.
- **Inside the band**: healthy; scheduled runs still apply unless the saturation threshold says otherwise.
- **At or above saturation**: the zone skips on its own, even when the day's overall verdict is Run, and the skip reason names the zone's moisture reading and the saturation threshold (for example, "Soil saturated (76% ≥ 65% threshold)").

If a bound probe goes offline, the zone falls back to the modeled bucket automatically. Nothing blocks; a missing probe never stops a run.

## A note on flow meters: capable vs connected

Flow metering is a separate idea from soil moisture, and it lives on the controller, not on a gateway. The Sensors view distinguishes two states so the wording is never misleading:

- **Capable**: your controller type supports a flow input (OpenSprinkler does). The Sensors view shows "Flow meter supported. None connected."
- **Connected**: a physical flow sensor is wired to that input and reporting. Then the view shows the live gallons per minute.

To go from capable to connected on an OpenSprinkler: wire a pulse flow sensor to the controller's FLOW input, set the K-factor on the device, and LocalSky reads it automatically. Once a flow meter is connected it validates that each run delivered the water the engine asked for and flags leaks (flow with no zone running).

## Where to read more

- [Weather and soil sensors](sensors.md): the full catalog of what each sensor type unlocks.
- [Soil probes and zones](soil-sensors.md): the short reference for the binding model.
- [Irrigation controllers](controllers.md): the supported controller list, including flow-meter support.
- [Skip rules in depth](skip-rules.md): exactly how a soil reading changes a verdict.
