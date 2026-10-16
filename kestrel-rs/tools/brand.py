#!/usr/bin/env python3
"""Generate Kestrel's brand assets for the desktop client.

Shares its treatment with the Roku channel (../../kestrel-roku/tools/brand.py): the
logo is a plain typographic wordmark, uppercase with generous tracking, no
symbol. There are no third-party source assets — everything is drawn here.

Run after changing the palette, from the kestrel-rs directory:

    python3 tools/brand.py        # needs Pillow: pip install --user pillow
"""

from pathlib import Path

from PIL import Image, ImageDraw

ROOT = Path(__file__).resolve().parent.parent  # kestrel-rs/
OUT = ROOT / "assets"

COPPER = (224, 121, 58)         # #E0793A
COPPER_BRIGHT = (245, 158, 91)  # #F59E5B
INK = (12, 17, 22)              # #0C1116
WHITE = (255, 255, 255)

NAME = "KESTREL"
FONT_BOLD = "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf"
TRACKING = 0.16  # fraction of the font size added between letters


def _font(size: int):
    from PIL import ImageFont
    return ImageFont.truetype(FONT_BOLD, size)


def _tracked_width(draw, text, font, tracking) -> float:
    return sum(draw.textlength(c, font=font) for c in text) + tracking * (len(text) - 1)


def _draw_tracked(draw, xy, text, font, fill, tracking) -> None:
    x, y = xy
    for ch in text:
        draw.text((x, y), ch, font=font, fill=fill)
        x += draw.textlength(ch, font=font) + tracking


def wordmark(width: int, color=WHITE, text: str = NAME) -> Image.Image:
    """The wordmark on transparency, scaled to an exact pixel width.

    Laid out at a large fixed size and downsampled, so proportions and tracking
    hold at every size rather than drifting with the font's hinting.
    """
    font_size = 240
    font = _font(font_size)
    tracking = font_size * TRACKING

    probe = ImageDraw.Draw(Image.new("RGBA", (1, 1)))
    box = probe.textbbox((0, 0), text, font=font)
    text_w = _tracked_width(probe, text, font, tracking)
    text_h = box[3] - box[1]

    pad_x = int(font_size * 0.06)
    pad_y = int(font_size * 0.14)
    total_w = int(text_w + pad_x * 2)
    total_h = int(text_h + pad_y * 2)

    img = Image.new("RGBA", (total_w, total_h), (0, 0, 0, 0))
    _draw_tracked(ImageDraw.Draw(img), (pad_x, pad_y - box[1]), text, font,
                  color + (255,), tracking)

    height = max(1, round(total_h * width / total_w))
    return img.resize((width, height), Image.LANCZOS)


def app_icon(size: int) -> Image.Image:
    """Square launcher icon: a copper K on a rounded black tile.

    The full wordmark is far too wide to survive a square icon slot, so the icon
    takes just the initial — still the same typography, so it reads as the same
    mark. Drawn 4x and downsampled to keep the corner radius clean.

    Dark tile, light letter: on a desktop the icon sits against wallpaper of any
    colour, and the copper reads as the brand while the near-black ground keeps
    it from competing with everything around it.
    """
    ss = size * 4
    img = Image.new("RGBA", (ss, ss), (0, 0, 0, 0))
    draw = ImageDraw.Draw(img)
    draw.rounded_rectangle([0, 0, ss - 1, ss - 1], radius=int(ss * 0.22),
                           fill=INK + (255,))

    font_size = int(ss * 0.60)
    font = _font(font_size)
    box = draw.textbbox((0, 0), "K", font=font)
    draw.text(((ss - (box[2] - box[0])) / 2 - box[0],
               (ss - (box[3] - box[1])) / 2 - box[1]),
              "K", font=font, fill=COPPER + (255,))

    return img.resize((size, size), Image.LANCZOS)


def chevron(size: int, direction: str, color=(147, 161, 175)) -> Image.Image:
    """A small chevron for tree expand/collapse indicators.

    Qt's default branch arrows come from the platform style and are invisible
    against the dark palette, so the sidebar supplies its own.
    """
    ss = size * 4
    img = Image.new("RGBA", (ss, ss), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)

    pad = ss * 0.30
    mid = ss / 2
    if direction == "right":
        points = [(pad, pad * 0.7), (ss - pad, mid), (pad, ss - pad * 0.7)]
    else:  # down
        points = [(pad * 0.7, pad), (mid, ss - pad), (ss - pad * 0.7, pad)]
    d.line(points, fill=color + (255,), width=max(2, int(ss * 0.075)), joint="curve")

    return img.resize((size, size), Image.LANCZOS)


def about_banner(width: int = 520) -> Image.Image:
    """Wordmark centred on ink with a copper rule beneath — used in About."""
    height = int(width * 0.30)
    img = Image.new("RGBA", (width, height), INK + (255,))
    mark = wordmark(int(width * 0.62))
    img.alpha_composite(mark, ((width - mark.width) // 2,
                               int(height * 0.42 - mark.height / 2)))

    rule_w, rule_h = int(width * 0.10), max(2, int(height * 0.018))
    rule = Image.new("RGBA", (rule_w, rule_h), COPPER + (255,))
    img.alpha_composite(rule, ((width - rule_w) // 2, int(height * 0.66)))
    return img


def main() -> None:
    OUT.mkdir(parents=True, exist_ok=True)

    wordmark(440).save(OUT / "wordmark.png")
    wordmark(440, color=COPPER).save(OUT / "wordmark-copper.png")
    print(f"wrote wordmark.png, wordmark-copper.png (440px wide)")

    for size in (16, 24, 32, 48, 64, 128, 256, 512):
        app_icon(size).save(OUT / f"icon-{size}.png")
    print("wrote icon-{16,24,32,48,64,128,256,512}.png")

    about_banner().save(OUT / "about-banner.png")
    print("wrote about-banner.png")

    for direction in ("right", "down"):
        chevron(12, direction).save(OUT / f"chevron-{direction}.png")
    print("wrote chevron-{right,down}.png")


if __name__ == "__main__":
    main()
