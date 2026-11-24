#!/usr/bin/env python3
"""Regenerate Floki's raster brand assets from the locked NEO-3 SVGs.

Source of truth for the mark geometry is ``brand/svg/floki-mark-*.svg`` and
``brand/svg/floki-tile-dark.svg``. This script does not invent a second
outline. It:

* rasterizes the obsidian OS tile at 32 and up (supersampled)
* stamps hand-pixel-hinted 16×16 and 24×24 tiles (integer steps, 1 px gaps)
* writes ``floki.ico`` and ``floki.icns``
* writes the 1280×640 GitHub social card and the README lockup banners
* writes the wordmark SVGs and the spectral rule

Run from anywhere:

    python3 brand/scripts/generate_assets.py
"""

from __future__ import annotations

import re
import struct
from io import BytesIO
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

ROOT = Path(__file__).resolve().parents[2]
SVG = ROOT / "brand" / "svg"
FONTS = ROOT / "brand" / "fonts"
ICONS = ROOT / "assets" / "icons"
DOCS_IMG = ROOT / "docs" / "images"

MARK_DARK = SVG / "floki-mark-dark.svg"
MARK_LIGHT = SVG / "floki-mark-light.svg"
TILE_DARK = SVG / "floki-tile-dark.svg"

TAGLINE = (
    "Inspect, compare, and play multi-layer image sequences "
    "with color-accurate OCIO viewing."
)

TOKEN = re.compile(r"[MLQZ]|[-+]?(?:\d*\.\d+|\d+)")

# Hand-authored rows for the dock sizes where a downsample melts the three
# planes together. Each entry is (y, x0, x1) inclusive, painted in order.
# 1 px of badge is left between planes. Shear is a 1 px step, matching the
# Parallax lean (top edge sits to the right of the bottom edge).
ROWS_16 = [
    (4, 5, 14, "top"),
    (5, 4, 13, "top"),
    (7, 5, 11, "middle"),
    (8, 4, 10, "middle"),
    (10, 6, 9, "bottom"),
    (11, 5, 8, "bottom"),
]
ROWS_24 = [
    (7, 7, 18, "top"),
    (8, 6, 17, "top"),
    (9, 6, 17, "top"),
    (11, 7, 15, "middle"),
    (12, 6, 14, "middle"),
    (13, 6, 14, "middle"),
    (15, 7, 11, "bottom"),
    (16, 6, 10, "bottom"),
    (17, 6, 10, "bottom"),
]


def parse_commands(d: str) -> list[tuple]:
    tokens = TOKEN.findall(d)
    cmds: list[tuple] = []
    i = 0
    while i < len(tokens):
        op = tokens[i]
        i += 1
        if op in ("M", "L"):
            cmds.append((op, float(tokens[i]), float(tokens[i + 1])))
            i += 2
        elif op == "Q":
            nums = [float(t) for t in tokens[i : i + 4]]
            cmds.append(("Q", *nums))
            i += 4
        elif op == "Z":
            cmds.append(("Z",))
        else:
            raise SystemExit(f"unexpected path token {op!r}")
    return cmds


def parse_paths(svg_path: Path) -> list[tuple[list[tuple], str]]:
    text = svg_path.read_text(encoding="utf-8")
    found = re.findall(r'<path\s+d="([^"]+)"\s+fill="([^"]+)"\s*/>', text)
    if len(found) != 3:
        raise SystemExit(f"{svg_path} should carry exactly 3 planes, found {len(found)}")
    return [(parse_commands(d), fill) for d, fill in found]


def badge_fill(svg_path: Path) -> str:
    text = svg_path.read_text(encoding="utf-8")
    match = re.search(r'<rect[^>]*fill="(#[0-9A-Fa-f]{6})"', text)
    if not match:
        raise SystemExit(f"{svg_path} has no badge rect")
    return match.group(1)


def hex_rgb(value: str) -> tuple[int, int, int]:
    raw = value.removeprefix("#")
    return (int(raw[0:2], 16), int(raw[2:4], 16), int(raw[4:6], 16))


def flatten(cmds: list[tuple], steps: int = 16) -> list[tuple[float, float]]:
    points: list[tuple[float, float]] = []
    cx = cy = 0.0
    for cmd in cmds:
        if cmd[0] in ("M", "L"):
            cx, cy = cmd[1], cmd[2]
            points.append((cx, cy))
        elif cmd[0] == "Q":
            x1, y1, x2, y2 = cmd[1], cmd[2], cmd[3], cmd[4]
            for step in range(1, steps + 1):
                t = step / steps
                u = 1.0 - t
                points.append(
                    (
                        u * u * cx + 2 * u * t * x1 + t * t * x2,
                        u * u * cy + 2 * u * t * y1 + t * t * y2,
                    )
                )
            cx, cy = x2, y2
    return points


def geometry_bbox(paths: list[tuple[list[tuple], str]]) -> tuple[float, float, float, float]:
    xs: list[float] = []
    ys: list[float] = []
    for cmds, _fill in paths:
        for cmd in cmds:
            if cmd[0] in ("M", "L"):
                xs.append(cmd[1])
                ys.append(cmd[2])
            elif cmd[0] == "Q":
                xs.extend((cmd[1], cmd[3]))
                ys.extend((cmd[2], cmd[4]))
    return min(xs), min(ys), max(xs), max(ys)


def plane_colors(paths: list[tuple[list[tuple], str]]) -> dict[str, tuple[int, int, int]]:
    names = ("top", "middle", "bottom")
    return {name: hex_rgb(fill) for name, (_cmds, fill) in zip(names, paths, strict=True)}


def render_polygons(
    draw: ImageDraw.ImageDraw,
    paths: list[tuple[list[tuple], str]],
    scale: float,
    origin: tuple[float, float],
) -> None:
    ox, oy = origin
    for cmds, fill in paths:
        if fill == "currentColor":
            raise SystemExit("refusing to rasterize the mono mark as an app icon")
        poly = [((x - ox) * scale, (y - oy) * scale) for x, y in flatten(cmds)]
        draw.polygon(poly, fill=(*hex_rgb(fill), 255))


def supersample_for(size: int) -> int:
    if size >= 1024:
        return 2
    if size >= 256:
        return 4
    return 8


def render_tile(size: int, paths: list[tuple[list[tuple], str]], badge: tuple[int, int, int]) -> Image.Image:
    ss = supersample_for(size)
    hi = size * ss
    image = Image.new("RGBA", (hi, hi), (0, 0, 0, 0))
    draw = ImageDraw.Draw(image)
    radius = 144 / 1024 * hi
    draw.rounded_rectangle((0, 0, hi - 1, hi - 1), radius=radius, fill=(*badge, 255))
    render_polygons(draw, paths, hi / 1024, (0.0, 0.0))
    return image.resize((size, size), Image.Resampling.BOX)


def rounded_insets(size: int) -> list[int]:
    """Discrete squircle. 144/1024 of the tile is the corner radius."""
    radius = max(1, round(144 / 1024 * size))
    if size == 16:
        return [2, 1] + [0] * 12 + [1, 2]
    if size == 24:
        return [4, 2, 1] + [0] * 18 + [1, 2, 4]
    # Unused for the hinted sizes; kept so a future size can share the mask.
    inset = [0] * size
    for i in range(radius):
        # A simple circular corner cut, only used if a new hinted size appears.
        cut = radius - int((radius**2 - (radius - i) ** 2) ** 0.5)
        inset[i] = cut
        inset[size - 1 - i] = cut
    return inset


def render_hinted(
    size: int,
    rows: list[tuple[int, int, int, str]],
    colors: dict[str, tuple[int, int, int]],
    badge: tuple[int, int, int],
) -> Image.Image:
    image = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    pixels = image.load()
    insets = rounded_insets(size)
    if len(insets) != size:
        raise SystemExit(f"insets for {size}px must be {size} long, got {len(insets)}")
    for y, inset in enumerate(insets):
        for x in range(inset, size - inset):
            pixels[x, y] = (*badge, 255)
    for y, x0, x1, plane in rows:
        color = colors[plane]
        for x in range(x0, x1 + 1):
            if insets[y] <= x < size - insets[y]:
                pixels[x, y] = (*color, 255)
            else:
                raise SystemExit(f"{size}px {plane} pixel {(x, y)} falls outside the squircle")
    assert_separated(image, set(colors.values()))
    return image


def assert_separated(image: Image.Image, brand: set[tuple[int, int, int]]) -> None:
    pixels = image.load()
    width, height = image.size
    seen: set[tuple[int, int, int]] = set()
    for y in range(height):
        for x in range(width):
            color = pixels[x, y][:3]
            if color not in brand or pixels[x, y][3] != 255:
                continue
            seen.add(color)
            for nx, ny in ((x + 1, y), (x, y + 1)):
                if nx >= width or ny >= height:
                    continue
                neighbor = pixels[nx, ny]
                if neighbor[3] == 255 and neighbor[:3] in brand and neighbor[:3] != color:
                    raise SystemExit(
                        f"brand planes touch at {(x, y)} {color} and {(nx, ny)} {neighbor[:3]}"
                    )
    if seen != brand:
        raise SystemExit(f"hinted icon is missing a plane: have {seen}, want {brand}")


def render_cropped_mark(paths: list[tuple[list[tuple], str]], height: int) -> Image.Image:
    """Bare Parallax mark, cropped to the plane bounds, supersampled to ``height``."""
    min_x, min_y, max_x, max_y = geometry_bbox(paths)
    ss = 4
    scale = (height * ss) / (max_y - min_y)
    hi_w = max(1, int(round((max_x - min_x) * scale)) + 4)
    hi_h = max(1, int(round((max_y - min_y) * scale)) + 4)
    image = Image.new("RGBA", (hi_w, hi_h), (0, 0, 0, 0))
    draw = ImageDraw.Draw(image)
    # 2 px of supersampled padding so the antialiased edge is not clipped.
    render_polygons(draw, paths, scale, (min_x - 2 / scale, min_y - 2 / scale))
    out_h = height
    out_w = max(1, int(round(image.width / ss)))
    return image.resize((out_w, out_h), Image.Resampling.BOX)


def font(path: Path, size: int) -> ImageFont.FreeTypeFont:
    """Open a vendored IBM Plex file.

    ``Layout.BASIC`` draws only that file. Raqm (Pillow's default when it is
    installed) will ask fontconfig for a missing glyph and can substitute a
    system serif for the whole run.
    """
    if not path.is_file():
        raise SystemExit(f"vendored font missing: {path}")
    face = ImageFont.truetype(str(path), size, layout_engine=ImageFont.Layout.BASIC)
    family, style = face.getname()
    if not str(family).startswith("IBM Plex"):
        raise SystemExit(
            f"{path.name} opened as {family} {style}. Refusing a system fallback."
        )
    if face.getlength("Ag") < 1:
        raise SystemExit(f"{path.name} has no Latin glyphs")
    return face


def line_box(face: ImageFont.FreeTypeFont) -> tuple[int, int]:
    ascent, descent = face.getmetrics()
    return ascent, descent


def wrap_text(text: str, face: ImageFont.FreeTypeFont, max_width: float) -> list[str]:
    words = text.split()
    lines: list[str] = []
    current = ""
    for word in words:
        trial = word if not current else f"{current} {word}"
        if face.getlength(trial) <= max_width:
            current = trial
        else:
            if not current:
                raise SystemExit(f"word {word!r} does not fit in {max_width}px")
            lines.append(current)
            current = word
    if current:
        lines.append(current)
    return lines


def draw_tracked(
    draw: ImageDraw.ImageDraw,
    origin: tuple[float, float],
    text: str,
    face: ImageFont.FreeTypeFont,
    fill: tuple[int, int, int],
    tracking: float,
) -> float:
    """Draw ``text`` on one baseline.

    ``origin`` is the left baseline (``anchor='ls'``). Per-glyph ``lt`` alignment
    pins a middle dot to the cap line, which reads as a superscript tick.
    """
    x, y = origin
    for index, ch in enumerate(text):
        draw.text((x, y), ch, font=face, fill=(*fill, 255), anchor="ls")
        x += face.getlength(ch)
        if index != len(text) - 1:
            x += tracking
    return x


def tracked_width(text: str, face: ImageFont.FreeTypeFont, tracking: float) -> float:
    width = sum(face.getlength(ch) for ch in text)
    if len(text) > 1:
        width += tracking * (len(text) - 1)
    return width


def save_png(image: Image.Image, path: Path) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    image.save(path, "PNG", optimize=True)


def write_social(paths: list[tuple[list[tuple], str]], colors: dict[str, tuple[int, int, int]]) -> None:
    """1280×640 card: large FLOKI, tight kicker and tagline, URL near one segmented rule.

    Colors stay NEO-3 Acid Arcade. The rule is a single 7 px bar split into three
    equal side-by-side segments, not three stacked stripes.
    """
    width, height = 1280, 640
    rule_h = 7
    canvas = (0x14, 0x15, 0x18, 255)
    image = Image.new("RGBA", (width, height), canvas)
    draw = ImageDraw.Draw(image)

    mark = render_cropped_mark(paths, 188)
    mark_x = 104
    text_gap = 72
    margin_right = 80
    text_x = mark_x + mark.width + text_gap
    text_w = width - margin_right - text_x
    if text_w < 560:
        raise SystemExit(f"social text column is only {text_w:.0f}px")

    logo_font = font(FONTS / "IBMPlexSans-SemiBold.ttf", 112)
    kicker_font = font(FONTS / "IBMPlexSans-Medium.ttf", 18)
    body_font = font(FONTS / "IBMPlexSans-Regular.ttf", 23)
    url_font = font(FONTS / "IBMPlexMono-Regular.ttf", 18)

    logo = "FLOKI"
    kicker = "IMAGE SEQUENCE VIEWER  ·  BYVFX"
    url = "github.com/byvfx/floki"
    kicker_track = 0.5
    if logo_font.getlength(logo) > text_w:
        raise SystemExit("FLOKI does not fit at 112 px")
    if tracked_width(kicker, kicker_font, kicker_track) > text_w:
        raise SystemExit("social kicker overflows")
    # Break where the reference card breaks: the second line starts at "with".
    first_line = "Inspect, compare, and play multi-layer image sequences"
    body_lines = wrap_text(TAGLINE, body_font, body_font.getlength(first_line) + 2)
    if body_lines != [first_line, "with color-accurate OCIO viewing."]:
        raise SystemExit(f"tagline wrapped unexpectedly: {body_lines}")
    if body_font.getlength(first_line) > text_w:
        raise SystemExit("tagline does not fit the text column")
    if url_font.getlength(url) > text_w:
        raise SystemExit("social url overflows")

    # Baseline-anchored: the dot's center belongs in the x-height, not on the cap line.
    dot = kicker_font.getbbox("·", anchor="ls")
    cap = kicker_font.getbbox("H", anchor="ls")
    dot_mid = (dot[1] + dot[3]) / 2.0
    span = cap[3] - cap[1]
    if span <= 0 or not (0.25 < (dot_mid - cap[1]) / span < 0.85):
        raise SystemExit(f"middle dot is not in the x-height (dot {dot}, cap {cap})")

    def ink(face: ImageFont.FreeTypeFont, sample: str) -> tuple[int, int]:
        _left, top, _right, bottom = face.getbbox(sample, anchor="ls")
        return top, bottom

    logo_top, logo_bot = ink(logo_font, "FLOKI")
    kick_top, kick_bot = ink(kicker_font, "IMAGE")
    body_top, body_bot = ink(body_font, "Inspect")
    url_top_off, _url_bot = ink(url_font, url)
    # Ink gaps, not em-box gaps. The em box pads all-caps FLOKI with an empty
    # descent and pushes the kicker away from the wordmark.
    gap_kick, gap_body, body_lead = 10, 14, 6
    logo_baseline_off = -logo_top
    kick_baseline_off = logo_baseline_off + logo_bot + gap_kick - kick_top
    body_baseline_off = kick_baseline_off + kick_bot + gap_body - body_top
    body_step = (body_bot - body_top) + body_lead
    stack_h = body_baseline_off + body_step + body_bot

    rule_top = height - rule_h
    url_baseline = rule_top - 36
    url_top = url_baseline + url_top_off
    region_top = 44
    region_bottom = url_top - 40
    if stack_h > region_bottom - region_top:
        raise SystemExit(f"headline stack {stack_h:.0f}px does not fit above the URL")
    stack_top = region_top + (region_bottom - region_top - stack_h) / 2

    mark_y = stack_top + (stack_h - mark.height) / 2
    if mark_x < 80 or mark_y < 36:
        raise SystemExit("mark crowds the card edge")
    if mark_y + mark.height > url_top - 12:
        raise SystemExit("mark crowds the URL")

    image.alpha_composite(mark, (mark_x, int(round(mark_y))))

    baseline = stack_top + logo_baseline_off
    draw.text(
        (text_x, baseline),
        logo,
        font=logo_font,
        fill=(0xF4, 0xF5, 0xF7, 255),
        anchor="ls",
    )
    draw_tracked(
        draw,
        (text_x, stack_top + kick_baseline_off),
        kicker,
        kicker_font,
        colors["top"],
        kicker_track,
    )
    for index, line in enumerate(body_lines):
        draw.text(
            (text_x, stack_top + body_baseline_off + index * body_step),
            line,
            font=body_font,
            fill=(0xD8, 0xDC, 0xE4, 255),
            anchor="ls",
        )
    draw.text(
        (text_x, url_baseline),
        url,
        font=url_font,
        fill=(0x8C, 0x91, 0x9E, 255),
        anchor="ls",
    )

    cuts = [round(i * width / 3) for i in range(4)]
    for index, plane in enumerate(("top", "middle", "bottom")):
        draw.rectangle(
            (cuts[index], rule_top, cuts[index + 1], height),
            fill=(*colors[plane], 255),
        )

    pixels = image.load()
    planes = {colors["top"], colors["middle"], colors["bottom"]}
    rule_row = [pixels[x, rule_top][:3] for x in range(width)]
    if set(rule_row) != planes:
        raise SystemExit(f"segmented rule colors drifted: {set(rule_row)}")
    if (
        rule_row[0] != colors["top"]
        or rule_row[width // 2] != colors["middle"]
        or rule_row[-1] != colors["bottom"]
    ):
        raise SystemExit("rule segments are not orange | green | blue")
    above = {pixels[x, rule_top - 1][:3] for x in range(0, width, 4)}
    if planes & above:
        raise SystemExit("the rule is thicker than one bar, or a plane leaked above it")

    save_png(image, DOCS_IMG / "social-preview.png")


def render_hero(paths: list[tuple[list[tuple], str]], colors: dict[str, tuple[int, int, int]], dark: bool) -> Image.Image:
    mark = render_cropped_mark(paths, 132)
    logo_font = font(FONTS / "IBMPlexSans-SemiBold.ttf", 64)
    by_font = font(FONTS / "IBMPlexSans-Medium.ttf", 28)
    by = "  ·  BYVFX"
    logo_w = logo_font.getlength("FLOKI")
    by_w = by_font.getlength(by)
    logo_a, logo_d = line_box(logo_font)
    pad_x, pad_y, gap = 40, 36, 28
    rule = 6
    row_h = max(mark.height, logo_a + logo_d)
    width = int(round(pad_x + mark.width + gap + logo_w + by_w + pad_x))
    height = int(pad_y + row_h + pad_y + rule * 3)
    paper = (0x14, 0x15, 0x18, 255) if dark else (0xFF, 0xFF, 0xFF, 255)
    name = (0xF4, 0xF5, 0xF7, 255) if dark else (0x14, 0x15, 0x18, 255)
    aside = (0x8C, 0x91, 0x9E, 255) if dark else (0x4A, 0x4E, 0x59, 255)
    image = Image.new("RGBA", (width, height), paper)
    mark_y = pad_y + (row_h - mark.height) / 2
    image.alpha_composite(mark, (pad_x, int(round(mark_y))))
    draw = ImageDraw.Draw(image)
    baseline = pad_y + (row_h - (logo_a + logo_d)) / 2 + logo_a
    text_x = pad_x + mark.width + gap
    draw.text((text_x, baseline), "FLOKI", font=logo_font, fill=name, anchor="ls")
    draw.text((text_x + logo_w, baseline), by, font=by_font, fill=aside, anchor="ls")
    for index, plane in enumerate(("top", "middle", "bottom")):
        y0 = height - rule * 3 + index * rule
        draw.rectangle((0, y0, width, y0 + rule), fill=(*colors[plane], 255))
    return image


def write_lockup_svg(
    source: Path,
    dest: Path,
    word_fill: str,
    aside_fill: str,
) -> None:
    """Transparent FLOKI · BYVFX lockup. Text is IBM Plex; paths are the mark."""
    text = source.read_text(encoding="utf-8")
    path_lines = re.findall(r"<path\s+d=\"[^\"]+\"\s+fill=\"[^\"]+\"\s*/>", text)
    paths = parse_paths(source)
    min_x, min_y, max_x, max_y = geometry_bbox(paths)
    mark_h = 132
    scale = mark_h / (max_y - min_y)
    mark_w = (max_x - min_x) * scale
    pad_x, pad_y, gap = 40, 36, 28
    # Width matches the raster hero so the SVG and PNG share one layout.
    logo_font = font(FONTS / "IBMPlexSans-SemiBold.ttf", 64)
    by_font = font(FONTS / "IBMPlexSans-Medium.ttf", 28)
    logo_w = logo_font.getlength("FLOKI")
    by_w = by_font.getlength("  ·  BYVFX")
    logo_a, logo_d = line_box(logo_font)
    row_h = max(mark_h, logo_a + logo_d)
    width = pad_x + mark_w + gap + logo_w + by_w + pad_x
    height = pad_y + row_h + pad_y
    mark_y = pad_y + (row_h - mark_h) / 2
    baseline = pad_y + (row_h - (logo_a + logo_d)) / 2 + logo_a
    tx = pad_x - min_x * scale
    ty = mark_y - min_y * scale
    text_x = pad_x + mark_w + gap
    paths_xml = "\n    ".join(path_lines)
    svg = f"""<?xml version="1.0" encoding="UTF-8"?>
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {width:.1f} {height:.1f}" width="{width:.0f}" height="{height:.0f}" fill="none">
  <title>Floki · BYVFX</title>
  <g transform="translate({tx:.3f} {ty:.3f}) scale({scale:.6f})">
    {paths_xml}
  </g>
  <text x="{text_x:.1f}" y="{baseline:.1f}" font-family="'IBM Plex Sans', sans-serif" font-weight="600" font-size="64" fill="{word_fill}">FLOKI</text>
  <text x="{text_x + logo_w:.1f}" y="{baseline:.1f}" font-family="'IBM Plex Sans', sans-serif" font-weight="500" font-size="28" fill="{aside_fill}">  ·  BYVFX</text>
</svg>
"""
    dest.write_text(svg, encoding="utf-8")


def write_wordmark_svg(source: Path, dest: Path, fill: str) -> None:
    text = source.read_text(encoding="utf-8")
    path_lines = re.findall(r"<path\s+d=\"[^\"]+\"\s+fill=\"[^\"]+\"\s*/>", text)
    paths = parse_paths(source)
    min_x, min_y, max_x, max_y = geometry_bbox(paths)
    mark_h = 132
    scale = mark_h / (max_y - min_y)
    mark_w = (max_x - min_x) * scale
    pad_x, pad_y, gap = 36, 32, 28
    logo_font = font(FONTS / "IBMPlexSans-SemiBold.ttf", 72)
    logo_w = logo_font.getlength("FLOKI")
    logo_a, logo_d = line_box(logo_font)
    row_h = max(mark_h, logo_a + logo_d)
    width = pad_x + mark_w + gap + logo_w + pad_x
    height = pad_y + row_h + pad_y
    mark_y = pad_y + (row_h - mark_h) / 2
    baseline = pad_y + (row_h - (logo_a + logo_d)) / 2 + logo_a
    tx = pad_x - min_x * scale
    ty = mark_y - min_y * scale
    text_x = pad_x + mark_w + gap
    paths_xml = "\n    ".join(path_lines)
    svg = f"""<?xml version="1.0" encoding="UTF-8"?>
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {width:.1f} {height:.1f}" width="{width:.0f}" height="{height:.0f}" fill="none">
  <title>Floki</title>
  <g transform="translate({tx:.3f} {ty:.3f}) scale({scale:.6f})">
    {paths_xml}
  </g>
  <text x="{text_x:.1f}" y="{baseline:.1f}" font-family="'IBM Plex Sans', sans-serif" font-weight="600" font-size="72" fill="{fill}">FLOKI</text>
</svg>
"""
    dest.write_text(svg, encoding="utf-8")


def write_spectral_rule(colors: dict[str, tuple[int, int, int]]) -> None:
    def hx(rgb: tuple[int, int, int]) -> str:
        return "#{:02X}{:02X}{:02X}".format(*rgb)

    svg = f"""<?xml version="1.0" encoding="UTF-8"?>
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1200 18" width="1200" height="18" fill="none">
  <title>Floki spectral rule</title>
  <rect width="1200" height="6" fill="{hx(colors["top"])}"/>
  <rect y="6" width="1200" height="6" fill="{hx(colors["middle"])}"/>
  <rect y="12" width="1200" height="6" fill="{hx(colors["bottom"])}"/>
</svg>
"""
    (SVG / "floki-spectral-rule.svg").write_text(svg, encoding="utf-8")


def png_bytes(image: Image.Image) -> bytes:
    buf = BytesIO()
    image.save(buf, "PNG", optimize=True)
    return buf.getvalue()


def write_ico(frames: dict[int, Image.Image]) -> None:
    order = [16, 24, 32, 48, 64, 128, 256]
    primary = frames[256]
    extra = [frames[size] for size in order if size != 256]
    path = ICONS / "floki.ico"
    primary.save(
        path,
        format="ICO",
        sizes=[(size, size) for size in order],
        append_images=extra,
    )
    check = Image.open(path)
    have = set(check.info["sizes"])
    if have != {(size, size) for size in order}:
        raise SystemExit(f"ico sizes {have} != requested")
    check.size = (16, 16)
    frame = check.convert("RGBA")
    if frame.tobytes() != frames[16].tobytes():
        raise SystemExit("ico 16px frame does not match the hand-hinted PNG")
    check.size = (24, 24)
    frame = check.convert("RGBA")
    if frame.tobytes() != frames[24].tobytes():
        raise SystemExit("ico 24px frame does not match the hand-hinted PNG")


def write_icns(frames: dict[int, Image.Image]) -> None:
    # PNG-encoded icns entries. Retina types repeat the matching pixel buffer.
    entries = [
        (b"icp4", 16),
        (b"icp5", 32),
        (b"ic11", 32),
        (b"icp6", 64),
        (b"ic12", 64),
        (b"ic07", 128),
        (b"ic08", 256),
        (b"ic13", 256),
        (b"ic09", 512),
        (b"ic14", 512),
        (b"ic10", 1024),
    ]
    chunks: list[bytes] = []
    for ostype, size in entries:
        blob = png_bytes(frames[size])
        chunks.append(ostype + struct.pack(">I", 8 + len(blob)) + blob)
    body = b"".join(chunks)
    data = b"icns" + struct.pack(">I", 8 + len(body)) + body
    (ICONS / "floki.icns").write_bytes(data)


def main() -> None:
    dark_paths = parse_paths(MARK_DARK)
    light_paths = parse_paths(MARK_LIGHT)
    tile_paths = parse_paths(TILE_DARK)
    colors = plane_colors(dark_paths)
    light_colors = plane_colors(light_paths)
    badge = hex_rgb(badge_fill(TILE_DARK))
    if badge != (0x14, 0x15, 0x18):
        raise SystemExit(f"tile badge drifted from #141518: {badge}")
    if colors["top"] != (0xFF, 0x55, 0x00) or colors["middle"] != (0x38, 0xFF, 0x4F):
        raise SystemExit(f"dark mark colors drifted: {colors}")
    if colors["bottom"] != (0x00, 0xAA, 0xFF):
        raise SystemExit(f"dark blue drifted: {colors['bottom']}")
    if light_colors != {
        "top": (0xFF, 0x55, 0x00),
        "middle": (0x00, 0xAA, 0x14),
        "bottom": (0x00, 0x99, 0xE6),
    }:
        raise SystemExit(f"light mark colors drifted: {light_colors}")

    ICONS.mkdir(parents=True, exist_ok=True)
    frames: dict[int, Image.Image] = {
        16: render_hinted(16, ROWS_16, colors, badge),
        24: render_hinted(24, ROWS_24, colors, badge),
    }
    for size in (32, 48, 64, 128, 256, 512, 1024):
        frames[size] = render_tile(size, tile_paths, badge)
    for size, image in frames.items():
        save_png(image, ICONS / f"floki-{size}.png")
    write_ico(frames)
    write_icns(frames)

    write_spectral_rule(colors)
    write_wordmark_svg(MARK_DARK, SVG / "floki-lockup-dark.svg", "#F4F5F7")
    write_wordmark_svg(MARK_LIGHT, SVG / "floki-lockup-light.svg", "#141518")
    write_lockup_svg(MARK_DARK, SVG / "floki-lockup-byvfx-dark.svg", "#F4F5F7", "#8C919E")
    write_lockup_svg(MARK_LIGHT, SVG / "floki-lockup-byvfx-light.svg", "#141518", "#4A4E59")

    write_social(dark_paths, colors)
    save_png(render_hero(dark_paths, colors, dark=True), DOCS_IMG / "readme-lockup-dark.png")
    save_png(
        render_hero(light_paths, light_colors, dark=False),
        DOCS_IMG / "readme-lockup-light.png",
    )
    print("wrote icons, icns, social card, and lockups")


if __name__ == "__main__":
    main()
