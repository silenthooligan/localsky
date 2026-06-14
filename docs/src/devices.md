# Devices

**Settings, Devices** is the single hub for everything LocalSky talks to: every controller, source, and sensor, whether LocalSky owns it natively or sees it mirrored from Home Assistant. If you only remember one screen for hardware, remember this one. The companion [Sensors](sensors.md) page is just a lens that filters this same set down to the probes and meters.

## The three tiers

LocalSky groups hardware into three tiers, and keeping them straight makes the rest of the UI obvious:

- **Controllers** open and close valves. Your OpenSprinkler, or the Home Assistant service that fronts your valves, is a controller. This is what actually waters. See [Irrigation controllers](controllers.md) for the supported list and per-kind configuration.
- **Sources** (also called gateways) bring data in. A weather station, an Ecowitt gateway on your LAN, a forecast provider, an MQTT broker, or a Home Assistant bridge: each is a source. A source is a pipe, not a probe.
- **Sensors** are the individual probes and meters those sources carry. A soil-moisture probe paired to an Ecowitt gateway is a sensor on that gateway; a flow meter wired to your OpenSprinkler is a sensor on that controller.

So a sensor never connects to LocalSky directly. It rides in through a source or sits on a controller. Add the source or controller here, and its sensors show up underneath it, ready to use. The [Add your first soil sensor](first-soil-sensor.md) walkthrough follows this model end to end.

## Native vs Home Assistant

Every device card is tagged with its origin:

- **Native** devices are ones LocalSky owns directly: a source or controller you added here. Native devices are editable in place. Click **Edit** on the card to open the same source or controller editor used elsewhere, change it, and save; the device registry hot-reloads shortly after.
- **Home Assistant** devices are mirrored in from a configured HA bridge. They are read-only here, because HA owns them. The card says "Managed in Home Assistant" instead of an Edit button. To change one, change it in HA; the mirror follows.

A native device that also exists in Home Assistant carries a small **+ HA** badge, so you can tell at a glance that the same physical thing is visible on both sides without it being a duplicate. Cards also show an **Online** or **Offline** pill when LocalSky has a reachability signal, and a small badge with a count of how many items the device carries. Expand the card to see what those items are: the sensors and zones the device brings in, broken out as child rows.

## Adding a device

The **Add a device** bar gives you two direct paths and one discovery path:

- **Weather source**: opens the source editor. Pick a kind (Ecowitt gateway, MQTT, a forecast provider, a Home Assistant passthrough, and so on), fill in its connection details, and save.
- **Controller**: opens the controller editor. Pick a kind and configure it. Exactly one controller is the default; new zones inherit it.
- **Scan network**: sweeps the LAN for supported gateways (Ecowitt today) that broadcast on your network.

### Scan and adopt

The fastest way to add an Ecowitt gateway is to let LocalSky find it:

1. Click **Scan network**. LocalSky listens for supported gateways broadcasting on the LAN.
2. Each gateway it finds shows up as a **Discovered** card with its model, IP, and MAC address.
3. Click **Adopt as source**. That opens the source editor prefilled with the gateway's host and a sensible poll interval, so you usually just confirm and save.
4. Once saved, LocalSky starts polling the gateway, and its soil channels appear as sensors under it (visible here and on the [Sensors](sensors.md) page), ready to bind to a zone.

If a scan finds nothing, the gateway may not be on the same subnet, or it may not broadcast; in that case add it by hand with **Weather source**, choosing the Ecowitt gateway kind and typing the IP into the `host` field.

## Where to go next

- [Add your first soil sensor](first-soil-sensor.md): the plain-language, end-to-end walkthrough.
- [Weather and soil sensors](sensors.md): the full catalog of what each sensor type unlocks.
- [Irrigation controllers](controllers.md): every supported controller in depth.
- [Forecast sources and merge](forecast.md): how multiple weather sources combine.
