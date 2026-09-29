#!/usr/bin/env python3
"""
Layout simulator for suona's docked states.

The pet is positioned by CSS, so the numbers that matter — how far the bell
rim reaches from the pet's centre, and how much of it stays on screen when
docked — cannot be eyeballed from the source.  This script re-implements the
same arithmetic as `src-tauri/src/pet.rs` and the transforms in
`src/style.css`, rasterises the real suona SVG, and renders what the user
would actually see at each screen edge.

Run from the repo root:  python3 scripts/simulate_dock.py
"""

from __future__ import annotations

import math
import re
import subprocess
from pathlib import Path

from PIL import Image, ImageDraw

ROOT = Path(__file__).resolve().parent.parent
SVG = ROOT / "assets" / "suona.svg"
OUT = ROOT / "assets" / "preview"
TMP = OUT / "_tmp"

# ── constants mirrored from src-tauri/src/pet.rs ────────────────────────────

WINDOW_W = 380.0
BUBBLE_ROOM = 182.0
BOTTOM_MARGIN = 34.0
PET_W = 158.0
PET_H = 214.0

VIEWBOX = (120.0, 18.0, 272.0, 486.0)  # x, y, w, h  (index.html)
BELL_TIP_FRACTION = (478.0 - 18.0) / 486.0

SIZES = {"large": 1.0, "medium": 0.785, "small": 0.635}

SS = 3  # supersampling factor for the rasterised SVG


def bell_reach(scale: float) -> float:
    pet_h = PET_H * scale
    return BELL_TIP_FRACTION * pet_h - pet_h / 2.0


def draw_pet(scale: float) -> Image.Image:
    """Rasterise the suona into a PET_W x PET_H transparent element box.

    Mirrors `preserveAspectRatio="xMidYMid meet"` in a (PET_W*scale,
    PET_H*scale) box.
    """
    box_w, box_h = PET_W * scale, PET_H * scale
    vb_x, vb_y, vb_w, vb_h = VIEWBOX

    # meet: scale to fit, i.e. the smaller of the two ratios.
    fit = min(box_w / vb_w, box_h / vb_h)
    draw_w, draw_h = vb_w * fit, vb_h * fit

    TMP.mkdir(parents=True, exist_ok=True)
    cropped = TMP / "suona-cropped.svg"
    svg = SVG.read_text()
    svg = re.sub(r'viewBox="[^"]*"', f'viewBox="{vb_x} {vb_y} {vb_w} {vb_h}"', svg, count=1)
    svg = re.sub(r'width="[^"]*"', "", svg, count=1)
    svg = re.sub(r'height="[^"]*"', "", svg, count=1)
    cropped.write_text(svg)

    png = TMP / "suona-cropped.png"
    subprocess.run(
        ["rsvg-convert", "-w", str(int(draw_w * SS)), "-h", str(int(draw_h * SS)),
         str(cropped), "-o", str(png)],
        check=True,
    )

    art = Image.open(png).convert("RGBA")
    art = art.resize((max(1, round(draw_w)), max(1, round(draw_h))), Image.LANCZOS)

    box = Image.new("RGBA", (max(1, round(box_w)), max(1, round(box_h))), (0, 0, 0, 0))
    box.paste(art, (round((box_w - draw_w) / 2), round((box_h - draw_h) / 2)), art)
    return box


def place(element: Image.Image, centre: tuple[float, float], canvas: Image.Image) -> None:
    """Paste `element` so its centre lands on `centre` of `canvas`."""
    x = round(centre[0] - element.width / 2)
    y = round(centre[1] - element.height / 2)
    canvas.alpha_composite(element, (x, y))


def docked_window(dock: str, scale: float) -> tuple[int, int]:
    # Mirrors pet::dims(): the strip must be at least the pet element's width,
    # or the bell gets sliced perpendicular to its axis.
    side = PET_W * scale + 12.0
    strip = 40.0 * scale
    if dock in ("left", "right"):
        return round(strip), round(side)
    return round(side), round(strip)


def render_dock(dock: str, scale: float, label: str) -> Image.Image:
    """Render one docked state, with the window outlined against the screen."""
    win_w, win_h = docked_window(dock, scale)
    reach = bell_reach(scale)
    strip = 40.0 * scale

    pet = draw_pet(scale)

    # CSS: the pet is centred in the window, then translate()d and rotated().
    # Rotation is about the element's own centre, so the centre just moves by
    # the translate vector.
    if dock == "left":
        css_angle, tx, ty = -90.0, strip / 2 - reach, 0.0
    elif dock == "right":
        css_angle, tx, ty = 90.0, reach - strip / 2, 0.0
    elif dock == "top":
        css_angle, tx, ty = 0.0, 0.0, strip / 2 - reach
    else:  # bottom
        css_angle, tx, ty = 180.0, 0.0, reach - strip / 2

    # PIL rotates counter-clockwise; CSS rotate() is clockwise.
    pet = pet.rotate(-css_angle, expand=True, resample=Image.BICUBIC)

    base = (win_w / 2, win_h / 2)

    # Compose the window contents, then clip to the window rectangle.
    content = Image.new("RGBA", (win_w, win_h), (0, 0, 0, 0))
    place(pet, (base[0] + tx, base[1] + ty), content)

    margin = 60
    canvas = Image.new("RGBA", (win_w + margin * 2, win_h + margin * 2), (24, 22, 20, 255))
    draw = ImageDraw.Draw(canvas)
    ox, oy = margin, margin

    # The window sits flush against the screen edge, so the screen is the
    # half-plane on the inward side of that edge.  Everything else is desktop.
    screen = (58, 54, 50, 255)
    edge = (240, 180, 90, 255)
    if dock == "left":
        draw.rectangle([ox, 0, canvas.width, canvas.height], fill=screen)
        draw.line([(ox, 0), (ox, canvas.height)], fill=edge, width=3)
    elif dock == "right":
        draw.rectangle([0, 0, ox + win_w, canvas.height], fill=screen)
        draw.line([(ox + win_w, 0), (ox + win_w, canvas.height)], fill=edge, width=3)
    elif dock == "top":
        draw.rectangle([0, oy, canvas.width, canvas.height], fill=screen)
        draw.line([(0, oy), (canvas.width, oy)], fill=edge, width=3)
    else:
        draw.rectangle([0, 0, canvas.width, oy + win_h], fill=screen)
        draw.line([(0, oy + win_h), (canvas.width, oy + win_h)], fill=edge, width=3)

    canvas.alpha_composite(content, (ox, oy))
    draw.rectangle([ox, oy, ox + win_w - 1, oy + win_h - 1], outline=(120, 200, 255, 200), width=1)
    draw.text((6, 6), label, fill=(235, 225, 210, 255))

    return canvas


def visible_bell_fraction(dock: str, scale: float) -> float:
    """How much of the bell's length survives the clip, as a fraction."""
    win_w, win_h = docked_window(dock, scale)
    reach = bell_reach(scale)
    bell_len = (478.0 - 366.0) / 486.0 * PET_H * scale

    if dock == "left":
        tip_x = win_w / 2 + (win_w / 2 - reach) + reach  # == strip
        visible = tip_x - max(0.0, tip_x - bell_len)
    elif dock == "right":
        tip_x = win_w / 2 + (reach - win_w / 2) - reach  # == 0
        visible = min(win_w, bell_len) - tip_x
    elif dock == "top":
        tip_y = win_h / 2 + (win_h / 2 - reach) + reach
        visible = tip_y - max(0.0, tip_y - bell_len)
    else:
        tip_y = win_h / 2 + (reach - win_h / 2) - reach
        visible = min(win_h, bell_len) - tip_y
    return max(0.0, min(1.0, visible / bell_len))


def render_free_positions(scale: float) -> Image.Image:
    """The pet at several screen positions, bell aimed at the screen centre.

    Verifies the `aim_at` maths: every pose should point its bell at the cross.
    """
    screen_w, screen_h = 1920.0, 1080.0
    factor = 0.46
    sw, sh = round(screen_w * factor), round(screen_h * factor)
    canvas = Image.new("RGBA", (sw, sh), (44, 42, 40, 255))
    draw = ImageDraw.Draw(canvas)

    cx, cy = screen_w / 2, screen_h / 2
    draw.line([(cx * factor - 9, cy * factor), (cx * factor + 9, cy * factor)],
              fill=(255, 210, 130, 200), width=1)
    draw.line([(cx * factor, cy * factor - 9), (cx * factor, cy * factor + 9)],
              fill=(255, 210, 130, 200), width=1)

    pet = draw_pet(scale)
    win_w, win_h = WINDOW_W, BOTTOM_MARGIN + PET_H * scale + BUBBLE_ROOM

    # Window top-left positions, in screen-relative fractions.
    spots = [(0.06, 0.10), (0.55, 0.06), (0.90, 0.30),
             (0.08, 0.72), (0.62, 0.78), (0.93, 0.80)]

    for fx, fy in spots:
        wx, wy = fx * (screen_w - win_w), fy * (screen_h - win_h)
        # pet::pet_centre_in_window
        pcx = wx + win_w / 2
        pcy = wy + win_h - BOTTOM_MARGIN - PET_H * scale / 2

        # pet::aim_at  ->  theta = atan2(-dx, dy)
        angle = math.degrees(math.atan2(-(cx - pcx), cy - pcy))
        rotated = pet.rotate(-angle, expand=True, resample=Image.BICUBIC)

        sx, sy = pcx * factor, pcy * factor
        place(rotated, (sx, sy), canvas)
        draw.line([(sx, sy), (cx * factor, cy * factor)], fill=(255, 170, 60, 70), width=1)

    draw.text((6, 6), "free state — bell aims at screen centre", fill=(230, 220, 205, 255))
    return canvas


def main() -> None:
    OUT.mkdir(parents=True, exist_ok=True)

    print("geometry (size=large):")
    scale = SIZES["large"]
    print(f"  pet box            {PET_W * scale:.1f} x {PET_H * scale:.1f}")
    print(f"  bell reach         {bell_reach(scale):.1f} px from pet centre")
    bell_len = (478.0 - 366.0) / 486.0 * PET_H * scale
    print(f"  bell length        {bell_len:.1f} px")
    print(f"  dock strip         {40.0 * scale:.1f} px")
    print()
    for dock in ("left", "right", "top", "bottom"):
        w, h = docked_window(dock, scale)
        frac = visible_bell_fraction(dock, scale)
        print(f"  dock={dock:<7} window {w:>3}x{h:<3}  bell visible {frac * 100:5.1f}%")

    tiles = []
    for size in ("large", "medium", "small"):
        for dock in ("left", "right", "top", "bottom"):
            tiles.append(render_dock(dock, SIZES[size], f"{size} / {dock}"))

    pad = 8
    cols = 4
    rows = math.ceil(len(tiles) / cols)
    tw = max(t.width for t in tiles)
    th = max(t.height for t in tiles)
    sheet = Image.new(
        "RGBA", (cols * (tw + pad) + pad, rows * (th + pad) + pad), (18, 18, 18, 255)
    )
    for i, tile in enumerate(tiles):
        r, c = divmod(i, cols)
        sheet.alpha_composite(tile, (pad + c * (tw + pad), pad + r * (th + pad)))

    out = OUT / "dock-simulation.png"
    sheet.save(out)
    print(f"\nwrote {out.relative_to(ROOT)}")

    free = render_free_positions(SIZES["small"])
    free_out = OUT / "aim-simulation.png"
    free.save(free_out)
    print(f"wrote {free_out.relative_to(ROOT)}")


if __name__ == "__main__":
    main()
