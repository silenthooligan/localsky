# Grass Species Catalog

LocalSky ships a built-in catalog of 12 grass species + ornamental categories with monthly Kc curves, root zone depths, and MAD percentages. Source: [src/engine/species_catalog.rs](../src/engine/species_catalog.rs).

ETc for any zone equals `ET0 * Kc(species, day-of-year) * heat_multiplier`. Picking the right species is the single most impactful zone setting.

## Warm-season turfgrasses (Florida-centric)

These five are LocalSky's primary use case. UF/IFAS Extension publication numbers cited.

### St. Augustinegrass

- **Citation**: UF/IFAS [ENH62](https://edis.ifas.ufl.edu/publication/ep063), "St. Augustinegrass for Florida Lawns"
- **Kc (Jan-Dec)**: 0.55 / 0.60 / 0.70 / 0.85 / 0.95 / 1.00 / 1.00 / 1.00 / 0.95 / 0.85 / 0.70 / 0.55
- **Root zone depth**: ~150 mm (4-6 in; aerated lawns up to 6 in)
- **MAD**: 50%
- **Salinity tolerance**: ~6 dS/m (ECe at 50% yield)
- **Mow height**: 3.5 in
- **Notes**: most common Florida turf. Shallow-rooted; prefers deeper, less-frequent watering. Active growth Apr-Oct; semi-dormant Nov-Mar in north FL.

### Bermudagrass

- **Citation**: UF/IFAS [ENH19](https://edis.ifas.ufl.edu/publication/lh007), "Bermudagrass for Florida Lawns"
- **Kc (Jan-Dec)**: 0.50 / 0.55 / 0.65 / 0.80 / 0.90 / 0.95 / 0.95 / 0.95 / 0.90 / 0.80 / 0.65 / 0.50
- **Root zone depth**: ~200 mm (4-8 in; deep on sand)
- **MAD**: 50%
- **Salinity tolerance**: ~8 dS/m
- **Mow height**: 1.5 in
- **Notes**: deepest-rooted common turf. Drought-tolerant; can go semi-dormant in heat.

### Zoysiagrass

- **Citation**: UF/IFAS [ENH11](https://edis.ifas.ufl.edu/publication/lh011), "Zoysiagrass for Florida Lawns"
- **Kc (Jan-Dec)**: 0.55 / 0.60 / 0.65 / 0.75 / 0.85 / 0.90 / 0.90 / 0.90 / 0.85 / 0.75 / 0.65 / 0.55
- **Root zone depth**: ~150 mm
- **MAD**: 50%
- **Salinity tolerance**: ~7 dS/m
- **Mow height**: 2.0 in
- **Notes**: slow but dense; tolerates moderate shade; recovers slowly from drought.

### Bahiagrass

- **Citation**: UF/IFAS [ENH6](https://edis.ifas.ufl.edu/publication/lh006), "Bahiagrass for Florida Lawns"
- **Kc (Jan-Dec)**: 0.55 / 0.60 / 0.65 / 0.75 / 0.80 / 0.85 / 0.85 / 0.85 / 0.80 / 0.75 / 0.65 / 0.55
- **Root zone depth**: ~200 mm
- **MAD**: 55%
- **Salinity tolerance**: ~4 dS/m
- **Mow height**: 3.5 in
- **Notes**: drought-tolerant; common Florida pasture grass; tolerates low fertility.

### Centipedegrass

- **Citation**: UF/IFAS [ENH8](https://edis.ifas.ufl.edu/publication/lh009), "Centipedegrass for Florida Lawns"
- **Kc (Jan-Dec)**: 0.50 / 0.55 / 0.60 / 0.70 / 0.80 / 0.85 / 0.85 / 0.85 / 0.80 / 0.70 / 0.60 / 0.50
- **Root zone depth**: ~100 mm (3-5 in; shallow)
- **MAD**: 50%
- **Salinity tolerance**: ~3 dS/m
- **Mow height**: 2.0 in
- **Notes**: low-maintenance; iron-chlorotic on high-pH soils.

## Cool-season turfgrasses

For northern and transitional-zone users. Curves drawn from FAO-56 Table 12.

### Kentucky Bluegrass

- **Kc (Jan-Dec)**: 0.55 / 0.60 / 0.75 / 0.85 / 0.85 / 0.80 / 0.78 / 0.80 / 0.85 / 0.80 / 0.65 / 0.55
- **Root zone depth**: ~150 mm
- **MAD**: 50%
- **Notes**: self-repairs via rhizomes; dormant in summer drought without irrigation. Peak ET in spring/fall; summer heat stress dips Kc.

### Tall Fescue

- **Kc (Jan-Dec)**: 0.55 / 0.65 / 0.78 / 0.85 / 0.85 / 0.80 / 0.78 / 0.80 / 0.85 / 0.80 / 0.65 / 0.55
- **Root zone depth**: ~250 mm (6-12 in; deepest cool-season)
- **MAD**: 55%
- **Notes**: deep-rooted; most heat- and drought-tolerant cool-season grass.

### Perennial Ryegrass

- **Kc (Jan-Dec)**: 0.55 / 0.65 / 0.78 / 0.85 / 0.85 / 0.80 / 0.78 / 0.80 / 0.85 / 0.80 / 0.65 / 0.55
- **Root zone depth**: ~125 mm
- **MAD**: 50%
- **Notes**: quick germination; often used for winter overseeding in the south.

## Non-turf categories

### Ornamental shrubs

- **Citation**: UF/IFAS [ENH1115](https://edis.ifas.ufl.edu/publication/EP378), "Florida-Friendly Landscaping"
- **Kc**: 0.45-0.55 year-round (low seasonal variation)
- **Root zone depth**: ~250 mm
- **MAD**: 40%
- **Notes**: established shrubs use ~half the ET0 of turf. Water deeply + infrequently. Drip preferred.

### Vegetable garden

- **Kc**: 0.55 / 0.65 / 0.75 / 0.90 / 1.10 / 1.15 / 1.15 / 1.05 / 0.90 / 0.75 / 0.65 / 0.55
- **Root zone depth**: ~400 mm
- **MAD**: 45%
- **Notes**: critical at germination and fruit set. Mulch heavily to cut ET. Curve drawn from FAO-56 Table 12 (vegetables mid-season).

### Drip xeriscape

- **Kc**: 0.25-0.35 year-round
- **Root zone depth**: ~300 mm
- **MAD**: 30%
- **Notes**: established native plantings on drip. Water only during establishment / drought stress.

### Other / unknown

- **Kc**: 0.70 flat
- **Root zone depth**: 150 mm
- **MAD**: 50%
- **Notes**: generic placeholder. Override per zone with measured values.

## How LocalSky uses these

The catalog drives three things:

1. **ETc per zone per day**: `ET0 * Kc(species, day-of-year)`. Day-of-year interpolates linearly between mid-month anchor points with Dec/Jan wrap, so the curve is smooth across new year.
2. **Default root zone depth**: feeds TAW (Total Available Water) computation, which together with MAD sets the irrigation trigger threshold. Operators can override via `ZoneConfig.root_depth_mm`.
3. **Default MAD**: sets how dry the soil gets before LocalSky recommends watering. Override via `ZoneConfig.mad_pct_override`.

## Contributing a species

New species PRs welcome. Open a PR against [src/engine/species_catalog.rs](../src/engine/species_catalog.rs) with:

- 12 monthly Kc values (mid-month anchors)
- Default root zone depth (mm)
- Default MAD percentage
- A citation: FAO-56 Table 12, an Extension publication number, or a peer-reviewed paper. We don't accept "trust me" submissions.

The catalog stores `citation` and `notes` strings inline; the dashboard exposes them in the zone-editor's species picker so operators see provenance at pick time.
