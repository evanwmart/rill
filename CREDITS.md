# Credits & third-party assets

Everything in this repository is original work or clearly-licensed
third-party material, listed here. Rust crate dependencies are licensed
via their own manifests (`cargo license` for the full report); this file
covers creative assets — fonts, icons, shaders — and the published
techniques the code implements.

## Fonts

- **Atkinson Hyperlegible Next** and **Atkinson Hyperlegible Mono** —
  Braille Institute of America. SIL Open Font License 1.1; license text
  ships with the fonts (`fonts/*/OFL.txt`). Embedded via
  `include_bytes!` in `rill-gpu`.

## Icons

- **Phosphor Icons** (regular weight, v2.0.2) — Helena Zhang, Tobias
  Fried; MIT license, text vendored at `crates/rill-ui/phosphor/LICENSE`.
  Vendored by `scripts/vendor-icons.sh` from `phosphor-icons/core`, then
  flattened into Rill's own icon format.

## Shaders

All shaders in this repository were written for Rill against its
FX/particle preambles and are covered by the repository license.
Published techniques are credited where they have names — algorithms and
ideas, not code lineage:

| File | Technique credit |
|---|---|
| `slime_update/diffuse/draw.wgsl` | Physarum transport networks — Jeff Jones, "Characteristics of pattern formation and evolution in approximations of Physarum transport networks" (2010); popularized by Sebastian Lague's slime simulation |
| `boids_compute/render.wgsl` (rill-gpu) | Boids — Craig Reynolds (1987) |
| `kawase.wgsl` (rill-gpu) | Dual-Kawase blur — Masaki Kawase (GDC 2003); dual-filter variant, Marius Bjørge (SIGGRAPH 2015) |
| `crt.wgsl`, `vhs.wgsl`, `pixel.wgsl` | Common CRT/tape/mosaic post-processing idioms; implementations original |
| `matrix.wgsl` | Digital-rain idiom; self-contained procedural implementation |
| `night.wgsl`, `vignette.wgsl`, `hue.wgsl` | Elementary color transforms |
| `spectrum.wgsl`, `window_aura.wgsl` | Original, driven by the compositor's audio FFT uniforms |
| `lofi.wgsl`, `showroom.wgsl` | Original scenes |
| `dust_update/draw.wgsl` | Original particle field |

And a broad tip of the hat to the Shadertoy community for making
realtime shader technique legible in the first place.
