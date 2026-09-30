#!/usr/bin/env python3
"""Draw a simple original Relay icon: two overlapping rounded squares + arrows."""

from pathlib import Path

from PIL import Image, ImageDraw


def rounded_rect(draw: ImageDraw.ImageDraw, box, radius: int, fill: tuple[int, int, int, int]) -> None:
    draw.rounded_rectangle(box, radius=radius, fill=fill)


def chevron(draw: ImageDraw.ImageDraw, cx: int, cy: int, size: int, direction: int, fill) -> None:
    # direction: 1 = right, -1 = left
    half = size // 2
    if direction > 0:
        pts = [(cx - half, cy - half), (cx + half, cy), (cx - half, cy + half)]
    else:
        pts = [(cx + half, cy - half), (cx - half, cy), (cx + half, cy + half)]
    draw.polygon(pts, fill=fill)


def main() -> None:
    size = 1024
    img = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    draw = ImageDraw.Draw(img)

    # Soft tile so the icon reads at small sizes.
    draw.rounded_rectangle((48, 48, 976, 976), radius=220, fill=(15, 23, 42, 255))

    back = (13, 148, 136, 255)
    front = (45, 212, 191, 255)
    ink = (15, 23, 42, 255)
    white = (248, 250, 252, 255)

    rounded_rect(draw, (170, 210, 690, 730), 140, back)
    rounded_rect(draw, (334, 294, 854, 814), 140, front)

    # Cut a smaller inner plate so the arrows sit cleanly.
    rounded_rect(draw, (390, 350, 798, 758), 110, (240, 253, 250, 255))

    chevron(draw, 530, 554, 110, 1, ink)
    chevron(draw, 660, 554, 110, -1, (15, 118, 110, 255))
    # Small highlight so it still reads as two arrows, not an X.
    draw.ellipse((588, 534, 628, 574), fill=white)

    out = Path(__file__).resolve().parents[1] / "app-icon.png"
    img.save(out, "PNG")
    print(f"wrote {out}")


if __name__ == "__main__":
    main()
