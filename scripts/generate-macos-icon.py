#!/usr/bin/env python3
"""Write a minimal SnapIt.icns (blue tile + crop marks). Used by scripts/bundle-macos.sh."""

from __future__ import annotations

import struct
import sys
import zlib
from pathlib import Path

SIZE = 1024
BLUE = (29, 78, 216, 255)
WHITE = (255, 255, 255, 255)

# PNG-compressed ICNS types used by modern macOS.
ICNS_PNG = (
    (16, b"icp4"),
    (32, b"icp5"),
    (32, b"ic11"),
    (64, b"icp6"),
    (64, b"ic12"),
    (128, b"ic07"),
    (256, b"ic08"),
    (256, b"ic13"),
    (512, b"ic09"),
    (512, b"ic14"),
    (1024, b"ic10"),
)


def png_bytes(width: int, height: int, rgba: bytes) -> bytes:
    def chunk(tag: bytes, data: bytes) -> bytes:
        crc = zlib.crc32(tag + data) & 0xFFFFFFFF
        return struct.pack(">I", len(data)) + tag + data + struct.pack(">I", crc)

    rows = b"".join(b"\x00" + rgba[y * width * 4 : (y + 1) * width * 4] for y in range(height))
    ihdr = struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0)
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", ihdr)
        + chunk(b"IDAT", zlib.compress(rows, 9))
        + chunk(b"IEND", b"")
    )


def rounded_rect_alpha(x: float, y: float, x0: float, y0: float, x1: float, y1: float, radius: float) -> float:
    if x < x0 or x >= x1 or y < y0 or y >= y1:
        return 0.0
    cx = min(max(x, x0 + radius), x1 - radius)
    cy = min(max(y, y0 + radius), y1 - radius)
    in_corner = (x < x0 + radius or x >= x1 - radius) and (y < y0 + radius or y >= y1 - radius)
    if in_corner:
        dist = ((x - cx) ** 2 + (y - cy) ** 2) ** 0.5
        if dist >= radius + 0.5:
            return 0.0
        if dist <= radius - 0.5:
            return 1.0
        return max(0.0, min(1.0, radius + 0.5 - dist))
    return 1.0


def fill_rect(pixels: bytearray, x: int, y: int, w: int, h: int, color: tuple[int, int, int, int]) -> None:
    x0 = max(0, x)
    y0 = max(0, y)
    x1 = min(SIZE, x + w)
    y1 = min(SIZE, y + h)
    r, g, b, a = color
    for yy in range(y0, y1):
        row = yy * SIZE * 4
        for xx in range(x0, x1):
            i = row + xx * 4
            pixels[i : i + 4] = bytes((r, g, b, a))


def render() -> bytes:
    pixels = bytearray(SIZE * SIZE * 4)
    inset = 80
    radius = 220
    x0, y0, x1, y1 = inset, inset, SIZE - inset, SIZE - inset
    for y in range(SIZE):
        row = y * SIZE * 4
        for x in range(SIZE):
            alpha = rounded_rect_alpha(x + 0.5, y + 0.5, x0, y0, x1, y1, radius)
            if alpha <= 0:
                continue
            i = row + x * 4
            pixels[i] = BLUE[0]
            pixels[i + 1] = BLUE[1]
            pixels[i + 2] = BLUE[2]
            pixels[i + 3] = int(round(255 * alpha))

    t = 56
    arm = 210
    pad = inset + 118
    far = SIZE - pad - t
    far_arm = SIZE - pad - arm
    fill_rect(pixels, pad, pad, arm, t, WHITE)
    fill_rect(pixels, pad, pad, t, arm, WHITE)
    fill_rect(pixels, far_arm, pad, arm, t, WHITE)
    fill_rect(pixels, far, pad, t, arm, WHITE)
    fill_rect(pixels, pad, far, arm, t, WHITE)
    fill_rect(pixels, pad, far_arm, t, arm, WHITE)
    fill_rect(pixels, far_arm, far, arm, t, WHITE)
    fill_rect(pixels, far, far_arm, t, arm, WHITE)
    return bytes(pixels)


def downscale(rgba: bytes, dest: int) -> bytes:
    if dest == SIZE:
        return rgba
    factor = SIZE // dest
    out = bytearray(dest * dest * 4)
    area = factor * factor
    for y in range(dest):
        for x in range(dest):
            r = g = b = a = 0
            for yy in range(factor):
                src_row = ((y * factor + yy) * SIZE + x * factor) * 4
                for xx in range(factor):
                    i = src_row + xx * 4
                    r += rgba[i]
                    g += rgba[i + 1]
                    b += rgba[i + 2]
                    a += rgba[i + 3]
            o = (y * dest + x) * 4
            out[o : o + 4] = bytes((r // area, g // area, b // area, a // area))
    return bytes(out)


def build_icns(rgba: bytes) -> bytes:
    entries = bytearray()
    for edge, tag in ICNS_PNG:
        png = png_bytes(edge, edge, downscale(rgba, edge))
        entries.extend(tag)
        entries.extend(struct.pack(">I", 8 + len(png)))
        entries.extend(png)
    return b"icns" + struct.pack(">I", 8 + len(entries)) + bytes(entries)


def main() -> int:
    dest = Path(sys.argv[1]) if len(sys.argv) > 1 else Path("macos/AppIcon.icns")
    dest.parent.mkdir(parents=True, exist_ok=True)
    dest.write_bytes(build_icns(render()))
    print(dest)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
