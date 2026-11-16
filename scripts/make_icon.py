"""Draws the app icon.

An icon is the one part of a bundle that cannot be inferred from the code, and
a generic placeholder in the Dock reads as an unfinished app. This is a mark
rather than a picture: a ribbon folded back on itself, the way thread lies when
it comes off a spool. It reads as an S without being set in a typeface.

The register is deliberate — a near-black squircle, one geometric form, two
flat tones, no gloss and no drop shadow. An envelope would say "mail" to
someone who has not opened the app yet and nothing at all to anyone who has;
every icon in the Dock beside it belongs to a program whose mark is its name,
not its function.

The whole point of it being a script is that changing it is editing numbers.
The two that matter most are GROUND (what the tile is) and INK/DIM (the lit
face of the ribbon and the face turned away). Swapping GROUND for the accent
blue in `theme.rs` gives the same mark as a colour icon.

    python3 scripts/make_icon.py out.png
"""

import math

from PIL import Image, ImageDraw

# Drawn far larger than any size it is used at, then downsampled by `sips` in
# `bundle.sh`: the curves of the fold alias badly rasterised at 32 points.
SIZE = 2048

# The tile. Near-black rather than black, and a gradient rather than a flat
# fill, so it does not read as a hole punched in a dark Dock.
GROUND_TOP = (0x27, 0x29, 0x2D, 255)
GROUND_BOTTOM = (0x11, 0x12, 0x14, 255)

# The ribbon, lit and turned away. The gap between them is what makes one
# shape read as two planes; too close and the fold disappears, too far and the
# lower half looks like a different object.
INK_TOP = (0xF5, 0xF6, 0xF8, 255)
INK_BOTTOM = (0xD8, 0xDD, 0xE3, 255)
DIM_TOP = (0x9B, 0xA4, 0xB0, 255)
DIM_BOTTOM = (0x79, 0x82, 0x8E, 255)

# Thickness of the ribbon, as a fraction of the icon. The mark is built from
# two mirrored curves; this is the distance between each curve and its twin.
WEIGHT = 0.118


def squircle(exponent: float = 5.0, inset: float = 0.055, steps: int = 1440):
    """The macOS icon outline, as a point list.

    macOS rounds app icons with a superellipse, not a rounded rectangle. The
    difference is invisible described and obvious side by side: a rounded
    rectangle has a constant-radius corner that meets the straight edge at a
    visible seam, and the superellipse never stops curving. `inset` is the
    margin Apple leaves so an icon sits the same size as everything else in
    the Dock.
    """
    radius = SIZE / 2 - SIZE * inset
    centre = SIZE / 2
    points = []
    for i in range(steps):
        t = 2 * math.pi * i / steps
        cos_t, sin_t = math.cos(t), math.sin(t)
        points.append((
            centre + radius * math.copysign(abs(cos_t) ** (2 / exponent), cos_t),
            centre + radius * math.copysign(abs(sin_t) ** (2 / exponent), sin_t),
        ))
    return points


def gradient(top, bottom, band=None) -> Image.Image:
    """A vertical gradient the size of the canvas, to paste through a mask.

    `band` is the pair of y coordinates the ramp runs between, so a small shape
    gets the whole gradient across itself rather than the slice of a
    full-height one that happens to fall behind it.
    """
    column = Image.new("RGBA", (1, SIZE))
    pixels = column.load()
    start, end = (0, SIZE) if band is None else band
    span = max(1, end - start)
    for y in range(SIZE):
        t = min(1.0, max(0.0, (y - start) / span))
        pixels[0, y] = tuple(
            round(top[i] + (bottom[i] - top[i]) * t) for i in range(4)
        )
    return column.resize((SIZE, SIZE))


def mask(draw_shape) -> Image.Image:
    layer = Image.new("L", (SIZE, SIZE), 0)
    draw_shape(ImageDraw.Draw(layer))
    return layer


def curve(start, control, end, steps: int = 160):
    """A quadratic bezier as a point list, for `polygon` to fill."""
    points = []
    for i in range(steps + 1):
        t = i / steps
        u = 1 - t
        points.append((
            u * u * start[0] + 2 * u * t * control[0] + t * t * end[0],
            u * u * start[1] + 2 * u * t * control[1] + t * t * end[1],
        ))
    return points


def at(x: float, y: float):
    """Fractions of the icon to pixels, so the numbers above stay readable."""
    return (x * SIZE, y * SIZE)


def draw() -> Image.Image:
    image = Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0))
    image.paste(
        gradient(GROUND_TOP, GROUND_BOTTOM),
        (0, 0),
        mask(lambda pen: pen.polygon(squircle(), fill=255)),
    )

    # Each half of the ribbon is the area between a curve and the same curve
    # shifted by WEIGHT — an outer edge and an inner one, closed into a
    # polygon. The control points sit outside the mark on purpose: that is
    # what gives the hook at each end its tension instead of a lazy arc.
    def upper(pen):
        outer = curve(at(0.705, 0.310), at(0.295, 0.310), at(0.332, 0.500))
        inner = curve(
            at(0.332 + WEIGHT, 0.500),
            at(0.295 + WEIGHT * 1.15, 0.310 + WEIGHT),
            at(0.705, 0.310 + WEIGHT),
        )
        pen.polygon(outer + inner, fill=255)

    def lower(pen):
        outer = curve(at(0.295, 0.690), at(0.705, 0.690), at(0.668, 0.500))
        inner = curve(
            at(0.668 - WEIGHT, 0.500),
            at(0.705 - WEIGHT * 1.15, 0.690 - WEIGHT),
            at(0.295, 0.690 - WEIGHT),
        )
        pen.polygon(outer + inner, fill=255)

    # Lower first: where the two halves meet at the waist, the lit face is the
    # one that should be on top.
    image.paste(
        gradient(DIM_TOP, DIM_BOTTOM, band=(int(0.50 * SIZE), int(0.70 * SIZE))),
        (0, 0),
        mask(lower),
    )
    image.paste(
        gradient(INK_TOP, INK_BOTTOM, band=(int(0.30 * SIZE), int(0.50 * SIZE))),
        (0, 0),
        mask(upper),
    )

    return image


if __name__ == "__main__":
    import sys

    draw().save(sys.argv[1])
