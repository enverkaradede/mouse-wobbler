"""Generate a 512x512 RGBA PNG icon for Mouse Wobbler."""
import struct, zlib, math

def make_chunk(tag: bytes, data: bytes) -> bytes:
    payload = tag + data
    return struct.pack(">I", len(data)) + payload + struct.pack(">I", zlib.crc32(payload) & 0xFFFFFFFF)

def write_png(path, size, pixel_fn):
    raw = bytearray()
    for y in range(size):
        raw.append(0)  # filter = None
        for x in range(size):
            raw.extend(pixel_fn(x, y, size))

    ihdr = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)  # 8-bit RGBA
    idat = zlib.compress(bytes(raw), 9)

    with open(path, "wb") as f:
        f.write(b"\x89PNG\r\n\x1a\n")
        f.write(make_chunk(b"IHDR", ihdr))
        f.write(make_chunk(b"IDAT", idat))
        f.write(make_chunk(b"IEND", b""))

def pixel(x, y, size):
    cx, cy = size / 2.0, size / 2.0
    dx, dy = x - cx, y - cy
    dist = math.hypot(dx, dy)
    outer_r = size / 2.0 - 4
    ring_w  = size * 0.06

    if dist > outer_r:
        return (0, 0, 0, 0)  # fully transparent

    # Soft edge anti-alias
    aa = min(1.0, (outer_r - dist) / max(1, ring_w * 0.5))

    # Gradient: deep navy → vivid blue
    t = dist / outer_r                  # 0 = center, 1 = edge
    r = int(30  + 50  * (1 - t))
    g = int(100 + 60  * (1 - t))
    b = int(220 + 30  * (1 - t))

    # Highlight streak (top-left)
    angle = math.atan2(dy, dx)
    streak = max(0, math.cos(angle + math.pi * 0.75)) ** 4
    r = min(255, r + int(80 * streak * (1 - t * 0.5)))
    g = min(255, g + int(60 * streak * (1 - t * 0.5)))
    b = min(255, b + int(30 * streak * (1 - t * 0.5)))

    # Mouse cursor shape hint: small white dot in centre
    if dist < size * 0.06:
        fade = 1 - dist / (size * 0.06)
        r = int(r + (255 - r) * fade * 0.8)
        g = int(g + (255 - g) * fade * 0.8)
        b = int(b + (255 - b) * fade * 0.8)

    alpha = int(255 * aa)
    return (r, g, b, alpha)

write_png("icon.png", 512, pixel)
print("icon.png created (512x512)")
