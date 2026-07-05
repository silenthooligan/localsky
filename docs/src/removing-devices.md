# Removing and disabling devices

LocalSky is meant to be the single place you manage your setup, so removing a
sensor or a zone should not leave you hunting through a second app. This chapter
covers what "remove" and "disable" actually do, and how far LocalSky can reach
into the upstream device on your behalf.

## Disable vs remove

- **Disable** keeps the configuration but stops LocalSky using the thing. A
  source has an `enabled` flag; turning it off leaves the binding in place so
  you can turn it back on later. Nothing changes on the device.
- **Remove** clears the binding entirely. For a soil probe, that means the
  zone stops depending on it, and the "soil probe offline" warning for that
  zone clears (a zone with no soil sensor simply waters on schedule and
  forecast).

## Removing a soil probe

Soil probes are managed wherever you see them:

- **Settings, Devices**: the gateway or source that carries soil probes lists
  each probe on its card with a bind-to-zone selector and a **Remove** action
  right there. The card also links to the full soil-probe manager.
- **Sensors** (the main sensors view): click a probe and its detail view
  carries a **Remove probe** action; the header's **Manage soil probes**
  button opens the full manager.

The Remove action is the same everywhere. It
always does three things safely on the LocalSky side:

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

### Why the gateway step matters

An Ecowitt gateway keeps a sensor's registration, and its last signal and
battery reading, effectively **forever** after the sensor goes quiet. Pulling
the battery stops the live readings but does **not** remove the sensor from the
gateway, so it keeps appearing in the Ecowitt app as if it were still there. The
only ways to clear it are to delete it in the gateway's own UI, or to let
LocalSky do it for you.

LocalSky uses the gateway's "disable this slot" state, which also stops the
gateway from **auto-adding** the sensor back if it is still powered and
broadcasting. So a removal from LocalSky is a real remove-and-stays-removed, not
a temporary un-pair.

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
way to remove things. LocalSky is honest about this per device rather than
pretending:

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
