# Device setup

**Settings → Devices** is where you add weather sources, sensor gateways, and irrigation controllers. The Sensors page shows discovered channels and their latest readings.

## Add a device

1. Choose **Add a device** and the relevant type.
2. Enter its address or provider credentials.
3. Use the available connection test or discovery action.
4. Save, then check its status and latest readings.
5. Follow a restart prompt if the saved change needs one.

Discovery depends on the adapter and network. A device on another subnet may need a manually entered address.

## Weather sources

A weather source may supply current observations, forecasts, or both. Add your station or provider first, then set the [reading source order](sources.md) and [main forecast](forecast.md).

A connected source can be on standby because another source is preferred. That is different from a failed connection.

## Controllers

A controller connects LocalSky to valves. Supported scanners can import controller zones or offer bindings for existing LocalSky zones.

Check the controller station for every zone. Display names can differ; LocalSky needs the correct underlying station or entity ID.

[Controller guide](controllers.md)

## Soil sensors

Add the gateway or input source, confirm readings on Sensors, then bind a probe to its zone. Configure calibration and targets in the zone settings.

A gateway's connection status does not prove that every attached probe is reporting.

[Connect your first probe](first-soil-sensor.md)

## Diagnose a connection

Open the device's **Technical details** for the failed operation, error code, and available evidence. Keep the timestamp when comparing LocalSky logs with a provider or controller log.

[Error codes](source-errors.md) · [Disable or remove a device](removing-devices.md)
