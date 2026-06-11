# Why this duration?

Every zone's detail view includes the full arithmetic behind tonight's
planned minutes, because "trust me" is not a number.

The chain, top to bottom:

1. **Bucket deficit (mm)**: how far the zone's modeled soil moisture
   sits below full. Rain and runs fill it; daily crop ET drains it.
2. **Crop coefficient (Kc)**: the species' seasonal multiplier on
   reference ET (see the grass species catalog). Hemisphere-aware:
   south of the equator the curve shifts six months.
3. **Heat multiplier**: optional extension when forecast highs cross
   the heat-advisory threshold.
4. **Throughput (mm/hr)**: how fast your sprinklers actually apply
   water, either measured (catch cups) or the catalog default for the
   head type.
5. **Capture efficiency**: how much of the applied water lands in the
   root zone (wind drift, overspray, runoff losses).

Planned seconds = deficit / (throughput x efficiency), capped by the
zone's max-runtime guard. Every input is shown live with its source,
so when a number looks wrong you can see exactly which knob to turn.
