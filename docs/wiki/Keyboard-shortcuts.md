Single-key shortcuts are suppressed while a text field has focus, and while the contact sheet is open. `F11` and `Esc` stay live in both cases: `Esc` has to cancel an annotation even when the annotation text field is focused, and `T` is handled above that gate so it can close the sheet it just opened.

| Shortcut | Action |
|---|---|
| `F` | Frame the image (reset zoom and pan to fit) |
| `R` | Isolate red |
| `G` | Isolate green. Plain `G` only — `Shift + G` is the gamma reset |
| `B` | Isolate blue |
| `A` | Isolate alpha |
| `C` | RGB |
| `T` | Show or hide the contact sheet |
| `E` | Reset exposure to 0 EV |
| `Shift + G` | Reset gamma to 1.0 |
| `F11` | Toggle fullscreen (hides the menu bar and panels) |
| `Cmd/Ctrl + Shift + S` | Snapshot the current view to the clipboard |
| `Esc` | Cancel the active annotation tool, or leave fullscreen if nothing was in progress |
| `Shift + Click` | Sample the pixel and save it to the swatch palette |
| Scroll wheel | Zoom at the cursor |
| Click and drag | Pan |
| Drag and drop | Add EXRs as layers |

## With a sequence loaded

These keys are consumed only when a sequence is active, and not while a text field wants the keyboard (the fps field, for example).

| Shortcut | Action |
|---|---|
| `Space` | Play / pause |
| `←` / `→` | Step one frame back / forward |
| `I` / `O` | Set the in / out points of the play range |

Without a sequence, `Space` is not consumed and toggles blink compare. With a sequence, it is consumed so blink does not also fire. See [Comparison](Comparison) and [Playback](Playback).

The interface theme (Light, Dark, System) is the **Theme** menu, not a shortcut. System tracks the OS setting. The choice persists.
