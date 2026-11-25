"""Generate assets/syncapp.ico: the SyncApp icon (blue rounded square with a white
"download into drive" glyph) at 16-256 px, anti-aliased. Same design as the tray/window
icon drawn in src/main.rs. Re-run only if the design changes:

    python scripts/make_icon.py
"""
import os
import struct
import zlib

BLUE = (37, 99, 235)
SIZES = [16, 24, 32, 48, 64, 128, 256]
SS = 4  # supersampling per axis


def inside(u, v):
    """Shape test in the 32x32 design grid. Returns 'bg', 'glyph' or None."""
    r = 7.0
    cx = min(max(u, r), 32 - r)
    cy = min(max(v, r), 32 - r)
    if (u - cx) ** 2 + (v - cy) ** 2 > r * r or not (0 <= u <= 32 and 0 <= v <= 32):
        return None
    shaft = 14 <= u < 18 and 6 <= v < 17
    head = 15 <= v < 22 and abs(u - 16) <= 22 - v
    base = 8 <= u < 24 and 23 <= v < 26
    sides = (8 <= u < 11 or 21 <= u < 24) and 19 <= v < 26
    return "glyph" if (shaft or head or base or sides) else "bg"


def render(size):
    px = bytearray()
    n = size * SS
    for y in range(size):
        px.append(0)  # PNG filter: none
        for x in range(size):
            cov_bg = cov_glyph = 0
            for sy in range(SS):
                for sx in range(SS):
                    u = (x * SS + sx + 0.5) / n * 32
                    v = (y * SS + sy + 0.5) / n * 32
                    k = inside(u, v)
                    if k == "bg":
                        cov_bg += 1
                    elif k == "glyph":
                        cov_glyph += 1
            total = cov_bg + cov_glyph
            a = total / (SS * SS)
            if total:
                w = cov_glyph / total
                rgb = [round(BLUE[i] * (1 - w) + 255 * w) for i in range(3)]
            else:
                rgb = [0, 0, 0]
            px += bytes(rgb + [round(a * 255)])
    return bytes(px)


def png(size):
    def chunk(t, d):
        return struct.pack(">I", len(d)) + t + d + struct.pack(">I", zlib.crc32(t + d) & 0xFFFFFFFF)

    ihdr = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", ihdr) + chunk(b"IDAT", zlib.compress(render(size), 9)) + chunk(b"IEND", b"")


def main():
    images = [png(s) for s in SIZES]
    header = struct.pack("<HHH", 0, 1, len(images))
    offset = 6 + 16 * len(images)
    entries = b""
    for s, img in zip(SIZES, images):
        dim = 0 if s >= 256 else s
        entries += struct.pack("<BBBBHHII", dim, dim, 0, 0, 1, 32, len(img), offset)
        offset += len(img)
    out = os.path.join(os.path.dirname(__file__), "..", "assets", "syncapp.ico")
    os.makedirs(os.path.dirname(out), exist_ok=True)
    with open(out, "wb") as f:
        f.write(header + entries + b"".join(images))
    print("wrote", os.path.normpath(out), os.path.getsize(out), "bytes")


if __name__ == "__main__":
    main()
