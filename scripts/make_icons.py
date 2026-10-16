"""Generate ExcelScry's application icons.

Tauri needs a multi-resolution .ico for Windows plus a few PNG sizes. Drawing
the master large and downsampling with LANCZOS keeps the strokes smooth.

Run from the repository root:  python scripts/make_icons.py
"""

from __future__ import annotations

import math
import pathlib

from PIL import Image, ImageDraw

OUT_DIR = pathlib.Path(__file__).resolve().parent.parent / "src-tauri" / "icons"

# Everything is drawn at this size, then scaled down for each output.
MASTER = 2048

BG_TOP = (27, 42, 74, 255)
BG_BOTTOM = (12, 20, 38, 255)
PANEL = (247, 250, 252, 255)
# Deliberately lighter than the background: the first version used a near
# black band that merged into the backdrop and made the sheet read as if it
# started a row lower than it does.
HEADER = (71, 85, 105, 255)
PANEL_EDGE = (148, 163, 184, 255)
GRID_LINE = (203, 213, 225, 255)
ACCENT_A = (34, 197, 94, 255)  # green
ACCENT_B = (56, 189, 248, 255)  # cyan
ACCENT_C = (251, 191, 36, 255)  # amber
LENS = (226, 232, 240, 255)
GLASS = (56, 189, 248, 46)


def rounded_gradient(size: int, radius: int) -> Image.Image:
    """A vertically graded rounded square, used as the app background."""
    column = Image.new("RGBA", (1, size))
    for y in range(size):
        t = y / max(size - 1, 1)
        column.putpixel(
            (0, y), tuple(round(BG_TOP[i] + (BG_BOTTOM[i] - BG_TOP[i]) * t) for i in range(4))
        )
    background = column.resize((size, size), Image.NEAREST)

    mask = Image.new("L", (size, size), 0)
    ImageDraw.Draw(mask).rounded_rectangle((0, 0, size - 1, size - 1), radius, fill=255)

    out = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    out.paste(background, (0, 0), mask)
    return out


def draw_grid(canvas: Image.Image) -> None:
    """The spreadsheet sitting behind the lens."""
    draw = ImageDraw.Draw(canvas)
    u = MASTER
    left, top = int(u * 0.10), int(u * 0.20)
    right, bottom = int(u * 0.78), int(u * 0.88)
    radius = int(u * 0.035)
    cols, rows = 5, 5
    cell_w = (right - left) / cols
    cell_h = (bottom - top) / rows
    stroke = max(2, int(u * 0.006))

    draw.rounded_rectangle((left, top, right, bottom), radius, fill=PANEL)

    # Header band: draw a fully rounded dark panel, then paste only its top
    # strip so the band keeps rounded top corners and a straight lower edge.
    band = Image.new("RGBA", canvas.size, (0, 0, 0, 0))
    ImageDraw.Draw(band).rounded_rectangle((left, top, right, bottom), radius, fill=HEADER)
    band_bottom = int(top + cell_h)
    strip = band.crop((left, top, right, band_bottom))
    canvas.paste(strip, (left, top), strip)

    for c in range(1, cols):
        x = int(left + c * cell_w)
        draw.line((x, top, x, bottom), fill=GRID_LINE, width=stroke)
    for r in range(1, rows):
        y = int(top + r * cell_h)
        draw.line((left, y, right, y), fill=GRID_LINE, width=stroke)

    def fill_cell(col: int, row: int, colour: tuple[int, int, int, int]) -> None:
        x0 = int(left + col * cell_w) + stroke * 2
        y0 = int(top + row * cell_h) + stroke * 2
        draw.rounded_rectangle(
            (x0, y0, x0 + int(cell_w) - stroke * 4, y0 + int(cell_h) - stroke * 4),
            radius // 2,
            fill=colour,
        )

    # A few filled cells so it reads as data rather than a blank sheet.
    fill_cell(1, 2, ACCENT_A)
    fill_cell(3, 3, ACCENT_B)
    fill_cell(2, 4, ACCENT_C)

    # Drawn last so the sheet keeps a crisp edge against the dark backdrop.
    draw.rounded_rectangle(
        (left, top, right, bottom), radius, outline=PANEL_EDGE, width=stroke
    )


def draw_lens(canvas: Image.Image) -> None:
    """The 'scry': a magnifying glass over the sheet."""
    u = MASTER
    cx, cy = int(u * 0.66), int(u * 0.42)
    r = int(u * 0.20)
    ring = int(u * 0.055)
    handle = int(u * 0.062)

    # Tinted glass, composited so the grid underneath still reads through.
    glass = Image.new("RGBA", canvas.size, (0, 0, 0, 0))
    ImageDraw.Draw(glass).ellipse((cx - r, cy - r, cx + r, cy + r), fill=(*GLASS[:3], 46))
    canvas.alpha_composite(glass)

    draw = ImageDraw.Draw(canvas)
    draw.ellipse((cx - r, cy - r, cx + r, cy + r), outline=LENS, width=ring)

    angle = math.radians(45)
    x1 = cx + int(r * math.cos(angle))
    y1 = cy + int(r * math.sin(angle))
    x2 = x1 + int(u * 0.14)
    y2 = y1 + int(u * 0.14)
    draw.line((x1, y1, x2, y2), fill=LENS, width=handle)
    for px, py in ((x1, y1), (x2, y2)):
        draw.ellipse(
            (px - handle // 2, py - handle // 2, px + handle // 2, py + handle // 2),
            fill=LENS,
        )


def build_master() -> Image.Image:
    canvas = rounded_gradient(MASTER, int(MASTER * 0.22))
    draw_grid(canvas)
    draw_lens(canvas)
    return canvas


def main() -> None:
    OUT_DIR.mkdir(parents=True, exist_ok=True)
    master = build_master()

    png_sizes = {
        "32x32.png": 32,
        "64x64.png": 64,
        "128x128.png": 128,
        "128x128@2x.png": 256,
        "icon.png": 512,
    }
    for name, size in png_sizes.items():
        master.resize((size, size), Image.LANCZOS).save(OUT_DIR / name)

    ico_sizes = [(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)]
    master.save(OUT_DIR / "icon.ico", format="ICO", sizes=ico_sizes)

    root = OUT_DIR.parent.parent
    for path in sorted(OUT_DIR.iterdir()):
        print(f"wrote {path.relative_to(root)} ({path.stat().st_size} bytes)")


if __name__ == "__main__":
    main()
