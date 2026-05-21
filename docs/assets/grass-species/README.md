# Grass species images

Photographic reference for each species in the catalog. Used by the
zone editor's species picker to give operators a visual confirmation
they're picking the right grass.

## Convention

One image per species slug, JPG, **1200x800** (3:2 landscape), under
200 KB after optimization. Filename matches the GrassSpecies variant
in src/config/schema.rs converted to snake_case:

- st_augustine.jpg
- bermuda.jpg
- zoysia.jpg
- bahia.jpg
- centipede.jpg
- kentucky_bluegrass.jpg
- tall_fescue.jpg
- perennial_ryegrass.jpg
- ornamental_shrubs.jpg
- vegetable_garden.jpg
- drip_xeriscape.jpg
- other.jpg

A 300x200 thumbnail variant (`-thumb.jpg`) accompanies each for the
species picker grid.

## Licensing

All images must be either:

- Operator-supplied with permission to license under CC-BY-SA-4.0, or
- Sourced from public-domain / Creative Commons-licensed collections
  (USDA, university extensions, Wikimedia Commons) with explicit
  attribution in IMAGE_CREDITS.md alongside this README

We do not bundle any image we cannot redistribute. The catalog renders
a stylized SVG placeholder when an image is missing so the picker
remains functional.

## Contributing images

Pull requests with new images welcome. Required:

1. Source + license declared in IMAGE_CREDITS.md
2. Both 1200x800 and 300x200 variants
3. Optimized (jpegoptim --strip-all + tight quality)
4. Verified against the species catalog (a Bahia photo labeled as
   St. Augustine is worse than no photo)

## Runtime serving

The docker build copies `docs/assets/grass-species/*.jpg` into the
container at `/app/site/grass-species/` (mapped to the URL path
`/grass-species/<slug>.jpg`). The species picker in the zone editor
loads them via standard `<img src="/grass-species/<slug>.jpg">`.

A 200 response means the image is bundled; a 404 triggers the SVG
fallback in `src/components/settings/zone_species_picker.rs` (planned
component; placeholder pattern documented here for future
contribution).
