#!/usr/bin/env python3
"""Bake an Inkscape SVG into a plain single-colour icon for crates/rill-ui.

    scripts/bake-icon.py SRC.svg DEST.svg [--rotate DEG]

Composes every <g transform> (matrix / translate / scale / rotate) into
the path coordinates, converts m/l/h/v/c/z to absolute commands, applies
an optional extra rotation, and normalises the drawing into the icon
set's 256-unit viewBox (232 across the longer side, centred). Fills are
dropped: the result takes the style's colour like every other glyph.
"""
import math, re, sys, xml.etree.ElementTree as ET

NS = "{http://www.w3.org/2000/svg}"


def mul(A, B):  # A∘B: apply B, then A
    a, b, c, d, e, f = A
    g, h, i, j, k, l = B
    return [a * g + c * h, b * g + d * h, a * i + c * j, b * i + d * j, a * k + c * l + e, b * k + d * l + f]


def parse_transform(t):
    M = [1, 0, 0, 1, 0, 0]
    for name, args in re.findall(r"(\w+)\(([^)]*)\)", t or ""):
        v = [float(x) for x in re.split(r"[ ,]+", args.strip()) if x]
        if name == "matrix":
            T = v
        elif name == "translate":
            T = [1, 0, 0, 1, v[0], v[1] if len(v) > 1 else 0]
        elif name == "scale":
            T = [v[0], 0, 0, v[1] if len(v) > 1 else v[0], 0, 0]
        elif name == "rotate":
            r = math.radians(v[0])
            T = [math.cos(r), math.sin(r), -math.sin(r), math.cos(r), 0, 0]
            if len(v) == 3:
                T = mul(mul([1, 0, 0, 1, v[1], v[2]], T), [1, 0, 0, 1, -v[1], -v[2]])
        else:
            sys.exit(f"unsupported transform {name}")
        M = mul(M, T)
    return M


def apply(M, x, y):
    a, b, c, d, e, f = M
    return (a * x + c * y + e, b * x + d * y + f)


def walk(el, M, out):
    M2 = mul(M, parse_transform(el.get("transform")))
    if el.tag == NS + "path":
        out.append((M2, el.get("d")))
    for ch in el:
        walk(ch, M2, out)


def bake(src, dst, extra_rot_deg=0.0):
    root = ET.parse(src).getroot()
    paths = []
    walk(root, [1, 0, 0, 1, 0, 0], paths)
    baked = []
    for M, dd in paths:
        toks = re.findall(r"[a-zA-Z]|-?\d*\.?\d+(?:e-?\d+)?", dd)
        i = 0
        cmd = None
        cur = start = (0.0, 0.0)
        s = []

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
                    p = (cur[0] + x, cur[1] + y) if rel else (x, y)
                    pts.append(p)
                cur = pts[2]
                s.append(("C", [apply(M, *p) for p in pts]))
            elif c == "z":
                s.append(("Z", []))
                cur = start
            else:
                sys.exit(f"unsupported path command {cmd} in {src}")
        baked.append(s)
    r = math.radians(extra_rot_deg)
    cr, sr = math.cos(r), math.sin(r)
    baked = [[(c, [(p[0] * cr - p[1] * sr, p[0] * sr + p[1] * cr) for p in pts]) for c, pts in path] for path in baked]
    xs = [p[0] for path in baked for _, pts in path for p in pts]
    ys = [p[1] for path in baked for _, pts in path for p in pts]
    x0, x1, y0, y1 = min(xs), max(xs), min(ys), max(ys)
    k = 232.0 / max(x1 - x0, y1 - y0)
    ox = (256 - (x1 - x0) * k) / 2 - x0 * k
    oy = (256 - (y1 - y0) * k) / 2 - y0 * k
    fmt = lambda p: f"{p[0] * k + ox:.2f},{p[1] * k + oy:.2f}"
    body = "".join(
        '<path d="' + " ".join(c + " ".join(fmt(p) for p in pts) for c, pts in path) + '"/>' for path in baked
    )
    with open(dst, "w") as f:
        f.write(f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 256 256" fill="currentColor">{body}</svg>\n')
    print(f"{dst}: {len(baked)} paths, aspect {(x1 - x0) / (y1 - y0):.2f}")


if __name__ == "__main__":
    args = sys.argv[1:]
    rot = 0.0
    if "--rotate" in args:
        i = args.index("--rotate")
        rot = float(args[i + 1])
        del args[i : i + 2]
    bake(args[0], args[1], rot)
