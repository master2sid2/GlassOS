"""Draws the GlassOS logo and the GitHub social preview in the style of the system:
the first wallpaper (diagonal gradient with glows), bokeh circles and a frosted glass pane."""
import math, random, sys
from PIL import Image, ImageDraw, ImageFilter, ImageFont, ImageChops

# Usage: python3 tools/logo.py [FONTS_DIR] [OUT_DIR] — needs Pillow.
FONTS = sys.argv[1] if len(sys.argv) > 1 else "assets/fonts"
OUT = sys.argv[2] if len(sys.argv) > 2 else "docs/images"
SEMI = f"{FONTS}/Inter-SemiBold.ttf"
REG = f"{FONTS}/Inter-Regular.ttf"

FROM, TO = (0x0F, 0x12, 0x26), (0x24, 0x16, 0x40)
GLOWS = [(0.20, 0.25, 0.65, 0x22D3EE, 0.30), (0.85, 0.85, 0.75, 0xEC4899, 0.28), (0.60, 0.05, 0.50, 0x818CF8, 0.20)]
ACCENT = (0x7D, 0xD3, 0xFC)


def rgb(c):
    return (c >> 16 & 255, c >> 8 & 255, c & 255)


def wallpaper(w, h, seed, bokeh=True, density=9000):
    img = Image.new("RGB", (w, h))
    px = img.load()
    for y in range(h):
        for x in range(w):
            t = (x / w + y / h) / 2
            px[x, y] = tuple(int(a + (b - a) * t) for a, b in zip(FROM, TO))
    glow = Image.new("RGB", (w, h))
    for gx, gy, r, c, a in GLOWS:
        layer = Image.new("RGB", (w, h))
        d = ImageDraw.Draw(layer)
        R = r * max(w, h) * 0.55
        col = tuple(int(v * a * 2.2) for v in rgb(c))
        d.ellipse((gx * w - R, gy * h - R, gx * w + R, gy * h + R), fill=col)
        layer = layer.filter(ImageFilter.GaussianBlur(R * 0.45))
        glow = ImageChops.add(glow, layer)
    img = ImageChops.add(img, glow)
    if bokeh:
        rnd = random.Random(seed)
        over = Image.new("RGBA", (w, h))
        d = ImageDraw.Draw(over)
        cols = [0x22D3EE, 0xEC4899, 0x818CF8, 0xF472B6, 0xFB923C]
        for _ in range(int(w * h / density)):
            r = rnd.uniform(0.02, 0.11) * min(w, h)
            x, y = rnd.uniform(-r, w + r), rnd.uniform(-r, h + r)
            c = rgb(rnd.choice(cols))
            d.ellipse((x - r, y - r, x + r, y + r), fill=c + (rnd.randint(14, 34),), outline=c + (rnd.randint(40, 80),), width=max(1, int(min(w, h) / 400)))
        over = over.filter(ImageFilter.GaussianBlur(min(w, h) / 500))
        img = Image.alpha_composite(img.convert("RGBA"), over).convert("RGB")
    return img


def rounded_mask(size, box, radius):
    m = Image.new("L", size, 0)
    ImageDraw.Draw(m).rounded_rectangle(box, radius, fill=255)
    return m


def glass(img, box, radius, blur, tint=(255, 255, 255), alpha=34):
    """Frosted glass: blurred background inside the pane, a light tint, a top highlight and a rim."""
    w, h = img.size
    mask = rounded_mask(img.size, box, radius)
    frosted = img.filter(ImageFilter.GaussianBlur(blur))
    img.paste(frosted, (0, 0), mask)
    over = Image.new("RGBA", img.size)
    d = ImageDraw.Draw(over)
    d.rounded_rectangle(box, radius, fill=tint + (alpha,))
    # The highlight: brighter at the top edge, fading down.
    x0, y0, x1, y1 = box
    hl = Image.new("L", img.size, 0)
    hd = ImageDraw.Draw(hl)
    for i in range(int((y1 - y0) * 0.5)):
        hd.line((x0, y0 + i, x1, y0 + i), fill=int(46 * (1 - i / ((y1 - y0) * 0.5)) ** 2))
    hl = ImageChops.multiply(hl, mask)
    over = Image.alpha_composite(over, Image.merge("RGBA", (*[Image.new("L", img.size, 255)] * 3, hl)))
    d = ImageDraw.Draw(over)
    rim = max(2, int(min(w, h) / 260))
    d.rounded_rectangle(box, radius, outline=(255, 255, 255, 70), width=rim)
    return Image.alpha_composite(img.convert("RGBA"), over).convert("RGB")


def shadow(img, box, radius, spread, alpha=120):
    sh = Image.new("RGBA", img.size)
    x0, y0, x1, y1 = box
    ImageDraw.Draw(sh).rounded_rectangle((x0, y0 + spread * 0.4, x1, y1 + spread * 0.4), radius, fill=(0, 0, 0, alpha))
    sh = sh.filter(ImageFilter.GaussianBlur(spread))
    return Image.alpha_composite(img.convert("RGBA"), sh).convert("RGB")


def mark(img, cx, cy, s):
    """The glyph: a window frame with a title bar dot row and a 'G' — glass + OS."""
    d = ImageDraw.Draw(img, "RGBA")
    font = ImageFont.truetype(SEMI, int(s * 1.0))
    text = "G"
    bb = d.textbbox((0, 0), text, font=font)
    tw, th = bb[2] - bb[0], bb[3] - bb[1]
    # A soft glow behind the letter.
    g = Image.new("RGBA", img.size)
    gd = ImageDraw.Draw(g)
    gd.text((cx - tw / 2 - bb[0], cy - th / 2 - bb[1]), text, font=font, fill=ACCENT + (200,))
    g = g.filter(ImageFilter.GaussianBlur(s * 0.08))
    out = Image.alpha_composite(img.convert("RGBA"), g)
    d = ImageDraw.Draw(out)
    d.text((cx - tw / 2 - bb[0], cy - th / 2 - bb[1]), text, font=font, fill=(240, 250, 255, 255))
    return out.convert("RGB")


def dots(img, x, y, r):
    d = ImageDraw.Draw(img)
    for i, c in enumerate([(0xF8, 0x71, 0x71), (0xFB, 0xBF, 0x24), (0x4A, 0xDE, 0x80)]):
        cx = x + i * r * 3.2
        d.ellipse((cx - r, y - r, cx + r, y + r), fill=c)


def logo(size):
    S = size * 2
    img = wallpaper(S, S, 7, density=14000)
    pad = S * 0.14
    box = (pad, pad, S - pad, S - pad)
    rad = S * 0.16
    img = shadow(img, box, rad, S * 0.04)
    img = glass(img, box, rad, S * 0.05)
    dots(img, pad + S * 0.09, pad + S * 0.085, S * 0.022)
    img = mark(img, S / 2, S / 2 + S * 0.035, S * 0.42)
    # The icon itself is a rounded square.
    m = rounded_mask((S, S), (0, 0, S, S), S * 0.22)
    out = Image.new("RGBA", (S, S))
    out.paste(img, (0, 0), m)
    return out.resize((size, size), Image.LANCZOS)


def preview(w, h):
    S = 2
    W, H = w * S, h * S
    img = wallpaper(W, H, 3, density=26000)
    box = (W * 0.06, H * 0.17, W * 0.94, H * 0.83)
    rad = H * 0.07
    img = shadow(img, box, rad, H * 0.04)
    img = glass(img, box, rad, H * 0.035, alpha=28)
    dots(img, box[0] + H * 0.06, box[1] + H * 0.06, H * 0.013)
    # The icon on the left of the card.
    icon = logo(int(H * 0.40))
    ix, iy = int(box[0] + H * 0.09), int((H - icon.size[1]) / 2)
    img.paste(icon, (ix, iy), icon)
    d = ImageDraw.Draw(img)
    tx = ix + icon.size[0] + H * 0.07
    title = ImageFont.truetype(SEMI, int(H * 0.15))
    sub = ImageFont.truetype(REG, int(H * 0.044))
    small = ImageFont.truetype(REG, int(H * 0.034))
    d.text((tx, H * 0.30), "GlassOS", font=title, fill=(240, 250, 255))
    d.text((tx, H * 0.505), "An operating system written from scratch by an AI", font=sub, fill=(0xC3, 0xC8, 0xD8))
    d.text((tx, H * 0.585), "Rust kernel  ·  own drivers  ·  frosted-glass desktop  ·  Lua apps", font=small, fill=ACCENT)
    return img.resize((w, h), Image.LANCZOS)


logo(512).save(f"{OUT}/logo.png", optimize=True)
preview(1280, 640).save(f"{OUT}/social-preview.png", optimize=True)
print("ok")
