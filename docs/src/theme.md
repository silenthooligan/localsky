# Theme

The theme picker sets how LocalSky looks on **this device**. It is a
per-browser preference, not a per-deployment setting: your choice is
stored in this browser's `localStorage` and no config is written, so two
people looking at the same install can each pick their own theme.

Find it under Settings, then Theme. Pick a card and it applies
instantly, no reload. A tiny boot script reads your saved theme before
the first paint, so the page never flashes the wrong colors on reload.

## The four presets

- **Dark** (the default): the house look, glass panels over deep blue.
- **Light**: a hand-tuned light theme, the same panels lifted to a bright
  background.
- **Auto**: follow your operating system's light/dark preference and
  switch with it.
- **High contrast**: pure black on pure white with the glass effects
  dropped, for maximum legibility.

Because the choice lives in the browser, it does not travel with a
backup or sync to your other devices; set it once per browser. Clearing
site data resets you to Dark.
