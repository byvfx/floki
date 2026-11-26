# Floki brand

NEO-3 Acid Arcade. The mark is three sheared planes that read as an F. Orange on top, acid green in the middle, electric blue on the bottom. No box around the letter. No yellow, amber, or Foundry Nuke gold.

The product line, used everywhere it introduces the tool:

> Inspect, compare, and play multi-layer image sequences with color-accurate OCIO viewing.

## Mark

The canonical drawings live in `brand/svg/`.

| File | Use |
| --- | --- |
| `floki-mark-dark.svg` | Bare mark on `#141518` / `#1C1D22`. Inks `#FF5500` / `#38FF4F` / `#00AAFF`. |
| `floki-mark-light.svg` | Bare mark on `#FFFFFF`. Inks `#FF5500` / `#00AA14` / `#0099E6`. |
| `floki-tile-dark.svg` | OS, dock, and window icon. Obsidian squircle (`#141518`, `rx="144"` on a 1024 grid) plus the dark mark. |
| `floki-tile-light.svg` | Light-tile companion. Not the dock icon. |
| `floki-mark-mono.svg` | One-color stamp. `fill="currentColor"`, default `#D8DCE4`. |
| `floki-lockup-dark.svg` / `floki-lockup-light.svg` | Mark plus `FLOKI`. |
| `floki-lockup-byvfx-dark.svg` / `floki-lockup-byvfx-light.svg` | Mark plus `FLOKI · BYVFX`. |
| `floki-spectral-rule.svg` | Three full-bleed bands, orange then green then blue, for dark surfaces. |

Raster exports (icons, social card, README banners) are produced by `brand/scripts/generate_assets.py`. Wordmarks are set in IBM Plex Sans, licensed under the SIL Open Font License (`brand/fonts/OFL.txt`).

## Color

Shared studio spine with BYVFX / BIF:

| Role | Hex |
| --- | --- |
| Canvas | `#141518` |
| Panel | `#1C1D22` |
| Elevated surface | `#262830` |
| Hairline | `#2E313B` |
| Primary ink on dark | `#D8DCE4` |
| Muted text | `#8C919E` |
| Light paper | `#F4F5F7` |
| Light paper, pure | `#FFFFFF` |

### Dark mark, on the chassis and the panel

Contrast is against `#141518` and `#1C1D22`.

| Plane | Hex | On chassis | On panel |
| --- | --- | --- | --- |
| Top orange | `#FF5500` | 5.70:1 AA | 5.25:1 AA |
| Middle acid green | `#38FF4F` | 13.55:1 AAA | 12.49:1 AAA |
| Bottom electric blue | `#00AAFF` | 7.12:1 AA | 6.56:1 AAA |

### Light mark, on white

WCAG 1.4.11 is a 3:1 floor for non-text graphics. The light inks are held just above that so the planes stay neon on `#FFFFFF` instead of collapsing into a dull green or a navy bar.

| Plane | Hex | On `#FFFFFF` |
| --- | --- | --- |
| Top | `#FF5500` | 3.21:1 |
| Middle | `#00AA14` | 3.11:1 |
| Bottom | `#0099E6` | 3.13:1 |

Those 3.1:1 inks are for the mark and other large non-text graphics. Small body and link text on `#FFFFFF` or `#F4F5F7` uses neutral ink: `#141518` primary, `#4A4E59` secondary, `#0070B8` links (all at least 4.5:1). Do not set a paragraph in acid green or orange.

### Pantone C

Nearest Formula Guide solids for the dark planes. These are screen sRGB approximations. Verify them against a physical Pantone Formula Guide before specifying anything for print.

| Plane | sRGB | Nearest Pantone C | ΔE |
| --- | --- | --- | --- |
| Orange | `#FF5500` | Orange 021 C | 1.1 |
| Green | `#38FF4F` | 802 C (neon spot) | 9.1 |
| Blue | `#00AAFF` | 299 C | 5.2 |

802 C is a reference for the neon, not a close match. ΔE 9.1 is too far to treat as a substitute without looking at a drawdown.

## Type

IBM Plex Sans for the wordmark and UI copy. IBM Plex Mono for frame numbers, paths, and code. Lockups are `FLOKI` and `FLOKI · BYVFX`, set in semibold / medium, not a second logotype.

## Clearspace and the lockup

Clearspace on every side of the bare mark is the height of the bottom plane (108 units on the 1024 grid, the band from y=626 to y=734). Do not put a container, keyline, or badge behind the bare mark. The squircle exists only on the OS tile.

The lockup sits the mark to the left of the word. The word shares the mark's optical center. `· BYVFX` is the muted secondary, same baseline, smaller size.

## App icon

The dock, taskbar, window, and `.ico` / `.icns` icon is always the dark obsidian tile. It is the one drawing that stays legible on both a light menu bar and a dark dock. 16×16 and 24×24 are hand-stepped: integer pixels, a 1 px gap between planes, a one-pixel shear. Larger sizes are supersampled from `floki-tile-dark.svg`.

## Chrome C

The in-app UI is an obsidian chassis (`#141518` canvas, `#1C1D22` panels) with silver/bone `#D8DCE4` focus rings and active highlights. Light mode is `#FFFFFF` / `#F4F5F7` paper, `#141518` text, `#0070B8` links. The Theme menu still offers Dark, Light, and System. Both themes are this chrome. Neither one introduces a brand hue into the widgets.

Chrome A (`#00AAFF` as a focus accent) is reserved in `brand/tokens.json` for a later toggle. It is not the shipping egui theme.

## Qt port — Direction C Final

The Qt stylesheets are a separate, locked port of this chrome (issue #44). They do not replace the egui theme above. Load exactly one sheet with `QApplication::setStyleSheet`. Do not cascade them. Tokens for all three live under `qt` in `brand/tokens.json`; the hexes in the sheets are those literals, repeated because Qt Style Sheets have no custom properties.

| Sheet | Theme menu | What it is |
| --- | --- | --- |
| `brand/floki-qt.qss` | Dark (primary default) | Direction C Final. Canvas `#141518`, panels `#1C1D22`, ink `#E6E9EF`, bone focus `#D8DCE4`. Dead-flat surround `#101114`. Studio-metal vertical gradients on the title, look bar, transport, and status. Recessed wells run `#0C0D10` → `#121318`. Play is `#FF6A1A` → `#E64800`. A 1px NEO-3 hairline sits under the FLOKI mark. Timeline lanes use the muted studio colors in `qt.themes.dark.timelineTracks`. |
| `brand/floki-qt-light.qss` | Light (default) | Acid Arcade Neon. Paper `#FFFFFF` / `#F4F6FB`, ink `#11131A`, surround `#22242B`. A 2px full-width spectral bar (`#FF5500` → `#38FF4F` → `#00AAFF` → `#FF2D87`) caps the window. Look-bar pills, the OCIO chip, and the fps/cache chips carry neon tints. Play runs `#FF6A1A` → `#FF2D87`. Lanes are `#00B8FF` / `#00E65A` / `#FF6B00` / `#D92BFF`. |
| `brand/floki-qt-light-studio.qss` | Studio Light (optional) | The quieter paper preset. `#FFFFFF` / `#F3F4F6`, ink `#141518`, surround `#2B2D33`. Same play disc as Dark. Lanes are desaturated studio tints. No full-width spectral bar. |

`QWidget#viewport` stays transparent in every sheet. The surround color belongs on `QWidget#viewportSurround`. HUD, A/B badges, the wipe handle, annotation notes, and the contact sheet stay on the dark plate, including in both light themes, because they sit on the picture.

Timeline lanes are 22px, `border-radius: 0`, edge to edge, with no label inside the bar. Pass names live in `QLabel#trackGutter`. The top lip is `border-top`; that is the QSS stand-in for the inset highlight.

QSS cannot paint a drop shadow, an outer glow, or a hatch. Each sheet says which of those are a `paintEvent` (or a `QGraphicsDropShadowEffect`): the play halo, the cache-proxy hatch (`-45°`, 3px on / 3px off), the missing-frame dash, and the playhead triangle. Load `brand/fonts/*.ttf` through `QFontDatabase` before the sheet. There is no `@font-face` in QSS.

The old `#paperCanvas` / `#paperPanel` object names still resolve. On the neon sheet they take the neon paper; on the studio sheet they take Studio Light. Dark no longer carries a second theme inside the same file.

### Viewport contract

The viewing room stays neutral so OCIO output is the only color on screen.

* The viewport backdrop stays the linear 0.10 / 0.20 checker, or the user's own solid or gradient. Do not tint it, and do not replace it with canvas `#141518`.
* Do not grade, overlay, or recolor rendered image pixels.
* Timeline cache greens stay `#346848` (proxy / beauty) and `#40A860` (full) in the egui theme, the dark Qt sheet, and Studio Light. Acid Arcade Neon uses the arcade pair on `qt.themes.lightNeon.cache`.
* Missing-frame holes stay `#CE5C3C`.
* Decode-warning amber stays `#FFC864`.
* Channel readouts stay `#FF5050` / `#50FF50` / `#6496FF` on dark surfaces. Both light Qt sheets use the paper-safe set on `qt.themes.*.readout` so the status line stays legible.

Brand color lives in the icon, the About mark, and the docs. It does not live in the viewport.

## Do

* Use the dark tile for any OS-facing icon.
* Use the light inks when the bare mark sits on white.
* Keep the three planes in order: orange, green, blue, top to bottom.
* Keep a gap between the planes so the three colors remain three colors at 16 px.

## Don't

* Don't draw a box, circle, or wordmark container around the F.
* Don't recolor a plane, swap their order, or substitute yellow, amber, or Nuke gold.
* Don't set small body text in the neon inks.
* Don't paint brand orange, green, or blue onto the egui panels, focus rings, or the viewport image. The Qt sheets under "Qt port" are the locked exception, and they still leave the rendered pixels alone.
* Don't retint the functional timeline, warning, or readout colors to "match the brand."
