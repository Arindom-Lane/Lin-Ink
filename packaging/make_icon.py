#!/usr/bin/env python3
"""Generates the Pen Touch icon: pen-touch-256.png (menu/AppImage) and icon64.rgba (window icon)."""
from PIL import Image, ImageDraw
import math

S = 1024                       # draw big, then downsample for smooth edges
img = Image.new("RGBA", (S, S), (0, 0, 0, 0))

# rounded-square background with a vertical indigo -> teal gradient
bg = Image.new("RGBA", (S, S))
px = bg.load()
top, bot = (72, 61, 190), (20, 150, 170)
for y in range(S):
    t = y / (S - 1)
    c = tuple(int(top[i] + (bot[i] - top[i]) * t) for i in range(3)) + (255,)
    for x in range(S):
        px[x, y] = c
mask = Image.new("L", (S, S), 0)
ImageDraw.Draw(mask).rounded_rectangle((24, 24, S - 24, S - 24), radius=230, fill=255)
img.paste(bg, (0, 0), mask)

d = ImageDraw.Draw(img, "RGBA")
cx, cy = 400, 640              # touch point
for r, a in ((250, 70), (170, 120), (90, 255)):
    d.ellipse((cx - r, cy - r, cx + r, cy + r), outline=(255, 255, 255, a), width=26 if r != 90 else 0,
              fill=(255, 255, 255, 255) if r == 90 else None)

# pen: a rotated rounded body with a cone tip ending exactly on the touch point
ang = math.radians(-50)         # pen points down-left toward the touch point
def rot(px_, py_):
    x, y = px_ - cx, py_ - cy
    return (cx + x * math.cos(ang) - y * math.sin(ang), cy + x * math.sin(ang) + y * math.cos(ang))
w = 62
tip_len, body_len = 130, 520
body = [rot(cx - w, cy - tip_len - body_len), rot(cx + w, cy - tip_len - body_len),
        rot(cx + w, cy - tip_len), rot(cx - w, cy - tip_len)]
tip = [rot(cx - w, cy - tip_len), rot(cx + w, cy - tip_len), rot(cx, cy - 6)]
d.polygon(body, fill=(255, 255, 255, 255))
d.polygon(tip, fill=(235, 235, 245, 255))
d.polygon([rot(cx - 16, cy - 44), rot(cx + 16, cy - 44), rot(cx, cy - 6)], fill=(60, 50, 150, 255))
band = [rot(cx - w, cy - tip_len - 40), rot(cx + w, cy - tip_len - 40), rot(cx + w, cy - tip_len - 90), rot(cx - w, cy - tip_len - 90)]
d.polygon(band, fill=(110, 100, 220, 255))

out = img.resize((256, 256), Image.LANCZOS)
out.save("pen-touch-256.png")
open("icon64.rgba", "wb").write(img.resize((64, 64), Image.LANCZOS).tobytes())
print("wrote pen-touch-256.png and icon64.rgba")
