#!/usr/bin/env python3
"""Regenerates packaging/icons/zebra-crosslink.ico from the master art.

    python packaging/make_icons.py

Master art is zebra-gui/assets/favicon.png, which is also what the running window uses.
The .ico is committed so no build step needs an image library; zebrad and zebra-gui
compile it into their .exe as the icon Explorer and the taskbar read.

The master is 128x128, so no 256 entry is emitted and Windows scales that one down.
A 256 layer would need larger source art.

Standard library only: zlib does the PNG decompression, everything else is here.
"""

import os
import struct
import sys
import zlib

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
MASTER = os.path.join(ROOT, "zebra-gui", "assets", "favicon.png")
OUT = os.path.join(ROOT, "packaging", "icons")
NAME = "zebra-crosslink"

ICO_SIZES = [16, 24, 32, 48, 64, 128]


def png_decode(path):
    """Returns (width, height, RGBA bytes) for an 8-bit non-interlaced PNG."""
    data = open(path, "rb").read()
    if data[:8] != b"\x89PNG\r\n\x1a\n":
        raise ValueError(f"{path}: not a PNG")

    idat = bytearray()
    width = height = color_type = bit_depth = None
    i = 8
    while i < len(data):
        (length,) = struct.unpack(">I", data[i:i + 4])
        kind = data[i + 4:i + 8]
        body = data[i + 8:i + 8 + length]
        if kind == b"IHDR":
            width, height, bit_depth, color_type, _, _, interlace = struct.unpack(">IIBBBBB", body)
            if bit_depth != 8 or interlace != 0:
                raise ValueError(f"{path}: expected 8-bit non-interlaced, got depth {bit_depth} interlace {interlace}")
            if color_type not in (2, 6):
                raise ValueError(f"{path}: expected RGB or RGBA, got colour type {color_type}")
        elif kind == b"IDAT":
            idat += body
        elif kind == b"IEND":
            break
        i += 12 + length

    channels = 4 if color_type == 6 else 3
    raw = zlib.decompress(bytes(idat))
    stride = width * channels
    out = bytearray(width * height * 4)
    prev = bytearray(stride)
    pos = 0
    for y in range(height):
        filter_type = raw[pos]
        pos += 1
        line = bytearray(raw[pos:pos + stride])
        pos += stride
        for x in range(stride):
            a = line[x - channels] if x >= channels else 0
            b = prev[x]
            c = prev[x - channels] if x >= channels else 0
            if filter_type == 1:
                line[x] = (line[x] + a) & 0xFF
            elif filter_type == 2:
                line[x] = (line[x] + b) & 0xFF
            elif filter_type == 3:
                line[x] = (line[x] + ((a + b) >> 1)) & 0xFF
            elif filter_type == 4:
                p = a + b - c
                pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
                pred = a if (pa <= pb and pa <= pc) else (b if pb <= pc else c)
                line[x] = (line[x] + pred) & 0xFF
            elif filter_type != 0:
                raise ValueError(f"{path}: unknown row filter {filter_type}")
        if channels == 4:
            out[y * width * 4:(y + 1) * width * 4] = line
        else:
            for x in range(width):
                out[(y * width + x) * 4:(y * width + x) * 4 + 4] = bytes(line[x * 3:x * 3 + 3]) + b"\xff"
        prev = line

    return width, height, bytes(out)


def resample(src, sw, sh, dw, dh):
    """Area-averages RGBA down to dw x dh, weighting colour by alpha to avoid halos."""
    dst = bytearray(dw * dh * 4)
    for dy in range(dh):
        y0, y1 = dy * sh // dh, max(dy * sh // dh + 1, (dy + 1) * sh // dh)
        for dx in range(dw):
            x0, x1 = dx * sw // dw, max(dx * sw // dw + 1, (dx + 1) * sw // dw)
            r = g = b = a = 0
            n = 0
            for sy in range(y0, y1):
                row = sy * sw
                for sx in range(x0, x1):
                    p = (row + sx) * 4
                    pa = src[p + 3]
                    r += src[p] * pa
                    g += src[p + 1] * pa
                    b += src[p + 2] * pa
                    a += pa
                    n += 1
            o = (dy * dw + dx) * 4
            if a:
                dst[o] = min(255, r // a)
                dst[o + 1] = min(255, g // a)
                dst[o + 2] = min(255, b // a)
            dst[o + 3] = a // n
    return bytes(dst)


def ico_image(rgba, size):
    """One 32-bit BMP icon image: header, bottom-up BGRA, then an empty AND mask."""
    xor = bytearray()
    for y in range(size - 1, -1, -1):
        for x in range(size):
            p = (y * size + x) * 4
            xor += bytes((rgba[p + 2], rgba[p + 1], rgba[p], rgba[p + 3]))
    mask_stride = ((size + 31) // 32) * 4
    mask = bytes(mask_stride * size)
    header = struct.pack("<IiiHHIIiiII", 40, size, size * 2, 1, 32, 0, len(xor) + len(mask), 0, 0, 0, 0)
    return header + bytes(xor) + mask


def write_ico(images, path):
    header = struct.pack("<HHH", 0, 1, len(images))
    offset = len(header) + 16 * len(images)
    entries, bodies = b"", b""
    for size, body in images:
        entries += struct.pack("<BBBBHHII", size & 0xFF, size & 0xFF, 0, 0, 1, 32, len(body), offset)
        offset += len(body)
        bodies += body
    open(path, "wb").write(header + entries + bodies)


def main():
    w, h, rgba = png_decode(MASTER)
    if w != h:
        raise ValueError(f"{MASTER}: expected square art, got {w}x{h}")
    print(f"master {w}x{h}")

    scaled = {}

    def at(size):
        if size not in scaled:
            scaled[size] = rgba if size == w else resample(rgba, w, h, size, size)
        return scaled[size]

    os.makedirs(OUT, exist_ok=True)

    ico = os.path.join(OUT, f"{NAME}.ico")
    write_ico([(s, ico_image(at(s), s)) for s in ICO_SIZES if s <= w], ico)
    print(f"wrote {ico}")


if __name__ == "__main__":
    sys.exit(main())
