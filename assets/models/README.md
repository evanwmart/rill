# Models

Sidecars for showroom models: a shader and a hint file per model. The
meshes themselves are never tracked — drop your own in
`~/.config/rill/models` and they appear in Theme Studio's Showroom.
Only the generic toon shader ships with the repo.

## What a model is

Three files sharing a stem, in `~/.config/rill/models`:

| file | what it does |
| --- | --- |
| `Name.obj` / `Name.stl` / `Name/` | the mesh — one file, or a directory of parts loaded as one mesh with a material id per file |
| `Name.wgsl` | its shader: materials, palette, the look that belongs to this model |
| `Name.toml` | scene hints applied when it is chosen — `model_up`, `model_scale`, `model_lift`, `spin_phase` |

Only the mesh is required. Without a `.wgsl` a model wears
[`figure_toon.wgsl`](figure_toon.wgsl), the generic auto-fitting toon
shader; without a `.toml` it uses the scene's current framing.

Drop a mesh in that folder and it appears in Theme Studio's **Showroom**
section. Copy any `.wgsl` here beside it to give it its own materials.

## Why meshes are not in git

They are large and third-party, with licences that are not ours to
redistribute. Sidecar shaders and hints you author can live in
`~/.config/rill/models` next to the meshes; only the generic toon
shader ships here.

## Writing one

Start from `figure_toon.wgsl` and replace `surface_color`. It receives
the part id, the height through the model (`up`, 0 at the feet), the
normal, and the world position — enough to zone a mesh that carries no
UVs. Everything else (auto-fit, the turntable, the studio's lights, the
floor reflection) is already wired to `[desktop.showroom]`.
