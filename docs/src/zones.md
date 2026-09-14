# Zones

A zone is one chunk of yard on one valve. LocalSky schedules each zone on
its own: you describe the grass, the soil, and the area, and the engine
computes the crop evapotranspiration (ETc), the weekly water balance, and
the runtime from there. Select a zone on **Zones** and choose **Edit zone**
to open its editor on that page. Settings → Zones uses the same editor.
**Cancel** sits beside **Save zone changes** and asks before discarding edits.
A failed save keeps the draft available to retry. Scalar changes apply on the
next tick; changes to zone membership or controller bindings show a restart
message when the server requires one.

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
  minutes. 60 unless you change it, and the field takes 5 to 360; every
  session is held to it. A save that raises the limit past 60 asks you to
  confirm first, so a stray keystroke cannot leave a valve open for six
  hours unattended. Only a raise past 60 asks: lowering it, or re-saving a
  zone already set to 90, stays quiet. Raising it does not switch off
  cycle-and-soak. When a zone's heads put water down faster than its soil
  takes it, a run longer than one cycle is still split into cycles with
  soak gaps between them, and a longer session just means more of them. A
  run that fits inside one cycle applies in a single pass with no soak. See
  [cycle-and-soak](irrigation-engine.md#cycle-and-soak).
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
- **Photo** (optional): a picture of the zone, shown on its zone card.
  Drop an image onto the field or browse for one, and LocalSky uploads it,
  writes the file into its photos directory (`/data/site/photos` unless
  `LOCALSKY_PHOTOS_DIR` points elsewhere), and fills the field in with the
  URL it serves the file back at, `/site/photos/<filename>`. JPG, PNG, GIF,
  and WebP up to 10 MB are accepted; SVG is not, because an SVG can carry
  script. If the picture already lives somewhere else, paste its address
  into the URL box under the drop zone and nothing is uploaded. Uploaded
  photos sit outside the config, so they are not in the backup bundle; copy
  `/data/site/photos` yourself if they matter to you.
  See [backup and restore](backup-restore.md).

Each zone card has a **Test run** button that fires the valve for 30
seconds, so you can confirm water actually comes out before trusting the
overnight engine.
