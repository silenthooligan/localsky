# Local and offline operation

LocalSky runs its app, irrigation engine, configuration, and history on your hardware. Home Assistant is optional.

Whether a connection works without internet depends on the devices and services you select.

## What stays local

| Connection | Local path |
|---|---|
| Tempest | UDP broadcasts from the hub |
| Supported Ecowitt gateways | LAN polling or custom upload |
| OpenSprinkler | Direct controller API |
| MQTT devices | Your reachable MQTT broker |
| HTTP sensors and controllers | Your device's local endpoint |
| Home Assistant passthrough and services | Your reachable HA API |
| AI advisor | A model endpoint on your network, if configured |

Check [sensor](sensors.md) and [controller](controllers.md) compatibility for each adapter.

## What needs internet

Online forecast providers, external radar and map layers, vendor cloud controllers, remote notification services, and remote AI endpoints depend on their services. An optional update check contacts the release feed when enabled.

LocalSky does not require a LocalSky cloud account or subscription. That does not remove a connected provider's own account or network requirements.

## During an outage

Already recorded history and the local interface remain on the server. Available local sensors can continue reporting.

Forecast caches preserve the original fetch time. A restart or failed refresh does not turn an old forecast into a new one. Automatic plans need sufficient evidence; missing or stale required weather or rain coverage can hold watering.

A local controller can still be reachable while automatic watering is held for missing forecast data. Check the reason under **Watering decisions** rather than assuming that reachable hardware means a valid plan.

## Set up a local installation

1. Run LocalSky on a host that stays on.
2. Add reachable local weather sources and controllers.
3. Configure zones and verify controller bindings.
4. Decide which online services, if any, you want.
5. Review source freshness and required planning evidence before enabling automatic watering.

For a station on another VLAN, configure routing and broadcast handling as needed. LAN discovery is not a substitute for network reachability.

## Add Home Assistant later

The [companion integration](hacs.md) adds LocalSky's state and controls to HA without moving the engine. HA passthrough lets LocalSky consume sensors that HA already owns.

Keep one owner for each automatic watering schedule. Disable overlapping schedules in the controller or other software when LocalSky takes over.

[Install](getting-started.md) · [Sources](sources.md) · [Decision rules](irrigation-engine.md)
