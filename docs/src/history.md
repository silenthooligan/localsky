# History

Every run, every skip, every decision, kept locally and rendered as a
story instead of a log.

- **Watered minutes per day** across the selected window (30/90/365
  days), with the per-zone split below.
- **Watering calendar**: one square per day; greener = more water,
  empty = a skip day.
- **Why it skipped**: the engine's decisions aggregated by reason
  (rain, wind, restriction, cold, soil), so a season of judgment is
  one glance.
- **Per-zone rows** with sparklines, for spotting a zone that's
  drifting from its siblings.

The Print button turns the page into a clean seasonal report. Data
lives in LocalSky's own SQLite store; nothing depends on a cloud
service or another system's recorder.
