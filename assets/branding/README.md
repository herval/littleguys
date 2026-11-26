# Ghost Wave

[Original artwork](ghost-wave.png) · [ImageGen prompt](ghost-wave-prompt.txt) · [Menu-bar SVG](ghost-wave-template.svg) · [1× PNG](ghost-wave-template.png) · [2× PNG](ghost-wave-template@2x.png)

`ghost-wave.png` is the approved 1254 × 1254 original, generated with the built-in ImageGen tool from the saved prompt, with a transparent background and no input image. The desktop icon uses this PNG directly as its source. The generator fits the artwork inside an 832 px square on a transparent 1024 px canvas for the Dock.

The standalone black template shows the original wave. The menu bar uses a different silhouette for each state. All templates have transparent eye cutouts and a logical size of 22 × 18 pt. macOS tints them for the menu bar, and the Windows tray adapts the same frames to the taskbar.

| Menu-bar state | Motion | Vector |
| --- | --- | --- |
| Idle | Stands with its arms down, blinks, then glances sideways with one hand on its hip. | [Idle SVG](menu-bar/idle.svg) |
| Working | Leans into a glide, paddling forward with its hem streaming behind it. | [Working SVG](menu-bar/working.svg) |
| New reply | Waves one arm with smiling eyes. | [New reply SVG](menu-bar/new-reply.svg) |
| Needs you | Lifts both arms overhead beside the exclamation mark. | [Needs you SVG](menu-bar/needs-you.svg) |
| Offline | Curls up with its head on its arms and breathes as the sleep marks rise. | [Offline SVG](menu-bar/offline.svg) |
| Connecting | Peeks over a ledge, looking from side to side as the dots fill. | [Connecting SVG](menu-bar/connecting.svg) |

Run `pnpm install --frozen-lockfile` and `pnpm icons` from the repository root to rebuild the assets. The generator exports the desktop PNG, ICNS and ICO files to `src-tauri/icons/`, the 17 status frames to `src-tauri/assets/tray/`, the six pose SVGs to `assets/branding/menu-bar/`, and the freestanding mascots to `src/ui/`. Edit the outlines in the [pose source](ghost-wave-poses.mjs). Frame names and animation timing are defined in the [tray animator](../../src-tauri/src/tray.rs).
