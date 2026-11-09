# Location and timezone

Set latitude, longitude, and elevation during setup. LocalSky uses them for sunrise, solar calculations, forecast coverage, and map placement. The timezone is inferred from coordinates.

Use address search, your device's location, or enter coordinates directly. Check the result, especially near a timezone boundary. Enter a known elevation when the automatic lookup is unavailable or inaccurate.

## Address search

Type a street, town, or landmark and press Enter. LocalSky looks it up in OpenStreetMap through its own server, so your browser never talks to the map service directly. When nothing matches, it retries with the town dropped (OpenStreetMap often files a street under a nearby place rather than the postal town) and then, for a United States address with a house number, asks the US Census Bureau geocoder, which lists new streets before OpenStreetMap has them named. Each result says which service found it.

If a search comes back empty, try the street or landmark on its own, add the ZIP or postal code, or include a house number for a street built in the last few years. Coordinates from your phone's map app always work.

Searches are limited to 30 a minute per device, and LocalSky spaces its own requests to OpenStreetMap one second apart, as that service requires.

## Pick on the map

The button opens a map under the search box. Tap or click where you are to drop the pin, drag it to fine-tune, and the coordinates below follow. A search result, your device's location, or a typed coordinate moves the pin too. The map tiles come from OpenStreetMap and are fetched only while the map is open.

## Use my location

The button asks your browser for the device's position. Browsers only share it over HTTPS or on localhost, and only after you allow it. When LocalSky is embedded in Home Assistant, open it in its own tab first. An indoor fix can be off by tens of meters, so check the coordinates before moving on.

After moving an installation, update its location in Settings and confirm the timezone, forecast coverage, and next scheduled run. Daily history and permitted watering days depend on local time.

Sunrise scheduling requires a valid sunrise. At locations or dates without one, inspect the reported scheduling state and configure an appropriate supported schedule instead of assuming the usual morning window exists.

[Manual schedules](schedules.md) · [Watering restrictions](restrictions.md)
