#!/usr/bin/env python3
"""Convert Lucide SVG icons into `vellum_ui::icon::Prim` geometry.

    python3 scripts/icons.py hand pencil frame star folder
    python3 scripts/icons.py --map Hand=hand Select=mouse-pointer-2

Prints Rust match arms to stdout; paste them into `crates/vellum-ui/src/icon.rs`.

# Why this exists

The icons were drawn by hand, point by point, in normalised 0..1 coordinates. That is
fine for a rectangle and hopeless for a hand: three separate attempts at one — an
outline, a filled silhouette, and strokes — all read badly at the 24pt the palette
actually draws, and each had to be screenshotted to find out. *"can you not get icons
from the internet or something good looking ones actually"* is the right instinct.

**Lucide** (https://lucide.dev) is the natural source: ISC-licensed, stroke-based on a
24×24 grid with round caps, which is the same drawing model `Icon::paint` already has.
What it is not is directly usable — Lucide paths are full SVG, with cubic béziers and
elliptical arcs, and `Prim` is polylines and circles. So this flattens them.

# What it does

1. Fetches `icons/<name>.svg` from the Lucide repository.
2. Parses the `d` of every `<path>`: `M L H V C S Q T A Z`, absolute and relative.
3. Flattens curves to line segments at [`FLATNESS`], and converts arcs endpoint-to-centre
   per the SVG spec's implementation notes (F.6.5) before flattening those.
4. Scales the 24×24 viewBox to 0..1 and rounds to 3 decimals, which is well under a pixel
   at any size the app draws.
5. Emits `line(...)` for an open subpath and `outline(...)` for one that closed with `Z`.

`<circle>` and `<rect>` elements are handled too — several Lucide icons use them, and a
circle maps straight onto `Prim::Circle` rather than being flattened.

# Licence

Lucide is ISC, which requires the copyright notice be carried. `crates/vellum-ui/src/icon.rs`
holds it in its module header, next to the generated geometry. Do not remove it.
"""

from __future__ import annotations

import argparse
import math
import re
import sys
import urllib.request

LUCIDE = "https://raw.githubusercontent.com/lucide-icons/lucide/main/icons/{}.svg"

# Maximum deviation between a flattened segment and the true curve, in viewBox units
# (the grid is 24 wide). 0.12 is about a twentieth of a stroke width — invisible at the
# sizes this app draws, and it keeps the point lists short enough to read in source.
FLATNESS = 0.12

VIEWBOX = 24.0

NUM = re.compile(r"[-+]?(?:\d*\.\d+|\d+\.?)(?:[eE][-+]?\d+)?")


def fetch(name: str) -> str:
    with urllib.request.urlopen(LUCIDE.format(name), timeout=30) as response:
        return response.read().decode("utf-8")


def numbers(text: str) -> list[float]:
    return [float(t) for t in NUM.findall(text)]


def tokenize(d: str) -> list[tuple[str, list[float]]]:
    """Path data as (command, args) pairs, in order."""
    out: list[tuple[str, list[float]]] = []
    for command, chunk in re.findall(r"([MmLlHhVvCcSsQqTtAaZz])([^MmLlHhVvCcSsQqTtAaZz]*)", d):
        out.append((command, numbers(chunk)))
    return out


def cubic(p0, p1, p2, p3, out: list[tuple[float, float]]) -> None:
    """Flatten one cubic bézier, subdividing by its own control-polygon error."""
    # Steps from the control polygon's length: longer curve, more segments. Cheap, and
    # always an over-estimate, which is the safe direction.
    span = (
        math.dist(p0, p1) + math.dist(p1, p2) + math.dist(p2, p3)
    )
    steps = max(2, min(64, int(math.sqrt(span / FLATNESS) * 2)))
    for i in range(1, steps + 1):
        t = i / steps
        u = 1 - t
        x = u**3 * p0[0] + 3 * u * u * t * p1[0] + 3 * u * t * t * p2[0] + t**3 * p3[0]
        y = u**3 * p0[1] + 3 * u * u * t * p1[1] + 3 * u * t * t * p2[1] + t**3 * p3[1]
        out.append((x, y))


def quadratic(p0, p1, p2, out: list[tuple[float, float]]) -> None:
    # Raised to a cubic rather than given its own flattener: one code path is one place
    # for the tolerance to be wrong.
    c1 = (p0[0] + 2 / 3 * (p1[0] - p0[0]), p0[1] + 2 / 3 * (p1[1] - p0[1]))
    c2 = (p2[0] + 2 / 3 * (p1[0] - p2[0]), p2[1] + 2 / 3 * (p1[1] - p2[1]))
    cubic(p0, c1, c2, p2, out)


def arc(p0, rx, ry, phi_deg, large, sweep, p1, out: list[tuple[float, float]]) -> None:
    """Elliptical arc, endpoint parameterisation, per SVG 1.1 F.6.5."""
    if p0 == p1:
        return
    rx, ry = abs(rx), abs(ry)
    if rx == 0 or ry == 0:
        out.append(p1)
        return
    phi = math.radians(phi_deg)
    cos_p, sin_p = math.cos(phi), math.sin(phi)

    dx2, dy2 = (p0[0] - p1[0]) / 2, (p0[1] - p1[1]) / 2
    x1 = cos_p * dx2 + sin_p * dy2
    y1 = -sin_p * dx2 + cos_p * dy2

    # F.6.6: scale the radii up if they are too small to span the chord.
    lam = (x1 * x1) / (rx * rx) + (y1 * y1) / (ry * ry)
    if lam > 1:
        scale = math.sqrt(lam)
        rx, ry = rx * scale, ry * scale

    denom = rx * rx * y1 * y1 + ry * ry * x1 * x1
    num = rx * rx * ry * ry - denom
    factor = math.sqrt(max(0.0, num / denom)) if denom else 0.0
    if large == sweep:
        factor = -factor
    cx1 = factor * rx * y1 / ry
    cy1 = -factor * ry * x1 / rx

    cx = cos_p * cx1 - sin_p * cy1 + (p0[0] + p1[0]) / 2
    cy = sin_p * cx1 + cos_p * cy1 + (p0[1] + p1[1]) / 2

    def angle(ux, uy, vx, vy):
        return math.atan2(ux * vy - uy * vx, ux * vx + uy * vy)

    start = angle(1, 0, (x1 - cx1) / rx, (y1 - cy1) / ry)
    delta = angle((x1 - cx1) / rx, (y1 - cy1) / ry, (-x1 - cx1) / rx, (-y1 - cy1) / ry)
    if not sweep and delta > 0:
        delta -= 2 * math.pi
    elif sweep and delta < 0:
        delta += 2 * math.pi

    steps = max(2, min(64, int(abs(delta) / (2 * math.pi) * max(rx, ry) / FLATNESS) + 2))
    for i in range(1, steps + 1):
        theta = start + delta * i / steps
        x = cos_p * rx * math.cos(theta) - sin_p * ry * math.sin(theta) + cx
        y = sin_p * rx * math.cos(theta) + cos_p * ry * math.sin(theta) + cy
        out.append((x, y))


def subpaths(d: str) -> list[tuple[list[tuple[float, float]], bool]]:
    """Flattened subpaths as (points, closed)."""
    result: list[tuple[list[tuple[float, float]], bool]] = []
    pts: list[tuple[float, float]] = []
    cur = (0.0, 0.0)
    start = (0.0, 0.0)
    prev_cubic: tuple[float, float] | None = None
    prev_quad: tuple[float, float] | None = None

    def flush(closed: bool) -> None:
        nonlocal pts
        if len(pts) > 1:
            result.append((pts, closed))
        pts = []

    for command, args in tokenize(d):
        rel = command.islower()
        c = command.upper()

        if c == "M":
            flush(False)
            for i in range(0, len(args), 2):
                x, y = args[i], args[i + 1]
                cur = (cur[0] + x, cur[1] + y) if rel else (x, y)
                if i == 0:
                    start = cur
                    pts = [cur]
                else:
                    pts.append(cur)
            prev_cubic = prev_quad = None
        elif c == "L":
            for i in range(0, len(args), 2):
                x, y = args[i], args[i + 1]
                cur = (cur[0] + x, cur[1] + y) if rel else (x, y)
                pts.append(cur)
            prev_cubic = prev_quad = None
        elif c == "H":
            for x in args:
                cur = (cur[0] + x, cur[1]) if rel else (x, cur[1])
                pts.append(cur)
            prev_cubic = prev_quad = None
        elif c == "V":
            for y in args:
                cur = (cur[0], cur[1] + y) if rel else (cur[0], y)
                pts.append(cur)
            prev_cubic = prev_quad = None
        elif c in ("C", "S"):
            stride = 6 if c == "C" else 4
            for i in range(0, len(args), stride):
                a = args[i : i + stride]
                if c == "C":
                    p1 = (cur[0] + a[0], cur[1] + a[1]) if rel else (a[0], a[1])
                    p2 = (cur[0] + a[2], cur[1] + a[3]) if rel else (a[2], a[3])
                    p3 = (cur[0] + a[4], cur[1] + a[5]) if rel else (a[4], a[5])
                else:
                    p1 = (
                        (2 * cur[0] - prev_cubic[0], 2 * cur[1] - prev_cubic[1])
                        if prev_cubic
                        else cur
                    )
                    p2 = (cur[0] + a[0], cur[1] + a[1]) if rel else (a[0], a[1])
                    p3 = (cur[0] + a[2], cur[1] + a[3]) if rel else (a[2], a[3])
                cubic(cur, p1, p2, p3, pts)
                prev_cubic, cur = p2, p3
            prev_quad = None
        elif c in ("Q", "T"):
            stride = 4 if c == "Q" else 2
            for i in range(0, len(args), stride):
                a = args[i : i + stride]
                if c == "Q":
                    p1 = (cur[0] + a[0], cur[1] + a[1]) if rel else (a[0], a[1])
                    p2 = (cur[0] + a[2], cur[1] + a[3]) if rel else (a[2], a[3])
                else:
                    p1 = (
                        (2 * cur[0] - prev_quad[0], 2 * cur[1] - prev_quad[1])
                        if prev_quad
                        else cur
                    )
                    p2 = (cur[0] + a[0], cur[1] + a[1]) if rel else (a[0], a[1])
                quadratic(cur, p1, p2, pts)
                prev_quad, cur = p1, p2
            prev_cubic = None
        elif c == "A":
            for i in range(0, len(args), 7):
                a = args[i : i + 7]
                end = (cur[0] + a[5], cur[1] + a[6]) if rel else (a[5], a[6])
                arc(cur, a[0], a[1], a[2], bool(a[3]), bool(a[4]), end, pts)
                cur = end
            prev_cubic = prev_quad = None
        elif c == "Z":
            flush(True)
            cur = start
            prev_cubic = prev_quad = None

    flush(False)
    return result


def rust(variant: str, svg: str) -> str:
    prims: list[str] = []

    for cx, cy, r in re.findall(
        r'<circle[^>]*cx="([-\d.]+)"[^>]*cy="([-\d.]+)"[^>]*r="([-\d.]+)"', svg
    ):
        prims.append(
            f"                circle(({float(cx)/VIEWBOX:.3f}, {float(cy)/VIEWBOX:.3f}), "
            f"{float(r)/VIEWBOX:.3f}),"
        )

    for x, y, w, h in re.findall(
        r'<rect[^>]*x="([-\d.]+)"[^>]*y="([-\d.]+)"[^>]*width="([-\d.]+)"[^>]*height="([-\d.]+)"',
        svg,
    ):
        x0, y0 = float(x) / VIEWBOX, float(y) / VIEWBOX
        x1, y1 = x0 + float(w) / VIEWBOX, y0 + float(h) / VIEWBOX
        prims.append(f"                outline(rect_pts!({x0:.3f}, {y0:.3f}, {x1:.3f}, {y1:.3f})),")

    # `<line>`, `<polyline>` and `<polygon>` — Lucide uses all three, and a converter that
    # only read `<path>` produced a **silently empty** arm for `frame.svg`, which is four
    # `<line>`s. That shipped: the frame button drew nothing at all and a screenshot is the
    # only reason it was caught. Hence also the emptiness check at the end of this function.
    for x1, x2, y1, y2 in re.findall(
        r'<line[^>]*x1="([-\d.]+)"[^>]*x2="([-\d.]+)"[^>]*y1="([-\d.]+)"[^>]*y2="([-\d.]+)"',
        svg,
    ):
        prims.append(
            f"                line(&[({float(x1)/VIEWBOX:.3f}, {float(y1)/VIEWBOX:.3f}), "
            f"({float(x2)/VIEWBOX:.3f}, {float(y2)/VIEWBOX:.3f})]),"
        )

    for tag, points in re.findall(r'<(polyline|polygon)[^>]*points="([^"]+)"', svg):
        nums = numbers(points)
        body = ", ".join(
            f"({nums[i]/VIEWBOX:.3f}, {nums[i+1]/VIEWBOX:.3f})" for i in range(0, len(nums) - 1, 2)
        )
        fn = "outline" if tag == "polygon" else "line"
        prims.append(f"                {fn}(&[{body}]),")

    for d in re.findall(r'<path[^>]*\sd="([^"]+)"', svg):
        for pts, closed in subpaths(d):
            # Drop points closer together than the flattening tolerance: an arc that ends
            # where the next line begins otherwise leaves a duplicate.
            trimmed: list[tuple[float, float]] = []
            for p in pts:
                if not trimmed or math.dist(p, trimmed[-1]) > FLATNESS / 2:
                    trimmed.append(p)
            if len(trimmed) < 2:
                continue
            body = ", ".join(
                f"({p[0]/VIEWBOX:.3f}, {p[1]/VIEWBOX:.3f})" for p in trimmed
            )
            fn = "outline" if closed else "line"
            prims.append(f"                {fn}(&[{body}]),")

    if not prims:
        # **Never emit an empty arm.** One did, for `frame.svg`, and the result was a
        # button with nothing drawn in it — no compiler error, no test failure, and
        # nothing to notice until someone looked at a screenshot. An unsupported element
        # has to be loud.
        raise ValueError(
            f"{variant}: nothing convertible in the SVG "
            "(supported: <path>, <line>, <polyline>, <polygon>, <circle>, <rect>)"
        )

    joined = "\n".join(prims)
    return f"            Self::{variant} => const {{ &[\n{joined}\n            ] }},"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("names", nargs="*", help="Lucide icon names")
    parser.add_argument(
        "--map",
        action="append",
        default=[],
        metavar="Variant=lucide-name",
        help="emit as a specific Icon variant",
    )
    args = parser.parse_args()

    pairs = [(n.title().replace("-", ""), n) for n in args.names]
    for entry in args.map:
        variant, _, name = entry.partition("=")
        pairs.append((variant, name))
    if not pairs:
        parser.error("name at least one icon")

    for variant, name in pairs:
        try:
            svg = fetch(name)
        except Exception as error:  # noqa: BLE001 - a fetch failure is the whole story
            print(f"// {name}: {error}", file=sys.stderr)
            continue
        print(rust(variant, svg))
        print()
    return 0


if __name__ == "__main__":
    sys.exit(main())
