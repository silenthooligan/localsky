# Soil catalog

Choose the texture that represents each zone. LocalSky uses these catalog values to estimate root-zone storage and infiltration. They are model inputs, not a soil test of your property.

## Catalog

| Texture | FC (m³/m³) | WP (m³/m³) | AW (mm/m) | Infil flat (mm/hr) | Infil 3-5% (mm/hr) | Infil >5% (mm/hr) |
|---|---:|---:|---:|---:|---:|---:|
| Sand          | 0.09 | 0.03 |  60 | 50 | 35 | 25 |
| Loamy sand    | 0.14 | 0.06 |  80 | 35 | 25 | 18 |
| Sandy loam    | 0.23 | 0.10 | 130 | 25 | 18 | 12 |
| Loam          | 0.27 | 0.12 | 150 | 13 | 10 |  7 |
| Silt loam     | 0.32 | 0.15 | 170 | 10 |  8 |  5 |
| Clay loam     | 0.36 | 0.20 | 160 |  8 |  6 |  4 |
| Clay          | 0.38 | 0.24 | 140 |  5 |  4 |  3 |

## Water storage

```text
TAW_mm = (field_capacity - wilting_point) × root_depth_mm
RAW_mm = TAW_mm × allowed_depletion_fraction
```

TAW is the modeled water available to roots. RAW is the allowed depletion used to form the soil model's watering trigger. These quantities follow the root-zone balance described in [FAO-56](https://www.fao.org/4/x0490e/x0490e0e.htm).

For example, sandy loam with 150 mm roots has 19.5 mm of available storage. At an allowed depletion fraction of 0.5, RAW is 9.75 mm. The actual decision also depends on evidence quality, forecast rain, restrictions, and other gates.

## Infiltration and runoff

The infiltration value and slope help determine cycle and soak. Watering faster than soil can absorb it can cause runoff. Catalog values are starting estimates; compacted soil, slopes, surface cover, and sprinkler distribution affect the real result.

## Choose a texture

Use a soil test or a local soil survey where available. A hand texture assessment can narrow the choice, but do not treat a guess as a measured property. Check the zone after watering and investigate persistent runoff or rapid drying.

Do not select a different texture simply to suppress a capacity warning. Set the physical inputs first, then assess whether the system can deliver the required water.

[Zone setup](zones.md) · [Plant catalog](grass-species.md) · [Watering logic](irrigation-engine.md)
