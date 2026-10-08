# Design system — "Abstract Twilight"

Calm, precise, native. Apple-inspired hierarchy and restraint, translated from web guidance to a native Slint app: no fake
traffic lights, no unlicensed icons (all icons are original line drawings in `ui/components.slint`), the platform's own window
controls, the host's UI font.

* **Amber** = warmth and *active* focus (primary actions, the active destination, progress). **Purple** = the twilight environment
  (other computers, info). Gradients are atmosphere behind content only: two static washes (upper-left amber, lower-right violet),
  no animation, removed when transparency is reduced. Surfaces are solid.
* **Type**: system font; body 13 px, small 12, section 19, title 28; tracking tightens with size (−0.5 px at 28, +0.1 at 12);
  monospace only for fingerprints and diagnostics; the whole scale multiplies by Settings → Text size (100/115/130 %).
* **Layout**: 1120×760 default, 840×600 minimum, 208 px sidebar, 30 px page padding, 8-pt rhythm, 16 px panel radius, 10 px controls (36 px tall).
* **Window chrome (macOS)**: a unified title bar. The UI runs under a transparent, title-less system bar so the dark sidebar reaches the top
  edge and the traffic lights float over it (as in Finder or Notes) instead of a flat grey strip. A 28 pt strip is kept clear of content and acts
  as the drag handle (`WindowMoveArea`; double-click zooms). Windows and Linux keep their native frames. What was and was not checked in a real window: `docs/LEDGER.md`.
* **Motion**: 140 ms hover/press feedback (0 ms on press), 200 ms page cross-fade, 160 ms snap glide after dropping a screen;
  tiles follow the pointer 1:1 while dragged; nothing animates while idle; **Reduce motion** (system or setting) sets every duration to 0.
* **Accessibility**: every custom control sets an accessible role/label/state; Tab reaches buttons, switches, tabs, sliders and the
  layout canvas (arrow keys move the selected device; Shift for 1-point steps); a 2 px lavender focus ring offset 3 px; status is
  never colour alone (every badge has a shape *and* a word); targets are ≥ 32 px; tray state uses distinct *shapes* and labels.
  **Known gaps:** no screen-reader walkthrough was performed; Slint's accessibility is delegated to AccessKit and unverified here;
  there are no hover tooltips (icon-only buttons carry accessible names instead); the drag canvas has a keyboard alternative but no
  spoken position readout.

## Measured contrast (WCAG 2.x, `tests/design.rs`)

The starting palette was not trusted. Findings and adjustments:

| Token (dark) | Start | Measured | Adjusted |
|---|---|---|---|
| Muted text on elevated surface | `#92869F` | 4.56 : 1 (barely) | `#A598B4` → ≥ 5.77 : 1 on every surface |
| Border / control outline | `#3B314A` | 1.28–1.57 : 1 (fails 3 : 1) | split: decorative `#3B314A` stays; **control border `#75688E`** ≥ 3.07 : 1 |
| Text / secondary / amber / purple / lavender / success / warning / error | as specified | 5.7 – 17.5 : 1 | unchanged |
| Primary button label `#1A1008` on amber | — | 11.5 : 1 | — |

Light theme (warm near-white `#FBF7F2`, sidebar `#F3EDF7`, surface `#FFFFFF`): text `#1C1726` 15–17 : 1, secondary `#4A4258` 8–9 : 1,
muted `#6B6179` 5.1–5.8 : 1, amber text `#8A5300` ≥ 5.5 : 1, purple `#5B3FC0` ≥ 6.3 : 1, success `#1B7A52` ≥ 4.6 : 1, error `#B3263F`
≥ 5.6 : 1, control border `#8F849E` ≥ 3.06 : 1, focus ring `#5B3FC0` ≥ 6.3 : 1. The amber *fill* is used behind dark text (11.5 : 1), never as
light-theme text.
