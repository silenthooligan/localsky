# LocalSky Design System (the spine)

The conventions every screen follows. P3-10 migrates the existing UI onto this spine
incrementally; a CI grep gate (below) keeps drift from regrowing once the migration
is clean. This document is the source of truth a reviewer approves **once**, so the
per-component migration is mechanical, not 163 individual judgment calls.

## Primitives

Reach for a primitive before writing a raw element.

### `<Button>` (`components/ui/button.rs`)
Props: `variant`, `size` (sm/md/lg), `icon`, `disabled`, `loading`, `block`, `href`
(renders an `<a>`), `on_click`, `aria_label`, children.

| variant | use for | examples |
|---|---|---|
| `primary` | the ONE main action of a view | Save, Apply, Run zone |
| `success` | a positive "go / confirm / install" action | Install, Confirm, Enable |
| `secondary` | a real alternative action | Cancel (when it's a choice), Reset, Add another |
| `ghost` | low-emphasis / tertiary / toolbar | Print, Download CSV, dismiss, "show more" |
| `danger` | destructive / irreversible | Delete, Remove, Revoke token |

Resolved: `success` (solid teal gradient, mirroring `primary`'s prominence) was added
so the legacy green `.btn-clay-good` "go" actions keep a meaningful affordance instead
of flattening into the accent-blue `primary`.

### `<Card>` (`components/ui/card.rs`)
The grouped-content surface: `elev-1` fill, hairline border, specular lit edge,
`radius-lg`. Props: `compact` (tighter padding), `interactive` (hover lift, for
clickable cards), `accent` (the grad-flow identity stripe along the top, for hero /
featured cards). Migrate bespoke card-like divs onto it; keep genuinely one-off
surfaces (the emergency Stop-All panel) bespoke.

## What to migrate vs leave specialized

**Migrate** to `<Button>`: generic primary/secondary/ghost/danger actions, the bulk
of settings Save/Reset/Add/Delete, dialog confirm/cancel, toolbar actions.

**Leave specialized** (a generic button would regress the design or the affordance):
- **Segmented / toggle groups** (override Auto/Skip/Run, verdict toggles). These are
  radio-like controls, not buttons. (Candidate for a future `<SegmentedControl>`.)
- **Preset chips** (rain-delay 24h/48h/72h). (Candidate for a future `<ChipGroup>`.)
- **Emergency Stop-All**: safety-critical and intentionally distinctive; keep its
  prominent bespoke style rather than flatten it into a standard `danger` button.
- **Icon-only affordances**: the `×` close on prompts/sheets (already conventionalized
  per surface).

When in doubt: if it's a standalone "do this thing" action, it's a `<Button>`; if it's
part of a custom control (a group, a chip rail, a slider), it stays.

## Spacing

Use the `--space-0 … --space-9` scale. No bare `px`/`rem` for spacing. Allowed bare
values: hairlines (`1px`/`2px` borders, dividers), `0`, and `1px`-class visual details.

## Breakpoints: 3 tiers

Collapse the scattered `max-width` values (600/560/540/720/860/920/980/1024/1440…) onto
three layout tiers. Orthogonal queries (`prefers-reduced-motion`, `prefers-color-scheme`,
`prefers-contrast`/`forced-colors`, `print`) are NOT tiers and stay as-is.

| tier | query | layout |
|---|---|---|
| phone | `(max-width: 760px)` | single column; bottom tab-bar nav; sidebar hidden |
| tablet | `(min-width: 761px) and (max-width: 1100px)` | 64px icon rail sidebar |
| desktop | `(min-width: 1101px)` | full sidebar + content |

## Z-index

One named ladder (`--z-*`); never hardcode a raw z-index. Higher tier always wins.

| token | value | layer |
|---|---|---|
| `--z-base` | 1 | in-flow raised bits (active cells, chips) |
| `--z-raised` | 10 | sticky headers / mobile app bar |
| `--z-nav` | 20 | bottom tab-bar |
| `--z-dropdown` | 100 | popovers, menus, tooltips |
| `--z-backdrop` | 200 | modal/sheet scrims |
| `--z-modal` | 300 | sheets, dialogs |
| `--z-toast` | 400 | toasts / notifications |

(New code uses these now; the existing scattered raw z-indexes are retrofitted in a
separate visually-verified pass so stacking order isn't disturbed blind.)

## Viewport height

Use `100dvh` (with a `100vh` fallback line before it) for full-viewport heights so
mobile browser chrome doesn't clip the layout.

## CI grep gate (live, the CI quality-gate job)

- **HARD FAIL**: any `btn-clay` in `src/` or `style/` (the legacy family is retired;
  comments exempt). Prevents regrowth.
- **Informational** (warn): the raw `<button>` count still migrating onto `<Button>`.
  Flips to hard-fail when it reaches ~0 (then a bare `px`/`rem` SCSS check joins it).

## Migration status

Incremental, one surface per slice, each visually verified.

Done: `100vh → 100dvh`; the spine (success variant, styled Card, z-scale, Button
`class` prop); ~79 settings + wizard + form buttons → `<Button>`; **`.btn-clay*` fully
deleted**; the install CTA → `success`; the CI hard-fail gate.

Remaining: bespoke-class surfaces (login, page header, radar, the irrigation controls'
own specialized buttons), the breakpoint consolidation (11 → 3 tiers), the z-index
retrofit onto `--z-*`, and the ~35 hardcoded colors → tokens.
