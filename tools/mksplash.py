#!/usr/bin/env python3
"""Bakes the splash title: the kernel draws it before it loads the fonts."""
import struct
from PIL import Image, ImageDraw, ImageFont

SIZE = 140
font = ImageFont.truetype("assets/fonts/Inter-SemiBold.ttf", SIZE)
image = Image.new("L", (SIZE * 6, SIZE * 2), 0)
ImageDraw.Draw(image).text((SIZE // 2, SIZE // 4), "GlassOS", fill=255, font=font)
image = image.crop(image.getbbox())
with open("assets/splash-logo.bin", "wb") as out:
    out.write(struct.pack("<HH", *image.size))
    out.write(image.tobytes())
print(image.size)
