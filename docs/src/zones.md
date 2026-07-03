# Zones

A zone is one chunk of yard on one valve. LocalSky schedules each zone on
its own: you describe the grass, the soil, and the area, and the engine
computes the crop evapotranspiration (ETc), the soil-moisture bucket, and
the runtime from there. Edit zones under Settings, then Zones; each save
round-trips through the full config, so the engine picks up changes on the
next tick.

## The core fields

- **Name**: what you call the zone (for example "Back Yard"). It
  auto-derives a stable internal slug used by history and sensor bindings.
- **Grass species**: picks the seasonal Kc curve, root depth, and MAD
  (allowed depletion) threshold. See the [grass species
  catalog](grass-species.md).
- **Soil texture**: a USDA texture class (used worldwide). It drives field
  capacity, wilting point, and infiltration rate. See [soil
  textures](soil-textures.md).
- **Area**: approximate square footage. It does not have to be exact; it
  feeds leak detection and flow validation when a flow meter is present.
- **Controller** and **Controller station**: which configured controller
  fires this zone, and the station identifier on it. For OpenSprinkler the
  station is a 1-based number (1, 2, 3); for a DIY HTTP board it is the
  board's zone id; for a Home Assistant service call it is the entity id
  (for example `switch.back_yard_zone`).

A zone needs a controller before it can run; configure one under Settings,
then Controllers, first.

## Advanced options

The rest have sensible defaults, so a beginner can add a working zone with
just the fields above:

- **Sprinkler type** (rotor, spray, MP rotator, drip, bubbler): sets the
  default precipitation rate when the measured rate is blank.
- **Measured precip rate**: a catch-cup measurement in mm/hr. Leave blank
  to use the catalog default for the sprinkler type; measuring it improves
  runtime accuracy substantially.
- **Soil moisture sensor** (optional): assign a probe to drive this zone's
  skip decision. The picker lists every discovered soil channel, both Home
  Assistant entities and LocalSky-native sources. Blank means the engine
  uses its modeled soil bucket only.
- **Healthy band low %** and **Saturation %**: the zone's soil thresholds.
  Below the low band the zone reads "dry" on the Sensors page; at or above
  the saturation percentage the zone skips watering.

Each zone card has a **Test run** button that fires the valve for 30
seconds, so you can confirm water actually comes out before trusting the
overnight engine.
