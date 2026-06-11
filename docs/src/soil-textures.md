# Soil Texture Catalog

USDA soil texture classification (developed in the US but used internationally as the standard texture taxonomy; the classes apply to any soil, anywhere). LocalSky uses field capacity (FC), wilting point (WP), available water (AW = FC - WP), and infiltration rate per texture + slope. Source: [src/engine/soil_catalog.rs](../src/engine/soil_catalog.rs).

Pick texture per zone in the zone editor. If unsure, use the [USDA texture triangle](https://www.nrcs.usda.gov/sites/default/files/2022-09/Soil-Texture-Triangle.pdf): rub moist soil between your fingers and match to the closest class.

## Catalog

Values per FAO-56 Table 19 + USDA NRCS Part 652 Table 11-3.

| Texture | FC (m³/m³) | WP (m³/m³) | AW (mm/m) | Infil flat (mm/hr) | Infil 3-5% (mm/hr) | Infil >5% (mm/hr) |
|---|---:|---:|---:|---:|---:|---:|
| Sand          | 0.09 | 0.03 |  60 | 50 | 35 | 25 |
| Loamy sand    | 0.14 | 0.06 |  80 | 35 | 25 | 18 |
| Sandy loam    | 0.23 | 0.10 | 130 | 25 | 18 | 12 |
| Loam          | 0.34 | 0.12 | 220 | 13 | 10 |  7 |
| Silt loam     | 0.32 | 0.15 | 170 | 10 |  8 |  5 |
| Clay loam     | 0.39 | 0.20 | 190 |  8 |  6 |  4 |
| Clay          | 0.42 | 0.25 | 170 |  5 |  4 |  3 |

## How the values map into the engine

### Total Available Water (TAW)

```
TAW_mm = (FC - WP) * root_depth_mm
```

This is the depth of water the zone can hold between field capacity (fully wet, no gravity drainage) and the wilting point (so dry the plant gives up). St. Augustine on sandy loam at the default 150 mm root depth: TAW = (0.23 - 0.10) * 150 = 19.5 mm. Tall fescue on loam at its 250 mm default depth: TAW = (0.34 - 0.12) * 250 = 55 mm, nearly triple the buffer.

### Readily Available Water (RAW)

```
RAW_mm = TAW_mm * MAD_pct
```

MAD (Management Allowed Depletion) comes from the species catalog. RAW is the depletion threshold beyond which the plant starts to stress. LocalSky's irrigation trigger is `depletion >= RAW`.

St. Augustine on sandy loam with default 50% MAD: RAW = 19.5 * 0.50 = 9.75 mm. The engine triggers irrigation when the bucket dips below ~10 mm of depletion.

### Infiltration rate

Determines whether cycle-and-soak is needed. The three slope bands per row reflect that water runs off faster on a hillside than on a level patch. The cycle-and-soak splitter divides total runtime when the sprinkler's precipitation rate exceeds infiltration.

Example: spray head (15 mm/hr precip) on clay flat (5 mm/hr infiltration). Each minute of runtime delivers 15/60 = 0.25 mm but the soil can only absorb 5/60 = 0.083 mm. Cycling 1 minute on, 4 minutes "soak" wouldn't actually work because evaporation losses kick in. LocalSky's default minimum cycle is 3 minutes; soak gap is 30 minutes; the splitter computes the maximum continuous on-time at ~`(infiltration/precip) * 60` minutes.

## Picking the right texture for your zone

Without a soil test, two practical methods:

### Ribbon test

1. Take a handful of moist (not wet) soil. Squeeze into a ball.
2. Squeeze the ball through your thumb and forefinger to form a ribbon.
3. Categorize:
   - No ribbon, falls apart: **sand** or **loamy sand**
   - Weak ribbon (<2.5 cm before breaking): **sandy loam** or **loam**
   - Medium ribbon (2.5-5 cm): **clay loam** or **silt loam**
   - Strong ribbon (>5 cm): **clay**

### Jar test

1. Half-fill a one-litre (quart) jar with soil from the zone's root depth.
2. Fill the rest with water + a teaspoon of dish soap.
3. Shake hard. Set aside.
4. After 1 minute, mark the sand layer (settles first).
5. After 2 hours, mark the silt layer.
6. After 24-48 hours, mark the clay layer (or what hasn't settled yet).
7. Use the USDA triangle to classify based on relative thicknesses.

## When in doubt

If you genuinely don't know, **sandy loam** is the safest guess: it sits mid-triangle and the engine's math is most forgiving when off by one texture class in either direction (loamy sand or loam).

## Contributing a texture

The catalog is a fixed enumeration (USDA's classification is the standard; "soil 1" and "soil 2" aren't textures). New entries are not expected. If you need finer-grained soil characterization, override per zone via direct FC/WP/AW values in a future iteration's `ZoneConfig.soil_overrides` block.

## Further reading

- [USDA NRCS National Soil Survey Handbook](https://www.nrcs.usda.gov/resources/guides-and-instructions/national-soil-survey-handbook)
- [FAO Irrigation and Drainage Paper No. 56, Chapter 8 (ETc - Single Crop Coefficient)](https://www.fao.org/3/x0490e/x0490e08.htm)
- [USDA NRCS Part 652 National Irrigation Guide, Chapter 11 (Sprinkler Irrigation)](https://www.nrcs.usda.gov/sites/default/files/2022-09/Sprinkler-Irrigation.pdf)
