"""Affine helpers shared by bake-icon.py and bake-scene.py."""
import math, re, sys


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


