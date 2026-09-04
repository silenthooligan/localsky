# Zones

A zone is one chunk of yard on one valve. LocalSky schedules each zone on
its own: you describe the grass, the soil, and the area, and the engine
computes the crop evapotranspiration (ETc), the weekly water balance, and
the runtime from there. Edit zones under Settings, then Zones; each save
round-trips through the full config, so the engine picks up changes on the
next tick.

## The core fields

- **Name**: what you call the zone (for example "Back Yard"). Change it
  whenever you like. It auto-derives an internal slug the first time, and
  that slug then stays put: see [slugs are
  permanent](#the-slug-is-permanent) below.
- **Grass species**: picks the seasonal Kc curve, root depth, and MAD
  (allowed depletion) threshold. See the [grass species
  catalog](grass-species.md).
- **Soil texture**: a USDA texture class (used worldwide). It drives field
  capacity, wilting point, and infiltration rate. See [soil
  textures](soil-textures.md).
- **Area**: approximate square footage. It does not have to be exact; it
  feeds leak detection and flow validation when a flow meter is present.
- **Controller** and **Controller station**: which configured controller
  fires this zone, and which of that controller's own zones it is. This
  second field is the binding. Where LocalSky can ask the controller for its
  zone list (OpenSprinkler, Rachio, a DIY HTTP board, the simulated
  controller) the field lists them by the controller's name for each and
  stores its id, so you never copy an identifier by hand and the names on
  the two sides are free to differ. Where it cannot ask (Hydrawise, B-hyve,
  Rain Bird, Home Assistant) you enter the id: a relay id, a station number,
  or an entity id such as `switch.back_yard_zone`. MQTT is the one
  exception: an MQTT zone's binding is a command topic plus its payloads,
  which lives in the controller's `zone_command_map`, and this field is
  ignored for it.

A zone needs a controller before it can run; configure one under Settings,
then Controllers, first.

A zone with no station and no entry in its controller's zone map is
**Unbound**: nothing will water it. So is a zone whose station holds an id
this controller cannot use, which is easy to end up with by moving a zone
from one brand of controller to another: a Rachio zone UUID means nothing to
a Hydrawise. Either way the zone card marks it and the config check says
which it is, rather than letting you find out the first night it does not
run. See [controllers](controllers.md#when-a-zone-will-not-start) when a
zone will not start.

## The slug is permanent

Every zone has an internal slug, derived once from the name you first gave
it and shown read-only in Advanced options. It is not decoration. It is the
key that stores:

- this zone's run history, and the trailing week of water the weekly budget
  allocator reads from it;
- its auto / skip / run override, and its in-flight run ledger;
- its dismissed tuning recommendations;
- its soil sensor channel, as `soilmoisture_<slug>`;
- its nine Home Assistant entities, whose ids are built from it;
- its retained MQTT discovery topics, which have no way to be recalled;
- its `/zones/<slug>` page, and every notification that ever linked there.

Changing it orphans all of that at once, silently, with no way back. So the
slug field is read-only in the editor and the raw TOML editor refuses a
zone-key rename. To change what a zone is CALLED, edit its Name; the slug
stays as it is. Renaming a zone is also never the fix for a controller that
will not fire it, whatever older versions of this guide said: bind it in
**Controller station** instead.

## Advanced options

The rest have sensible defaults, so a beginner can add a working zone with
just the fields above:

- **Sprinkler type** (rotor, spray, MP rotator, drip, bubbler): sets the
  default precipitation rate when the measured rate is blank.
- **Measured precip rate**: a catch-cup measurement in mm/hr. Leave blank
  to use the catalog default for the sprinkler type; measuring it improves
  runtime accuracy substantially.
- **Max run time**: the longest single watering the zone may run, in
  minutes. 60 unless you change it; every session is held to it.
- **Weekly target** and **Sessions per week**: the two numbers that size
  every run. The target is a gross depth in inches a week, rain included;
  the sessions are how many mornings it is split across, 1 to 7, spaced
  `floor(7 / sessions)` days apart. Leave either blank and LocalSky uses a
  default set by the zone's species: its peak crop coefficient against
  reference turf, so warm-season turf starts at 1.00 inches over 2 sessions
  and established shrubs at 0.55 inches over 1. The box shows the default in effect, and the zone list marks
  a zone still watering on it. See the [weekly water
  budget](water-budget.md).
- **Soil moisture sensor** (optional): assign a probe to drive this zone's
  skip decision. The picker lists every discovered soil channel, both Home
  Assistant entities and LocalSky-native sources. Blank means the zone has no
  measured soil gate; it waters on the weekly water balance alone.
- **Healthy band low %** and **Saturation %**: the zone's soil thresholds.
  Below the low band the zone reads "dry" on the Sensors page; at or above
  the saturation percentage the zone skips watering.

Each zone card has a **Test run** button that fires the valve for 30
seconds, so you can confirm water actually comes out before trusting the
overnight engine.
