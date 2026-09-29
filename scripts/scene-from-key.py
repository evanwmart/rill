#!/usr/bin/env python3
"""Split a layer-separation key into a scene the compositor can paint.

    scene-from-key.py KEY.png DEST_DIR/ [--stops "#far,#mid,#near"] [--simplify TOL] [--median R]
                     [--lights 4,5[:PX]] [--holidays] [--remap ROW0-ROW1:V0-V1=NEW]... [--lights-only]

`--holidays` also writes a light set per holiday for every `--lights` band —
hearts for Valentine's, greens for St Patrick's, yellow and blue stripes for
Easter, a flag for the Fourth, the pride stripes for June, a jack-o'-lantern
for Halloween week, red and green for Christmas week, party colours for New
Year — each with a `when` date rule, and marks the everyday set
`no-holiday` so it steps aside on those days.

`--lights` names layers (1 = farthest) that are buildings: for each, a
night layer of lit windows is generated from that band's own pixels — a
grid of small warm rectangles, about 40% of them on — and placed right
after the band in the paint order, so nearer layers occlude it and it fades
in only when the sky goes dark.

A key is a flat image whose value bands are the layers, brightest farthest
(the sky) to darkest nearest — what the "layer separation key" prompt asks
an image model for. This finds the bands from the histogram, builds one
CUMULATIVE mask per band (this band and everything nearer), so every layer
runs to the bottom edge and occlusion comes out right, traces each mask
with vtracer in spline mode at tight corner and splice angles (polygon
mode, and lax angles, straighten anything under a sixty-degree corner:
palm fronds came out as blobs and rooftops rounded), flattens and thins it
through bake-scene's simplifier at 0.25 px, colours it from a ramp, and
writes NN-band.svg plus scene.toml with depths spaced far to near and the
horizon measured from the farthest band. `--median R` smooths each mask
with a (2R+1)² median first: 1 by default (3×3 takes the specks and keeps a
palm frond), 0 for none; the old 5×5 erased every feature thinner than it.

Needs vtracer, Pillow and numpy (a venv is fine: python -m venv v &&
v/bin/pip install vtracer pillow numpy).
"""
import os, re, shutil, subprocess, sys, tempfile

import numpy as np
from PIL import Image, ImageDraw, ImageFilter
import vtracer

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import importlib.util
_spec = importlib.util.spec_from_file_location("bake_scene", os.path.join(os.path.dirname(os.path.abspath(__file__)), "bake-scene.py"))
bake_scene = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(bake_scene)


def hex_to_rgb(h):
    h = h.lstrip("#")
    return tuple(int(h[i : i + 2], 16) for i in (0, 2, 4))


def ramp(stops, t):
    """Colour at t in 0..1 along a list of hex stops."""
    cols = [hex_to_rgb(s) for s in stops]
    if len(cols) == 1:
        return "#%02x%02x%02x" % cols[0]
    x = t * (len(cols) - 1)
    i = min(int(x), len(cols) - 2)
    f = x - i
    c = tuple(round(cols[i][k] + (cols[i + 1][k] - cols[i][k]) * f) for k in range(3))
    return "#%02x%02x%02x" % c


def smooth(mask, radius=2):
    """A median over a (2r+1)² window: specks and hairlines go, edges stay."""
    img = Image.fromarray(np.where(mask, 255, 0).astype(np.uint8)).copy()
    img = img.filter(ImageFilter.MedianFilter(size=2 * radius + 1))
    return np.asarray(img) > 127


def gap_cuts(lum, bands):
    """The value that separates each pair of neighbouring bands: per gap,
    the valley of the histogram between them. Anti-aliased edge pixels are
    spread evenly across a gap, but the haze a model paints *within* an
    object — a face a few values off its band — is a cluster, and a fixed
    cut lands in it as often as not: a face at 87..89 against a cut at 87.5
    came out as a ragged shape with holes. The valley sits where the
    clusters are not. A flat gap falls back to three quarters of the way
    towards the brighter band, so an edge pixel joins the nearer layer and
    fattens a silhouette by under a pixel rather than fringing it.
    Returns cuts[k]: pixels <= cuts[k] are darker than bands[k].
    """
    h = np.bincount(lum.ravel(), minlength=256).astype(np.float64)
    h = np.convolve(h, [0.25, 0.5, 0.25], mode="same")
    cuts = []
    for k in range(len(bands) - 1):
        brighter_lo, darker_hi = bands[k][0], bands[k + 1][1]
        inner = list(range(darker_hi + 1, brighter_lo))
        default = darker_hi + int(round((brighter_lo - darker_hi) * 0.75))
        if len(inner) < 3:
            cuts.append(default)
            continue
        counts = h[inner]
        # Flat if the valley is not clearly below the gap's mean count.
        if counts.min() > 0.6 * counts.mean():
            cuts.append(default)
            continue
        cuts.append(inner[int(counts.argmin())])
    return cuts


def fill_holes(mask):
    """Fill every background region not connected to the sky.

    A cumulative mask's legitimate gaps — sky between a crane's arm and its
    tower — reach the top of the image. An enclosed pocket can only be a
    speck in the key that fell on the far side of the cut, and traced it
    becomes an inner contour that even-odd fill punches straight through
    to the layer behind. Flood the background from the edges; whatever is
    left unreached is a hole, and becomes land.
    """
    h, w = mask.shape
    # `.copy()`: an image over a numpy buffer is read-only and floodfill
    # would silently write nothing.
    img = Image.fromarray(np.where(mask, 255, 0).astype(np.uint8)).copy()
    seeds = [(x, 0) for x in range(0, w, max(1, w // 64))]
    seeds += [(x, y) for y in range(0, h, max(1, h // 32)) for x in (0, w - 1)]
    for x, y in seeds:
        if img.getpixel((x, y)) == 0:
            ImageDraw.floodfill(img, (x, y), 128)
    holes = np.asarray(img) == 0
    if holes.any():
        print(f"  filled {int(holes.sum())} hole pixels")
    return mask | holes


def bands_of(lum, min_share=0.003):
    """Value bands as (lo, hi) ranges, brightest first, from histogram peaks."""
    h = np.bincount(lum.ravel(), minlength=256)
    total = lum.size
    groups = []
    for v in range(256):
        if h[v] < total * min_share:
            continue
        if groups and v - groups[-1][-1] <= 4:
            groups[-1].append(v)
        else:
            groups.append([v])
    return [(g[0], g[-1]) for g in reversed(groups)]


# ---------------------------------------------------------------- holidays

HOLIDAYS = [
    # name, when, tones (hex, weight), pattern
    ("valentines", "02-13..02-14", [("#ff2e63", 3), ("#ff5c8a", 3), ("#ff9fbf", 2)], "heart"),
    ("stpatricks", "03-17", [("#2ecc71", 3), ("#1e9e5a", 3), ("#8dff9c", 2), ("#c9ff7a", 1)], "random"),
    ("easter", "easter", [("#ffe066", 1), ("#9ad8ff", 1), ("#ffb3d9", 1), ("#c9a8ff", 1), ("#a8f0c8", 1)], "egg"),
    ("july4", "07-03..07-04", [("#ff3b3b", 1), ("#ffffff", 1), ("#3b6bff", 1)], "flag"),
    ("pride", "06-01..06-30", [("#e40303", 1), ("#ff8c00", 1), ("#ffed00", 1), ("#008026", 1), ("#24408e", 1), ("#732982", 1)], "stripes"),
    ("halloween", "10-24..10-31", [("#ff8c1a", 4), ("#ffb347", 2), ("#7a3fbf", 1)], "pumpkin"),
    ("christmas", "12-15..12-26", [("#ff3b3b", 9), ("#2ecc71", 9), ("#ffffff", 2)], "random"),
    ("newyear", "12-31..01-01", [("#ff5cc8", 1), ("#4dc3ff", 1), ("#b6ff3b", 1), ("#b25cff", 1), ("#ffffff", 1)], "random"),
]


def in_heart(u, v):
    """(u, v) in -1..1 box → inside a heart, point down."""
    x, y = u * 1.25, -v * 1.25 + 0.15
    return (x * x + y * y - 1) ** 3 - x * x * y * y * y <= 0


def in_tri(p, a, b, c):
    def sgn(p1, p2, p3):
        return (p1[0] - p3[0]) * (p2[1] - p3[1]) - (p2[0] - p3[0]) * (p1[1] - p3[1])
    d1, d2, d3 = sgn(p, a, b), sgn(p, b, c), sgn(p, c, a)
    return not ((d1 < 0 or d2 < 0 or d3 < 0) and (d1 > 0 or d2 > 0 or d3 > 0))


def in_pumpkin_face(u, v):
    """(u, v) in -1..1 box → inside an eye, the nose or the mouth."""
    if u * u + v * v > 1.0:
        return False
    p = (u, v)
    eye = in_tri(p, (-0.55, -0.15), (-0.2, -0.15), (-0.375, -0.55)) or in_tri(p, (0.2, -0.15), (0.55, -0.15), (0.375, -0.55))
    nose = in_tri(p, (-0.12, 0.2), (0.12, 0.2), (0.0, -0.05))
    # A grin: a band with two square teeth cut out.
    mouth = 0.35 <= v <= 0.62 and abs(u) <= 0.7 - 0.5 * (v - 0.35) and not (
        (0.35 <= v <= 0.46 and -0.35 <= u <= -0.15) or (0.5 <= v <= 0.62 and 0.1 <= u <= 0.3)
    )
    return eye or nose or mouth


def svg_mask(path, w, h):
    """The filled area of an SVG as a boolean array at w×h, via Inkscape:
    the same renderer the eye has been judging the file in. Missing file →
    nothing filled."""
    if not os.path.exists(path):
        return np.zeros((h, w), dtype=bool)
    tmp = tempfile.mkdtemp(prefix="svg-mask-")
    png = os.path.join(tmp, "m.png")
    subprocess.run(
        ["inkscape", path, "--export-type=png", f"--export-filename={png}", "-w", str(w), "-h", str(h)],
        check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    alpha = np.asarray(Image.open(png).convert("RGBA"))[:, :, 3] > 127
    shutil.rmtree(tmp)
    return alpha


def largest_rect(mask):
    """The largest all-True axis-aligned rectangle in `mask`, as
    (x0, y0, w, h): the widest solid tower face, where a shape can be drawn
    without the skyline cutting through it. Histogram-and-stack, O(w·h)."""
    h, w = mask.shape
    heights = np.zeros(w, dtype=int)
    best = ((0, 0), 0, 0, 0, 0)  # (square side, area), x0, y0, w, h
    for y in range(h):
        heights = np.where(mask[y], heights + 1, 0)
        stack = []
        for x in range(w + 1):
            cur = heights[x] if x < w else 0
            start = x
            while stack and stack[-1][1] > cur:
                sx, sh = stack.pop()
                rw = x - sx
                # The face that can hold the biggest square wins; area
                # breaks ties, so a shape gets width, not just height.
                score = (min(rw, sh), rw * sh)
                if score > best[0]:
                    best = (score, sx, y - sh + 1, rw, sh)
                start = sx
            stack.append((start, cur))
    return best[1:]


def holiday_tone(pattern, tones, rng, u, v, row, col, nrows):
    """The tone for a window at (u, v) in the band's box, or None for dark.
    `row`/`col` index the window grid (rows top to bottom) for stripe and
    dot patterns; `nrows` is the grid's height."""
    names = [t for t, _ in tones]
    weights = np.array([w for _, w in tones], dtype=float)
    weights /= weights.sum()
    pick = lambda: names[int(rng.choice(len(names), p=weights))]
    if pattern == "random":
        return pick() if rng.random() < 0.42 else None
    if pattern == "stripes":
        # Equal horizontal bands over the band's height, in tone order.
        band = min(len(names) - 1, int(((v + 1) / 2) * len(names)))
        return names[band] if rng.random() < 0.6 else None
    if pattern == "egg":
        # A decorated egg down every building: a two-row stripe, then a row
        # of dots on alternate columns, then the next stripe in the next
        # pastel, repeating. Stripes are nearly solid; dots always on.
        period = 3
        k = row // period
        phase = row % period
        stripe = names[k % len(names)]
        dot = names[(k + 2) % len(names)]
        if phase < 2:
            return stripe if rng.random() < 0.92 else None
        return dot if (col + k) % 2 == 0 else None
    elsewhere = abs(u) > 1 or abs(v) > 1  # not on the shape's facade
    if pattern == "flag":
        red, white, blue = names[0], names[1], names[2]
        if elsewhere:
            return pick() if rng.random() < 0.3 else None
        if u < -0.2 and v < 0.08:
            return blue if rng.random() < 0.85 else (white if rng.random() < 0.5 else None)
        return (red if row % 2 == 0 else white) if rng.random() < 0.9 else None
    if pattern == "heart":
        if elsewhere:
            return pick() if rng.random() < 0.3 else None
        return pick() if in_heart(u, v) and rng.random() < 0.95 else None
    if pattern == "pumpkin":
        if elsewhere:
            return pick() if rng.random() < 0.3 else None
        if in_pumpkin_face(u, v):
            return "#fff0a0" if rng.random() < 0.97 else None
        # The rest of the pumpkin's disc, dim orange, so the disc reads too.
        return names[0] if u * u + v * v <= 1.0 and rng.random() < 0.35 else None
    return None


def main():
    args = sys.argv[1:]
    stops = ["#8f8aa6", "#b8afa1", "#2f5a3c"]
    simplify = 0.25
    median = 1
    lights = set()
    holidays = "--holidays" in args
    if holidays:
        args.remove("--holidays")
    pane = {}
    if "--lights" in args:
        # `4,5,6:3`: band numbers, each with an optional pane width in key
        # pixels. Without one the width falls out of the band's depth, which
        # is right when tone means distance and wrong when a model paints
        # one downtown in five tones: those want the same panes.
        i = args.index("--lights")
        for x in args[i + 1].split(","):
            if not x.strip():
                continue
            band, _, px = x.partition(":")
            lights.add(int(band))
            if px:
                # `3` or `3x5`: width, or width and height, in key pixels.
                pw, _, ph = px.partition("x")
                pane[int(band)] = (int(pw), int(ph) if ph else max(2, int(round(int(pw) * 1.5))))
        del args[i : i + 2]
        # Nearer is bigger, no exceptions: a lit band's panes must be larger
        # in both dimensions than the farther lit band's, or a viewer reads
        # the smaller windows as the farther building.
        prev = None
        for band in sorted(lights):
            cur = pane.get(band)
            if prev and cur:
                (pw, ph) = prev[1]
                larger = cur[0] >= pw and cur[1] >= ph and (cur[0] > pw or cur[1] > ph)
                if not larger:
                    sys.exit(f"--lights: band {band} panes {cur[0]}x{cur[1]} are not larger than band {prev[0]}'s {pw}x{ph}")
            if cur:
                prev = (band, cur)
    # `--lights-only`: write the NNb-lights files and nothing else — no band
    # SVGs, no scene.toml — for a scene whose bands have been fixed by hand
    # and must not be regenerated. The windows still sit on the key's
    # facades, so a band moved by hand may leave a few of them adrift.
    lights_only = "--lights-only" in args
    if lights_only:
        args.remove("--lights-only")
    remaps = []
    while "--remap" in args:
        # `745-941:128-150=92`: pixels in those rows with those values take
        # the new value before the bands are found. For the region a key
        # gets wrong — a low-rise strip painted in a far tier's grey, which
        # would render pale and distant in front of the dark towers. The
        # new value falls between two bands and becomes a band of its own.
        i = args.index("--remap")
        rows, _, rest = args[i + 1].partition(":")
        vals, _, new = rest.partition("=")
        r0, r1 = (int(v) for v in rows.split("-"))
        v0, v1 = (int(v) for v in vals.split("-"))
        remaps.append((r0, r1, v0, v1, int(new)))
        del args[i : i + 2]
    if "--stops" in args:
        i = args.index("--stops")
        stops = [s.strip() for s in args[i + 1].split(",")]
        del args[i : i + 2]
    if "--simplify" in args:
        i = args.index("--simplify")
        simplify = float(args[i + 1])
        del args[i : i + 2]
    if "--median" in args:
        i = args.index("--median")
        median = int(args[i + 1])
        del args[i : i + 2]
    if len(args) != 2:
        sys.exit(__doc__)
    src, dest = args
    os.makedirs(dest, exist_ok=True)
    im = Image.open(src).convert("L")
    lum = np.asarray(im).copy()
    w, h = im.size
    for r0, r1, v0, v1, new in remaps:
        sel = np.zeros_like(lum, dtype=bool)
        sel[r0 : r1 + 1] = (lum[r0 : r1 + 1] >= v0) & (lum[r0 : r1 + 1] <= v1)
        lum[sel] = new
        print(f"  remapped {int(sel.sum())} pixels in rows {r0}-{r1}, values {v0}-{v1} -> {new}")
    bands = bands_of(lum)
    if len(bands) < 2:
        sys.exit("fewer than two value bands: not a key")
    sky, layers = bands[0], bands[1:]
    n = len(layers)
    print(f"{src}: sky {sky}, {n} layers, canvas {w}x{h}")
    horizon = None
    toml = ["# generated by scripts/scene-from-key.py — edit colours and depths freely", "", "[sky]"]
    tmp = tempfile.mkdtemp(prefix="scene-key-")
    cuts = gap_cuts(lum, bands)
    print(f"  cuts between bands: {cuts}")
    for i, (lo, hi) in enumerate(layers):
        # Cumulative: this band and every darker (nearer) one, so every
        # layer runs to the bottom edge and occlusion comes out right. The
        # cut to the brighter band is the histogram valley (gap_cuts).
        mask = lum <= cuts[i]
        if median:
            mask = smooth(mask, median)
        mask = fill_holes(mask)
        rows = np.where(mask.any(axis=1))[0]
        # The horizon line the compositor hazes and lights against is where
        # the far layers end and the near ones begin: the top of the last
        # band whose depth is still "far" (>= 0.6), not the farthest peak.
        depth_i = 0.92 - 0.84 * i / max(n - 1, 1)
        if depth_i >= 0.6 and len(rows):
            horizon = rows[0] / h
        colour = ramp(stops, i / max(n - 1, 1))
        name = f"{i + 1:02d}-band.svg"
        if not lights_only:
            bw = Image.fromarray(np.where(mask, 0, 255).astype(np.uint8))
            png = os.path.join(tmp, f"band-{i + 1}.png")
            bw.save(png)
            raw = os.path.join(tmp, f"band-{i + 1}.raw.svg")
            vtracer.convert_image_to_svg_py(
                png, raw, colormode="binary", hierarchical="stacked", mode="spline",
                filter_speckle=16, corner_threshold=25, length_threshold=2.0, splice_threshold=25, path_precision=2,
            )
            text = open(raw).read()
            # One group of black paths → the baker reads it as one layer.
            paths = re.findall(r"<path[^>]*/>", text)
            paths = [re.sub(r'fill="[^"]*"', "", p) for p in paths]
            wrapped = f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {w} {h}"><g id="band-{i + 1}" fill="{colour}">{"".join(paths)}</g></svg>'
            wsvg = os.path.join(tmp, f"band-{i + 1}.svg")
            open(wsvg, "w").write(wrapped)
            out_dir = os.path.join(tmp, f"out-{i + 1}")
            bake_scene.bake(wsvg, out_dir, simplify=simplify, min_area=0.00005)
            produced = [f for f in os.listdir(out_dir) if f.endswith(".svg")][0]
            shutil.move(os.path.join(out_dir, produced), os.path.join(dest, name))
        depth = 0.92 - 0.84 * i / max(n - 1, 1)
        toml += ["", "[[layer]]", f'svg = "{name}"', f"depth = {depth:.2f}", f"# value band {lo}-{hi}, fill {colour}"]
        if (i + 1) in lights:
            # This band alone (not the nearer ones), shrunk a few pixels so
            # windows never touch a silhouette's edge. With --lights-only the
            # shapes come from the band SVGs on disk, not the key: those are
            # what is drawn, and they may have been fixed by hand.
            if lights_only:
                # What shows of this band is its shape minus every nearer
                # band's — all of them, not just the next: hand-fixed files
                # need not be cumulative the way traced ones are.
                band_mask = svg_mask(os.path.join(dest, name), w, h)
                nearer = np.zeros_like(band_mask)
                for j in range(i + 2, n + 1):
                    nearer |= svg_mask(os.path.join(dest, f"{j:02d}-band.svg"), w, h)
                only = band_mask & ~nearer
            else:
                nearer = lum <= ((layers[i + 1][1] + lo) // 2) if i + 1 < n else np.zeros_like(mask)
                only = mask & ~nearer
            e = 3
            core = only.copy()
            core[:-e] &= only[e:]
            core[e:] &= only[:-e]
            core[:, :-e] &= only[:, e:]
            core[:, e:] &= only[:, :-e]
            # Size falls off steeply with distance: a band at depth 0.56
            # keeps specks, one at 0.44 gets panes about twice the size.
            near = 1.0 - depth
            k = 0.7 * (near / 0.44) ** 3.0
            cw, ch = pane.get(i + 1, (0, 0))
            if not cw:
                cw = max(1, int(round(w / 640 * k)))
                ch = max(2, int(round(cw * 1.5)))
            sx, sy = max(cw + 2, int(cw * 2.6)), max(ch + 2, int(ch * 2.2))
            # Windows are grouped per tone into small tiles, one <path> per
            # tile: the compositor draws each path as one command whose cost
            # is its bounding box times its edge count, so a tile of a few
            # dozen windows is cheap where one path of thousands is not.
            # Windows are dealt among three files, each switching on at a
            # different sun elevation: about an hour before sunset, at
            # sunset, about an hour after. The city lights up in stages.
            # The everyday set is `no-holiday`; with --holidays, one set per
            # holiday follows, each with its `when`.
            ys, xs = np.where(core)
            full = (xs.min(), xs.max(), ys.min(), ys.max()) if len(xs) else (0, w, 0, h)
            # Shapes go on one facade — the largest solid rectangle in the
            # band — so the skyline never cuts through them. The flag takes
            # the whole face; a heart or a face takes a centred square.
            fx, fy, fw, fh = largest_rect(core)
            side = min(fw, fh)
            square = (fx + (fw - side) / 2, fx + (fw + side) / 2, fy + (fh - side) / 2, fy + (fh + side) / 2)
            facade = (fx, fx + fw, fy, fy + fh)
            print(f"  facade for shapes: {fw}x{fh} at ({fx},{fy})")
            cells = []
            for row, y in enumerate(range(0, h - ch, sy)):
                for col, x in enumerate(range(0, w - cw, sx)):
                    if core[y : y + ch, x : x + cw].all():
                        cells.append((row, col, x, y))
            nrows = max(1, len(set(r for r, _, _, _ in cells)))

            def emit(tag, when, tone_at, shaped=False):
                rng = np.random.default_rng(i + 7 + sum(map(ord, tag)))
                groups = [{} for _ in range(3)]
                counts = [0, 0, 0]
                for row, col, x, y in cells:
                    # Stripes span the band. Shapes are measured in their
                    # facade box; a window on that facade but outside the
                    # shape stays dark so the shape is crisp, and windows
                    # elsewhere are lit at random in the holiday palette.
                    if shaped:
                        box = facade if shaped == "flag" else square
                        on_facade = fx <= x < fx + fw and fy <= y < fy + fh
                        u = (x + cw / 2 - box[0]) / max(1, box[1] - box[0]) * 2 - 1
                        v = (y + ch / 2 - box[2]) / max(1, box[3] - box[2]) * 2 - 1
                        if on_facade:
                            t = tone_at(rng, u, v, row, col, nrows) if abs(u) <= 1 and abs(v) <= 1 else None
                        else:
                            t = tone_at(rng, 9.0, 9.0, row, col, nrows)
                    else:
                        u = (x + cw / 2 - full[0]) / max(1, full[1] - full[0]) * 2 - 1
                        v = (y + ch / 2 - full[2]) / max(1, full[3] - full[2]) * 2 - 1
                        t = tone_at(rng, u, v, row, col, nrows)
                    if t is None:
                        continue
                    g = int(rng.integers(3))
                    key = (t, x // tile, y // tile)
                    groups[g].setdefault(key, []).append(f"M {x} {y} L {x + cw} {y} L {x + cw} {y + ch} L {x} {y + ch} Z")
                    counts[g] += 1
                for g, (by_tile, on_below, at) in enumerate(zip(groups, (10, 0, -8), ("an hour before sunset", "sunset", "an hour after sunset"))):
                    if not counts[g]:
                        continue
                    lname = f"{i + 1:02d}b-lights{tag}-{g + 1}.svg"
                    paths = "".join(f'<path d="{" ".join(d)}" fill="{t}"/>' for (t, _, _), d in sorted(by_tile.items()))
                    open(os.path.join(dest, lname), "w").write(
                        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {w} {h}">{paths}</svg>\n'
                    )
                    toml.extend(["", "[[layer]]", f'svg = "{lname}"', f"depth = {depth:.2f}",
                                 f"on_below = {on_below}   # sun elevation, degrees: {at}", f'when = "{when}"',
                                 f"# {counts[g]} lit windows on band {i + 1}"])
                print(f"{i + 1:02d}b-lights{tag}: {sum(counts)} windows in 3 stages, when {when}")

            tile = max(48, w // 24)
            # Everyday: mostly warm tungsten and paper white, a third off-key
            # — neon signs, screens, someone's lava lamp.
            warm = ["#ffd98a", "#ffc766", "#fff2c8", "#ffb35c", "#ffe9a8", "#ffab7a"]
            funky = ["#9be7ff", "#ff8ad8", "#c8ff8a", "#b9a8ff", "#ff6f9c", "#7dfff0", "#ffe25c"]
            everyday = [(t, 0.7 / len(warm)) for t in warm] + [(t, 0.3 / len(funky)) for t in funky]
            emit("", "no-holiday", lambda rng, u, v, row, col, nrows: holiday_tone("random", everyday, rng, u, v, row, col, nrows))
            if holidays:
                for hname, when, tones, pattern in HOLIDAYS:
                    # Shapes only on the first (farthest, densest) lights
                    # band; the nearer band's few large windows would only
                    # make a blob. Other bands light up in the palette.
                    shape_band = (i + 1) == min(lights)
                    emit(
                        f"-{hname}",
                        when,
                        lambda rng, u, v, row, col, nrows, tones=tones, pattern=pattern, sb=shape_band: holiday_tone(
                            pattern if (sb or pattern not in ("heart", "pumpkin", "flag")) else "random", tones, rng, u, v, row, col, nrows
                        ),
                        shaped=pattern if (shape_band and pattern in ("heart", "pumpkin", "flag")) else False,
                    )
    shutil.rmtree(tmp)
    if lights_only:
        print(f"{n} layers → lights files only in {dest} (bands and scene.toml untouched)")
        return
    toml.insert(3, f"horizon = {max(0.1, min(0.95, (horizon or 0.6) + 0.06)):.2f}")
    toml.insert(4, "haze = 0.22")
    toml.insert(5, "sun = true")
    open(os.path.join(dest, "scene.toml"), "w").write("\n".join(toml) + "\n")
    print(f"{n} layers → {dest}/scene.toml (horizon {horizon:.2f})")


if __name__ == "__main__":
    main()
