#!/usr/bin/env python3
"""Developer tool (not part of the application): derives icon files from the
supplied logo and draws the monochrome tray glyphs. Run from the repo root:

    python3 tools/make_assets.py

Needs Pillow. macOS .icns needs `iconutil` (ships with macOS).
"""
import os, subprocess, shutil
from PIL import Image, ImageDraw

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
A = os.path.join(ROOT, "assets")


def extract_tile(src):
    """Cut the tile out of the supplied artwork with the artwork's own alpha
    channel. The tile is all but opaque (250+) with a clean antialiased edge;
    the glow around it is almost transparent (32 at most) and its colour values
    are junk, so the file must never be flattened to RGB before it is masked."""
    im = Image.open(src).convert("RGBA")
    lo, hi = 40, 245  # above the glow, below the tile body
    alpha = im.getchannel("A").point(lambda v: 0 if v <= lo else min(255, (v - lo) * 255 // (hi - lo)))
    im.putalpha(alpha)
    tile = im.crop(alpha.getbbox())
    tw, th = tile.size
    size = 1024
    inner = int(size * 0.805)
    tile = tile.resize((inner, round(inner * th / tw)), Image.LANCZOS)
    canvas = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    canvas.alpha_composite(tile, ((size - tile.width) // 2, (size - tile.height) // 2))
    return canvas


def save_sizes(icon):
    for n in (16, 32, 48, 64, 128, 256, 512, 1024):
        icon.resize((n, n), Image.LANCZOS).save(os.path.join(A, f"icon-{n}.png"))
    # Windows .ico (real multi-resolution container).
    icon.resize((256, 256), Image.LANCZOS).save(os.path.join(A, "synkflow.ico"), sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)])
    # Linux hicolor tree.
    for n in (16, 32, 48, 64, 128, 256, 512):
        d = os.path.join(A, "linux", "icons", "hicolor", f"{n}x{n}", "apps")
        os.makedirs(d, exist_ok=True)
        icon.resize((n, n), Image.LANCZOS).save(os.path.join(d, "synkflow.png"))
    # macOS .icns through iconutil (a real container, not a renamed PNG).
    iconset = os.path.join(A, "Synkflow.iconset")
    shutil.rmtree(iconset, ignore_errors=True)
    os.makedirs(iconset)
    for n in (16, 32, 128, 256, 512):
        icon.resize((n, n), Image.LANCZOS).save(os.path.join(iconset, f"icon_{n}x{n}.png"))
        icon.resize((n * 2, n * 2), Image.LANCZOS).save(os.path.join(iconset, f"icon_{n}x{n}@2x.png"))
    try:
        subprocess.run(["iconutil", "-c", "icns", iconset, "-o", os.path.join(A, "Synkflow.icns")], check=True)
    finally:
        shutil.rmtree(iconset, ignore_errors=True)


def bezier(p0, p1, p2, p3, n=60):
    out = []
    for i in range(n + 1):
        t = i / n
        a, b, c, d = (1 - t) ** 3, 3 * (1 - t) ** 2 * t, 3 * (1 - t) * t ** 2, t ** 3
        out.append((a * p0[0] + b * p1[0] + c * p2[0] + d * p3[0], a * p0[1] + b * p1[1] + c * p2[1] + d * p3[1]))
    return out


def tray(color, state, px=44):
    """Monochrome mark: two screens joined by an S. The badge is the state:
    filled dot = sharing, two bars = paused, hollow ring = nothing connected."""
    S = 16
    u = px * S / 24.0  # drawing unit (24x24 design grid)
    im = Image.new("RGBA", (px * S, px * S), (0, 0, 0, 0))
    d = ImageDraw.Draw(im)
    w = max(2, int(2.0 * u))

    def cap(p):
        r = w / 2
        d.ellipse((p[0] * u - r, p[1] * u - r, p[0] * u + r, p[1] * u + r), fill=color)

    def stroke(points):
        pix = [(x * u, y * u) for x, y in points]
        d.line(pix, fill=color, width=w, joint="curve")
        cap(points[0])
        cap(points[-1])

    def screen(x0, y0, x1, y1):
        d.rounded_rectangle((x0 * u, y0 * u, x1 * u, y1 * u), radius=1.5 * u, outline=color, width=max(2, int(1.8 * u)))

    # Screens at the two ends of the S.
    screen(13.0, 1.8, 22.6, 8.6)
    screen(1.4, 15.4, 11.0, 22.2)
    # The S: from the upper screen, bulge left, bulge right, into the lower screen.
    path = bezier((13.0, 5.2), (5.5, 5.2), (5.5, 12.0), (11.5, 12.0)) + bezier((11.5, 12.0), (17.5, 12.0), (17.5, 18.8), (11.0, 18.8))[1:]
    stroke(path)
    # State badge, bottom right.
    cx, cy, r = 19.3, 19.3, 3.3
    if state == "active":
        d.ellipse(((cx - r) * u, (cy - r) * u, (cx + r) * u, (cy + r) * u), fill=color)
    elif state == "paused":
        for dx in (-1.5, 1.5):
            stroke([(cx + dx, cy - 2.4), (cx + dx, cy + 2.4)])
    else:
        d.ellipse(((cx - r) * u, (cy - r) * u, (cx + r) * u, (cy + r) * u), outline=color, width=max(2, int(1.7 * u)))
    return im.resize((px, px), Image.LANCZOS)


def main():
    icon = extract_tile(os.path.join(A, "logo-supplied.webp"))
    save_sizes(icon)
    for scheme, color in (("light", (24, 20, 30, 255)), ("dark", (247, 243, 251, 255))):
        for state in ("active", "paused", "idle"):
            tray(color, state).save(os.path.join(A, f"tray-{state}-{scheme}.png"))
    print("assets written to", A)


if __name__ == "__main__":
    main()
