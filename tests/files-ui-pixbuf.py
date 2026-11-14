"""Write a real PNG and report whether this machine can decode one.

Used by the ui smoke script to decide whether the picture-preview check can
assert anything. `GdkPixbuf` delegates every format but XPM to loaders that run
out of process inside a sandbox, and a container that cannot create a pty cannot
start one — so the answer depends on the host and has to be asked rather than
assumed. Prints `ok` when a decode works and `no` otherwise; the exit status is
always zero, because a machine with no image loader is not a broken machine.
"""

import struct
import sys
import zlib


def chunk(kind, data):
    return (struct.pack(">I", len(data)) + kind + data
            + struct.pack(">I", zlib.crc32(kind + data) & 0xFFFFFFFF))


def png(width, height):
    """An RGB image with no two pixels alike, big enough to fill an icon cell."""
    rows = (bytes([0] + [v for x in range(width)
                         for v in (x * 4 % 256, y * 4 % 256, 128)])
            for y in range(height))
    body = b"".join(rows)
    return (b"\x89PNG\r\n\x1a\n"
            + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(body))
            + chunk(b"IEND", b""))


out = sys.argv[1]
with open(out, "wb") as handle:
    handle.write(png(64, 64))

try:
    import gi

    gi.require_version("GdkPixbuf", "2.0")
    from gi.repository import GdkPixbuf

    image = GdkPixbuf.Pixbuf.new_from_file_at_scale(out, 128, 128, True)
    print("ok" if image.get_width() >= 16 else "no")
except Exception:  # noqa: BLE001 - any failure means the same answer here
    print("no")
