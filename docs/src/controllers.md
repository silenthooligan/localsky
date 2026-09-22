# Irrigation controllers

LocalSky connects to supported controllers to start and stop zones. Add one in **Settings → Devices**, test the connection, then verify each zone's controller binding.

## Choose a connection

| Controller path | Connection | Zone discovery |
|---|---|---|
| OpenSprinkler / OSPi | Direct LAN HTTP | Supported |
| Supported DIY HTTP board | Local HTTP contract | Supported by the example contract |
| MQTT valves | Your broker | Configure mappings |
| Home Assistant service calls | HA REST API | Configure entity mappings |
| Rachio | Vendor cloud API | Supported |
| Hydrawise | Vendor cloud API | Configure relay IDs |
| B-hyve | Vendor cloud API | Configure station IDs |
| Rain Bird | Vendor cloud API | Configure station IDs |
| DryRun | Simulation | Sample zones |

These are implemented adapters, not a claim that every hardware or firmware variant has been tested. Check the connection and a supervised zone before using unattended schedules.

## Bind the right zone

A LocalSky zone has a stable slug and a separate **Controller station** binding. Keep the slug stable: history, overrides, and integrations use it.

Where scanning is supported, pick the controller's zone from the list. Otherwise, enter the adapter's required station, relay, UUID, or HA entity ID. Similar display names are not a reliable binding.

For multiple controllers, assign each zone to its controller explicitly. A missing or invalid binding is a configuration problem, not a reason to guess another valve.

## OpenSprinkler

Use the controller's reachable LAN address and configured password. The Settings editor handles the password hash used by the API. Firmware and network access must support the endpoints LocalSky uses.

LocalSky reads controller status and starts/stops stations through the local API. Its schedule lives in LocalSky. Disable overlapping programs on the controller when LocalSky owns watering.

Controller state and queued programs matter when confirming a stop; check the reported state after a supervised test.

## Home Assistant valves

Use a **Home Assistant service call** controller when HA owns access to the valves. Configure the HA connection, start/stop services, and zone entity mapping.

The called service must accept the configured payload and enforce an appropriate device-side timer. HA must remain available for this controller path.

[Move watering from HA](migrating-from-ha.md)

## MQTT and DIY HTTP

The [DIY guide](diy-controllers.md) includes supported HTTP and MQTT examples. A native ESPHome API adapter is not an implemented control path; use the documented MQTT or HTTP route.

MQTT command delivery alone does not confirm valve state. Provide supported feedback where possible, and implement a hardware or firmware shutoff timer.

## Cloud controllers

Cloud adapters depend on internet access and the vendor API. Credentials, polling limits, and state latency differ.

Rachio, B-hyve, and Rain Bird can stop the entire device when asked to stop one zone. Read the stop confirmation before using this action on a controller with other active watering.

A successful command response can precede observed state. LocalSky reports confirmation timing where the adapter supports it. After a timeout, check state before issuing another run.

[Adapter configuration examples](controller-reference.md)

## Simulation and first test

DryRun exercises the software path without physical watering. It does not test wiring, pressure, or valve closure.

Before the first real run, check the binding, supervise the chosen zone, and verify Start and Stop. Set an appropriate maximum duration and remove competing schedules.

[Zone setup](zones.md) · [Troubleshooting](troubleshooting.md) · [Control API](api-irrigation.md)
