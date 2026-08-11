# Mark

**Viewport** — four corner brackets around a frame that is never drawn.

| File | Use |
|---|---|
| `mark.svg` | Light ground. The accented bracket is `signal-teal` `#00A38C` — the app's primary accent, and its default |
| `mark-red.svg` | The `xr-red` `#E65B58` accent, one of the three Preferences ▸ Accent colour offers |
| `mark-cobalt.svg` | The cobalt `#1B62E8` accent, the third of the three |
| `mark-dark.svg` | Dark ground, retained and unreachable. Frozen at `xr-red` `#C8102E`, a different value from the light coral rather than a filter |
| `mark-mono.svg` | Inherits `currentColor`; menus, print, disabled states |

The three accent variants differ in **one bracket only** — the other three stay `ink`
`#1A1D1F`. The accented corner is what gives the mark a reading order rather than
anonymous four-fold symmetry, so recolouring the rest would not be a variant, it would be
a different mark. `docs/images/logo-accents.svg` shows all three side by side.

**These files do not follow the runtime setting.** The in-app mark is drawn from
`palette.accent` and does; a static asset and the generated `.icns` cannot.

## Rules

- **Clear space**: one bracket arm (12 units at the 48-unit grid) on every side.
- **Minimum size**: 16px. Below that the negative space closes and it stops reading.
- **Never** recolour the brackets individually, add a container, rotate it, or place it on a busy image without a solid ground.
- **Lockup**: mark, then a gap of one bracket arm, then `velm` — always lowercase, tracking −0.045em, in the platform interface face so the app reads as native rather than branded.

The mark is the same shape as the canvas selection indicator. That is the point, not a coincidence — see `docs/05-design-language.md`.
