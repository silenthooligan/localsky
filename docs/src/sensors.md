# Weather and soil sensors

LocalSky can use a station on your LAN, sensors already in Home Assistant, supported online providers, or custom MQTT and HTTP inputs.

Add sources in **Settings → Devices**. Use the **Sensors** page to verify received channels and their observation times.

## Choose an input path

| Your data | Connection |
|---|---|
| Tempest hub on the local network | Tempest UDP |
| Supported Ecowitt gateway | Native LAN poll or custom upload |
| WeatherFlow or other entities already in HA | HA passthrough |
| Other supported station/provider | Its entry in the source picker |
| Custom broker messages | MQTT subscription and field mapping |
| Device pushing JSON | HTTP webhook mapping |
| Device exposing an HTTP API | Supported REST field mapping |

The source catalog lists the available adapters. A listed adapter does not guarantee every model or firmware variant works.

## Tempest

Place the receiver where the hub's UDP 50222 broadcasts can reach it. Broadcasts may not cross Docker bridges or VLANs.

If HA's WeatherFlow integration already receives the local feed, LocalSky can consume its entities instead. Disable or remove LocalSky's Tempest UDP source, configure HA passthrough, and select the desired HA readings.

For WeatherFlow's preceding-minute precipitation sensor, choose **Rain last minute (accumulate today)**. Do not map it directly as a daily total.

[HA weather setup](hacs.md#use-home-assistant-weather-sensors)

## Ecowitt

Add a supported gateway by its reachable LAN address. Native polling can discover available channels. Custom upload is a separate push path configured on the gateway.

Sensor capabilities vary. A WH51 supplies moisture and battery information; it does not provide soil temperature or conductivity. Only reported fields should appear as readings.

## Soil probes

Connect the source first, wait for a reading, then bind the channel to a zone. For manually mapped MQTT/HTTP channels, also set the zone on the source mapping.

Configure calibration and target bands for the actual probe placement. One reading should represent the zone's root conditions; a gateway connection alone is not soil evidence.

[First soil probe](first-soil-sensor.md) · [Calibration](soil-sensors.md)

## Flow meters

Flow reporting requires a supported controller and connected meter. Capability, connection, and a live flow reading are separate states.

A missing flow reading stays unknown. Watering duration is not metered volume, and a controller's water-level percentage is not a flow measurement.

## Without station hardware

A new installation can obtain weather and forecasts from Open-Meteo. Other regional or global sources can be added. Modeled values remain modeled; adding a provider does not make its data an on-site measurement.

Choose the [source order](sources.md) and review [provider capabilities](provider-matrix.md). Online providers need network access.

## Confirm the result

Check the value, unit, source, and original timestamp. A successful poll can return stale data. If a reading is wrong or absent, open its source's Technical details before changing watering thresholds.

[Device setup](devices.md) · [Custom ingest API](api-devices.md#data-ingest)
