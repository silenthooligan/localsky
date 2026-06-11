# Weekly water budget

The budget panel tracks how much water each zone has received over the
trailing week, from every counted source, against what the engine
thinks the week should deliver.

**Counted in:**

- Irrigation runs (recorded per zone, per second of runtime, converted
  through the zone's precipitation rate).
- Measured rainfall (from your station or gateway).

**The target** comes from the zone's crop evapotranspiration: daily
ET0 (computed FAO-56 from your weather) times the species coefficient
for the season, summed over the week. A warm-season lawn at the height
of summer needs the full bucket; the same lawn in midwinter needs a
fraction (the engine flips seasons automatically south of the equator).

**Reading the bars:** a zone sitting near 100% is on plan. Persistently
under target means runs are being skipped or are too short (check the
zone's math panel); persistently over means rain is doing the work and
the engine should be skipping more, or the precip rate is set too low.

The budget is advisory; it never blocks watering by itself. The deficit
model (soil bucket) is what gates actual run decisions.
