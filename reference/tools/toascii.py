#!/usr/bin/env python3
"""
Turn the LeBOS logo into a coloured ASCII boot banner.

    python3 toascii.py assets/logo.png 72 > src/banner_art.txt

The committed banner is src/banner.txt: this art with the wordmark appended.

Gitignored, like the other reading aids. Rerun it whenever the logo changes.

Colour is plain ANSI escapes -- the same thing every terminal has understood
since the 1970s. QEMU's -nographic hands the serial line straight to your real
terminal, so the kernel gets colour for free, three milestones before it has a
single pixel of its own. An escape is just bytes: ESC [ 3 8 ; 2 ; r ; g ; b m
means "draw what follows in this colour", and the terminal on the other end
does the work.

An escape is only emitted when the colour CHANGES, and colours are rounded to
the nearest 24 first. Otherwise every character carries 19 bytes of preamble
and the banner is larger than the frame allocator.
"""
import sys, colorsys
from PIL import Image, ImageChops

src   = sys.argv[1]
WIDTH = int(sys.argv[2]) if len(sys.argv) > 2 else 72

img = Image.open(src).convert("RGB")

# The source is mostly black padding; crop to what is actually drawn or the
# prism comes out a dozen rows tall no matter how wide the render is.
bg  = Image.new("RGB", img.size, (0, 0, 0))
box = ImageChops.difference(img, bg).convert("L").point(lambda p: 255 if p > 20 else 0).getbbox()
if box:
    img = img.crop(box)

w, h = img.size
height = max(1, round(WIDTH * h / w * 0.5))   # terminal cells are ~2:1
img = img.resize((WIDTH, height), Image.LANCZOS)

RAMP = " .:-=+*#%@"
ESC = "\x1b"


def quant(c):
    return tuple(min(255, (v + 12) // 24 * 24) for c_ in [c] for v in c_)


rows = []
for y in range(height):
    line, last = [], None
    for x in range(WIDTH):
        r, g, b = img.getpixel((x, y))
        _, sat, val = colorsys.rgb_to_hsv(r / 255, g / 255, b / 255)

        if val < 0.12:
            ch, col = " ", None
        elif sat > 0.30:
            # Coloured light becomes data -- the third layer of the logo, and
            # the only one that survives being reduced to monospace.
            #
            # The hue is kept and the brightness is thrown away. Sampling the
            # image directly gave muddy digits, because the beam fades toward
            # its edges and a dim red on black reads as dirt rather than as
            # light. A character is either there or it is not, so anything
            # drawn at all should be drawn at full strength.
            hh, ss, _ = colorsys.rgb_to_hsv(r / 255, g / 255, b / 255)
            br, bg_, bb = colorsys.hsv_to_rgb(hh, min(1.0, ss * 1.35), 1.0)
            ch = "01"[(x * 7 + y * 3) % 2]
            col = quant((int(br * 255), int(bg_ * 255), int(bb * 255)))
        else:
            ch, col = RAMP[min(len(RAMP) - 1, int(val * len(RAMP)))], quant((r, g, b))

        if col != last:
            if col is None:
                line.append(f"{ESC}[0m")
            else:
                line.append(f"{ESC}[38;2;{col[0]};{col[1]};{col[2]}m")
            last = col
        line.append(ch)
    if last is not None:
        line.append(f"{ESC}[0m")
    rows.append("".join(line).rstrip())

while rows and not rows[0].strip():  rows.pop(0)
while rows and not rows[-1].strip(): rows.pop()

print("\n".join(rows))
