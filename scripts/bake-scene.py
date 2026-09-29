#!/usr/bin/env python3
"""Bake an Inkscape drawing into a scene directory the compositor can paint.

    scripts/bake-scene.py SRC.svg DEST_DIR/ [--simplify TOL] [--min-area FRAC]

`--simplify TOL` flattens curves and thins every ring to straight segments
within TOL viewBox units (Ramer–Douglas–Peucker) — what a bitmap trace
needs before a glass can afford it. `--min-area FRAC` drops rings whose
bounding box is under FRAC of the canvas (speckles; try 0.0002).

Each visible Inkscape layer becomes one plain SVG, `NN-<label>.svg`, in
document order (bottom layer first, which is the horizon): every <g
transform> composed into the path coordinates, m/l/h/v/c/z made absolute,
`style="fill:#…"` turned into a `fill` attribute, hidden layers and
embedded images dropped. The viewBox is kept as drawn, so a 3840×2160
canvas stays one. Point the theme at DEST_DIR (`[desktop] wallpaper_scene`);
the compositor orders by name, spaces depth from the first file to the
last, and treats a layer whose label mentions night/light/star as a night
layer. Rename a file to re-order it; nothing else is stored.

What is not carried: strokes (fill only), fill-opacity (the reader has no
alpha in fills), gradients and patterns (a url() fill becomes grey and is
reported). Shapes that overlap inside ONE path cancel under even-odd —
keep one shape per path in Inkscape, or Path → Union them first.
"""
import os, re, sys, xml.etree.ElementTree as ET

NS = "{http://www.w3.org/2000/svg}"
INK = "{http://www.inkscape.org/namespaces/inkscape}"

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from bake_icon_lib import apply, mul, parse_transform  # noqa: E402


def style_of(el):
    st = {}
    for part in (el.get("style") or "").split(";"):
        if ":" in part:
            k, v = part.split(":", 1)
            st[k.strip()] = v.strip()
    return st


def hidden(el):
    return style_of(el).get("display") == "none"


def fill_of(el, inherited):
    f = style_of(el).get("fill") or el.get("fill") or inherited
    return f


def collect(el, M, fill, out, warn):
    if hidden(el):
        return
    M2 = mul(M, parse_transform(el.get("transform")))
    fill = fill_of(el, fill)
    if el.tag == NS + "path":
        d = el.get("d")
        if d:
            f = (fill or "").strip()
            if f == "none":
                return
            if not re.fullmatch(r"#[0-9a-fA-F]{6}", f):
                if re.fullmatch(r"#[0-9a-fA-F]{3}", f):
                    f = "#" + "".join(c * 2 for c in f[1:])
                else:
                    warn.append(f"{el.get('id')}: fill {f!r} is not a hex colour, painting grey")
                    f = "#808080"
            out.append((M2, d, f))
    for ch in el:
        if ch.tag in (NS + "g", NS + "path"):
            collect(ch, M2, fill, out, warn)


def absolute(M, dd, src):
    toks = re.findall(r"[a-zA-Z]|-?\d*\.?\d+(?:e-?\d+)?", dd)
    i, cmd, cur, start, s = 0, None, (0.0, 0.0), (0.0, 0.0), []

    def num():
        nonlocal i
        v = float(toks[i])
        i += 1
        return v

    while i < len(toks):
        if toks[i].isalpha():
            cmd = toks[i]
            i += 1
        rel = cmd.islower()
        c = cmd.lower()
        if c == "m":
            x, y = num(), num()
            cur = (cur[0] + x, cur[1] + y) if rel else (x, y)
            start = cur
            s.append(("M", [apply(M, *cur)]))
            cmd = "l" if rel else "L"
        elif c == "l":
            x, y = num(), num()
            cur = (cur[0] + x, cur[1] + y) if rel else (x, y)
            s.append(("L", [apply(M, *cur)]))
        elif c == "h":
            x = num()
            cur = (cur[0] + x, cur[1]) if rel else (x, cur[1])
            s.append(("L", [apply(M, *cur)]))
        elif c == "v":
            y = num()
            cur = (cur[0], cur[1] + y) if rel else (cur[0], y)
            s.append(("L", [apply(M, *cur)]))
        elif c == "c":
            pts = []
            for _ in range(3):
                x, y = num(), num()
                pts.append((cur[0] + x, cur[1] + y) if rel else (x, y))
            cur = pts[2]
            s.append(("C", [apply(M, *p) for p in pts]))
        elif c == "z":
            s.append(("Z", []))
            cur = start
        else:
            sys.exit(f"unsupported path command {cmd} in {src}: convert with Path → Object to Path / Simplify in Inkscape")
    return s


def flatten_ring(seg):
    """One M…Z run as a polyline: cubics sampled, lines kept."""
    pts = []
    for c, ps in seg:
        if c == "M":
            pts = [ps[0]]
        elif c == "L":
            pts.append(ps[0])
        elif c == "C":
            p0 = pts[-1] if pts else ps[0]
            for k in range(1, 9):
                t = k / 8.0
                u = 1 - t
                x = u * u * u * p0[0] + 3 * u * u * t * ps[0][0] + 3 * u * t * t * ps[1][0] + t * t * t * ps[2][0]
                y = u * u * u * p0[1] + 3 * u * u * t * ps[0][1] + 3 * u * t * t * ps[1][1] + t * t * t * ps[2][1]
                pts.append((x, y))
    return pts


def rdp(pts, tol):
    if len(pts) < 3:
        return pts
    (ax, ay), (bx, by) = pts[0], pts[-1]
    dx, dy = bx - ax, by - ay
    n = (dx * dx + dy * dy) ** 0.5
    best, idx = 0.0, 0
    for i in range(1, len(pts) - 1):
        px, py = pts[i]
        d = abs(dy * px - dx * py + bx * ay - by * ax) / n if n else ((px - ax) ** 2 + (py - ay) ** 2) ** 0.5
        if d > best:
            best, idx = d, i
    if best > tol:
        return rdp(pts[: idx + 1], tol)[:-1] + rdp(pts[idx:], tol)
    return [pts[0], pts[-1]]


def rings_of(absolute_segs):
    """Split an absolute command list into M…Z runs."""
    runs, cur = [], []
    for c, ps in absolute_segs:
        if c == "M" and cur:
            runs.append(cur)
            cur = []
        cur.append((c, ps))
    if cur:
        runs.append(cur)
    return runs


def slug(label, n):
    s = re.sub(r"[^a-z0-9]+", "-", (label or "").lower()).strip("-")
    return s or f"layer-{n}"


def bake(src, dest, simplify=None, min_area=None):
    root = ET.parse(src).getroot()
    vb = root.get("viewBox") or f"0 0 {root.get('width')} {root.get('height')}"
    vbw, vbh = [float(x) for x in vb.split()[2:4]]
    canvas = vbw * vbh
    os.makedirs(dest, exist_ok=True)
    # Every top-level group is a layer, Inkscape layer or not (a pasted
    # trace lands as a plain <g>); a loose top-level path is a layer of one.
    layers = [g for g in root if g.tag in (NS + "g", NS + "path")]
    n = 0
    for g in layers:
        if hidden(g):
            continue
        paths, warn = [], []
        collect(g, [1, 0, 0, 1, 0, 0], None, paths, warn)
        if not paths:
            continue
        n += 1
        label = g.get(INK + "label") or g.get("id")
        name = f"{n:02d}-{slug(label, n)}.svg"
        fmt = lambda p: f"{p[0]:.2f} {p[1]:.2f}"
        body = ""
        kept = dropped = 0
        for M, d, f in paths:
            segs = absolute(M, d, src)
            if simplify is None and min_area is None:
                body += '<path d="' + " ".join(c + " ".join(fmt(p) for p in pts) for c, pts in segs) + f'" fill="{f}"/>'
                kept += 1
                continue
            out_rings = []
            for run in rings_of(segs):
                pts = flatten_ring(run)
                if min_area is not None and pts:
                    xs = [q[0] for q in pts]
                    ys = [q[1] for q in pts]
                    if (max(xs) - min(xs)) * (max(ys) - min(ys)) < min_area * canvas:
                        dropped += 1
                        continue
                if simplify is not None:
                    pts = rdp(pts, simplify)
                if len(pts) >= 3:
                    out_rings.append(pts)
            if out_rings:
                d2 = " ".join("M " + " L ".join(fmt(q) for q in ring) + " Z" for ring in out_rings)
                body += f'<path d="{d2}" fill="{f}"/>'
                kept += 1
        paths = paths[:kept] if kept else []
        if dropped:
            print(f"  dropped {dropped} speckle rings under {min_area:.4%} of the canvas")
        with open(os.path.join(dest, name), "w") as out:
            out.write(f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="{vb}">{body}</svg>\n')
        fills = sorted({f for _, _, f in paths})
        nodes = len(re.findall(r"[LMC] ", body)) + body.count(" L ")
        print(f"{name}: {len(paths)} paths, ~{nodes} nodes, fills {' '.join(fills)}")
        if len(paths) > 300 or nodes > 4000:
            print(f"  warning: heavy for a glass ({len(paths)} paths, ~{nodes} nodes) — Path → Simplify, or trace with fewer speckles")
        for w in warn:
            print(f"  warning: {w}")
    if n == 0:
        sys.exit("no visible layers with paths")
    print(f"{n} layers → {dest} (viewBox {vb})")


if __name__ == "__main__":
    args = sys.argv[1:]
    simplify = min_area = None
    if "--simplify" in args:
        i = args.index("--simplify")
        simplify = float(args[i + 1])
        del args[i : i + 2]
    if "--min-area" in args:
        i = args.index("--min-area")
        min_area = float(args[i + 1])
        del args[i : i + 2]
    if len(args) != 2:
        sys.exit(__doc__)
    bake(args[0], args[1], simplify, min_area)
