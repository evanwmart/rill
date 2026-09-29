# Morning scene — Evan's Los Angeles

`scene-1.svg` … `scene-6.svg` are the sources (bitmap traces, black, in
transformed groups). The `0N-*.svg` files beside them are those sources
baked and coloured, and `scene.toml` stacks them: three distances, each a
solid mass plus its facet lines at the same depth.

Re-bake after editing a source:

    scripts/bake-scene.py assets/scenes/morning/scene-N.svg out/ --simplify 1.5 --min-area 0.0001

then set the fill in the baked file and copy it over `0N-*.svg`. Point the
theme at the scene file: `[desktop] wallpaper_scene = ".../morning/scene.toml"`.
Paths with hex fills in one viewBox is all the compositor reads; a night
layer (`night = true`, e.g. lit windows) is the obvious next addition.
