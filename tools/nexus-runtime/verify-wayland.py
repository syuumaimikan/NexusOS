"""Check actual compositor output independently of the guest's success log."""
from pathlib import Path
import re
import sys

data = Path(sys.argv[1]).read_bytes()
header = re.match(rb"P6\s+(\d+)\s+(\d+)\s+255\s", data)
if not header:
    raise ValueError("expected QEMU P6 RGB screendump")
width, height = map(int, header.groups())
pixels = data[header.end():]
if len(pixels) != width * height * 3:
    raise ValueError("incomplete screendump")
bands = []
origins = []
for colour in (b"\x33\x99\x66", b"\x22\x44\xee", b"\xcc\x55\x33"):
    widest, first, origin = 0, None, None
    for y in range(height):
        run = 0
        row = pixels[y * width * 3:(y + 1) * width * 3]
        for x in range(width):
            if row[x * 3:x * 3 + 3] == colour:
                run += 1
                if run > widest:
                    widest = run
                    origin = (x - run + 1, y)
                if first is None:
                    first = y
            else:
                run = 0
    if first is None or not 236 <= widest <= 240:
        raise ValueError(f"band missing or incorrect resize: {colour.hex()}, {widest=}")
    bands.append(first)
    origins.append(origin)
if bands != sorted(bands) or len(set(bands)) != 3:
    raise ValueError("bands are not in the client's drawn order")
print("PASS: three client bands visible at resized width 240")
if "--mesa" in sys.argv[2:]:
    x0, _ = origins[0]
    # The test server places its buffer under a native title bar, which clips
    # the start of the green band. The blue band starts exactly 50 rows in.
    y0 = bands[1] - 50
    visible, far = 0, 0
    for y in range(y0, y0 + 150):
        for x in range(x0, x0 + 240):
            red, green, blue = pixels[(y * width + x) * 3:(y * width + x) * 3 + 3]
            if 228 <= red <= 231 and 24 <= green <= 27 and 11 <= blue <= 14:
                if not (x0 + 60 <= x < x0 + 180 and y0 + 50 <= y < y0 + 100):
                    raise ValueError("Mesa triangle is outside its viewport")
                visible += 1
            if 24 <= red <= 27 and 228 <= green <= 231 and 126 <= blue <= 129:
                far += 1
    if not 1400 <= visible <= 2200 or far:
        raise ValueError(f"Mesa triangle or depth occlusion is incorrect: {visible=}, {far=}")
    print("PASS: real Mesa GLSL triangle visible, far triangle occluded")
