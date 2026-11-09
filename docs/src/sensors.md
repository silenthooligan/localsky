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

## MQTT

Add one subscription per topic and pick what it measures under **Reading**: temperature, humidity, dew point, pressure, wind speed, gust and direction, rain, solar, illuminance, UV, lightning, ET0, flow, leaf wetness, or soil moisture. Readings are stored in the units the form shows (°F, in, mph, inHg). For a metric device, set **Scale** and **Offset**: °C to °F is 1.8 and 32, mm to in is 0.03937, hPa to inHg is 0.02953, km/h to mph is 0.6214.

Soil moisture with a zone becomes that zone's probe and needs the exact topic. Without a zone, the same reading is air humidity.

Readings that no live source reports come from the forecast. On the **Sensors** page, **Current conditions sources** shows where each reading comes from.

## Ecowitt

Add a supported gateway by its reachable LAN address. Native polling can discover available channels. Custom upload is a separate push path configured on the gateway.

Sensor capabilities vary. A WH51 supplies moisture and battery information; it does not provide soil temperature or conductivity. Only reported fields should appear as readings.

## Soil probes

Connect the source first, wait for a reading, then bind the channel to a zone. For manually mapped MQTT/HTTP channels, also set the zone on the source mapping.

Configure calibration and target bands for the actual probe placement. Choose median, average, or lowest when a zone uses several probes. Each probe has its own freshness window and exclusion switch. A gateway connection alone is not soil evidence.

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
