# Disable or remove a device

Disable a device to keep its configuration while taking it out of use. Remove it when you no longer want its configuration or bindings. Read the confirmation: removing a probe can also remove stored readings or attempt a gateway change.

## Disable vs remove

- **Disable** keeps the configuration but stops LocalSky using the thing. A
  source has an `enabled` flag; turning it off leaves the binding in place so
  you can turn it back on later. Nothing changes on the device.
- **Remove** clears the binding entirely. For a soil probe, that means the
  zone stops depending on it, and the "soil probe offline" warning for that
  zone clears (a zone with no soil sensor simply waters on schedule and
  forecast).

## Releasing Tempest for another application

Open **Settings > Devices**, expand the Tempest source, and turn it off or use
**Remove**. Disabling keeps its configuration; removing also clears source
selections and zone bindings that reference it. Neither action changes the hub.

The Tempest listener checks saved configuration every 15 seconds. Disabling or
removing its source closes the UDP socket on that check, so another application
can use port **50222** before LocalSky restarts. Complete any requested LocalSky
restart to finish updating source connections and clear the watering hold.
Adding a new source, including HA passthrough, needs that restart to start reading.

The hub broadcasts to the local network. Separate hosts can receive the same
broadcasts independently; two applications sharing the same host network can
conflict over the listening address and port. If HA's WeatherFlow integration
failed during that conflict, reload its integration entry after LocalSky releases
its socket. You can then [feed WeatherFlow readings through HA](migrating-from-ha.md#keeping-weatherflow-in-home-assistant)
without changing Docker networking or editing TOML.

## Removing a soil probe

Soil probes are managed wherever you see them:

- **Settings, Devices**: the gateway or source that carries soil probes lists
  each probe on its card with a bind-to-zone selector and a **Remove** action
  right there. The card also links to the full soil-probe manager.
- **Sensors** (the main sensors view): click a probe and its detail view
  carries a **Remove probe** action; the header's **Manage soil probes**
  button opens the full manager.

The Remove action is the same everywhere. It
changes three things on the LocalSky side:

1. Clears the probe's binding from whatever zone used it.
2. Suppresses that zone's offline warning (a removed probe is not a fault).
3. Deletes the probe's recorded readings, so it disappears from the device
   list and the sensor pickers instead of lingering until data retention
   ages it out.

If the probe lives on an **Ecowitt gateway** and you have set the gateway login
on that source (see below), LocalSky can also **unregister the sensor on the
gateway** in the same click, so it stops showing there too. The confirmation
tells you exactly what will happen, and the result reports each side honestly:
it will not claim a gateway removal that did not occur.

### Gateway registration

Removing a probe from LocalSky does not automatically remove it from its upstream gateway. Where supported and authorized by a configured gateway login, LocalSky can disable the Ecowitt slot too. The result reports LocalSky cleanup and gateway cleanup separately. Otherwise, remove the registration in the gateway interface.

### Enabling gateway removal

Gateway removal is off until you give LocalSky the gateway's web login. The
polling that reads your sensors does not need it (those endpoints are open on
the LAN), so this is only for management writes. Add it to the Ecowitt source:

```toml
[[sources]]
id = "ecowitt_gw"
[sources.config]
host = "192.0.2.12"
username = "admin"
password = "your-gateway-password"
```

Without a login, the Remove button still clears the LocalSky binding; it just
tells you to delete the sensor in the gateway UI yourself.

## What each device class allows

Not every device can be managed from LocalSky, because they do not all expose a
way to remove things. Review the supported operation for each device:

| Device | Remove from LocalSky | Also remove upstream? |
|---|---|---|
| Ecowitt gateway sensors | Yes (clears the binding) | Yes, when the gateway login is set |
| Home Assistant entities | Yes (stops consuming) | No, delete the entity in HA |
| MQTT sensors | Yes (stops consuming) | No, the publisher owns the topic |
| Cloud controller zones (Rachio, B-hyve, Hydrawise) | Yes (clears the binding) | No, zones are defined in the vendor app |
| OpenSprinkler stations | Yes (clears the binding) | Station disable is on the roadmap |

Where LocalSky cannot reach the device, removing in LocalSky still does the
useful half (stops using it, clears the warnings) and points you at the one
manual step that remains.
