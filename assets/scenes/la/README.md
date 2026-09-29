# Los Angeles — from a layer-separation key

`key.png` is the source: a flat greyscale image whose value bands are the
layers, generated from a reference photo with the "layer separation key"
prompt (brightest = sky, darkest = nearest). This one carries eleven bands
plus one the remap makes: the low-rise strip in front of downtown is
painted in band 4's grey, so rows 745 down in that grey are remapped to
value 92 and become band 9, nearer than the towers. Downtown spans bands 4
to 8; the lit windows go on 4 to 9 with panes of 2 or 3 key pixels, since
the tiers are one downtown and not five distances. Everything else here is
derived (the script needs a venv with vtracer, pillow and numpy, e.g.
`~/.cache/rill-scene-venv/bin/python`):

    scripts/scene-from-key.py assets/scenes/la/key.png assets/scenes/la/ --stops "#9a93b6,#5f5878,#1e1a2b" --remap 745-941:128-150=92 --lights 4:2,5:2,6:3,7:3,8:3,9:2 --holidays

which thresholds the bands into cumulative masks, traces them with vtracer,
thins them, colours them along a three-stop ramp and writes `scene.toml`
with depths far to near and the horizon measured from the last far band.
Edit the fills or depths in place, or change the stops and re-run. Point the
theme at `scene.toml`. `--lights` generates the lit windows for the named bands in three sunset
stages, and `--holidays` a set per holiday with a `when` date rule (the
everyday set is `no-holiday`). Preview a date on the compositor with
`RILL_DATE=MM-DD`.
