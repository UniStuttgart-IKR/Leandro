#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
"""Pull one framebuffer over VNC/RFB, write a PNG, and count what is not black.

    probe/python/rfbshot.py <host> <port> <out.png>

Why this and not a VNC client: the measurement wanted here is "did the
picture arrive", and an open port does not answer that. This connects,
requests the whole framebuffer once, decodes raw rectangles and prints the
non-black pixel count -- the same currency the rest of this project uses for
"is there an image". It needs no VNC client and no PIL: the PNG is assembled
from zlib and struct.

Measured with it on 2026-08-07: X core drawing arrives in full (xsetroot
fills 1593640 of 2073600 pixels) while a GLX window on the same screen stays
black -- which is how capture was ruled out as the cause.
"""
import socket, struct, sys, zlib

host = sys.argv[1]; port = int(sys.argv[2]); out = sys.argv[3]
s = socket.create_connection((host, port), timeout=20); s.settimeout(60)

def rd(n):
    b = b""
    while len(b) < n:
        c = s.recv(n - len(b))
        if not c:
            raise SystemExit(f"connection closed after {len(b)}/{n} bytes")
        b += c
    return b

print("server:", rd(12).decode().strip())
s.sendall(b"RFB 003.008\n")
sec = rd(rd(1)[0])
if 1 not in sec:
    raise SystemExit(f"no None security, offered {list(sec)}")
s.sendall(bytes([1]))
if struct.unpack(">I", rd(4))[0]:
    raise SystemExit("security handshake failed")
s.sendall(bytes([1]))                       # shared session

w, h = struct.unpack(">HH", rd(4))
pf = rd(16)
bpp, depth, big_endian, true_colour = pf[0], pf[1], pf[2], pf[3]
rmax, gmax, bmax = struct.unpack(">HHH", pf[4:10])
rsh, gsh, bsh = pf[10], pf[11], pf[12]
name = rd(struct.unpack(">I", rd(4))[0]).decode(errors="replace")
print(f"desktop: {name!r}  {w}x{h}  bpp={bpp}")
if bpp != 32 or not true_colour:
    raise SystemExit(f"only 32bpp truecolour handled here, got bpp={bpp}")

s.sendall(struct.pack(">BBHi", 2, 0, 1, 0))           # SetEncodings: raw
s.sendall(struct.pack(">BBHHHH", 3, 0, 0, 0, w, h))   # full, non-incremental

if rd(1)[0] != 0:
    raise SystemExit("unexpected message type")
rd(1)
rows = [bytearray(w * 3) for _ in range(h)]
order = ">I" if big_endian else "<I"
for _ in range(struct.unpack(">H", rd(2))[0]):
    x, y, rw, rh, enc = struct.unpack(">HHHHi", rd(12))
    if enc != 0:
        raise SystemExit(f"encoding {enc} is not raw")
    data = rd(rw * rh * 4)
    for j in range(rh):
        row = rows[y + j]
        base = j * rw * 4
        for i in range(rw):
            (v,) = struct.unpack_from(order, data, base + i * 4)
            p = (x + i) * 3
            row[p]     = (v >> rsh) & rmax
            row[p + 1] = (v >> gsh) & gmax
            row[p + 2] = (v >> bsh) & bmax

raw = b"".join(b"\x00" + bytes(r) for r in rows)
def chunk(tag, body):
    return (struct.pack(">I", len(body)) + tag + body
            + struct.pack(">I", zlib.crc32(tag + body) & 0xffffffff))
png = (b"\x89PNG\r\n\x1a\n"
       + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0))
       + chunk(b"IDAT", zlib.compress(raw, 6))
       + chunk(b"IEND", b""))
open(out, "wb").write(png)
nz = sum(1 for r in rows for i in range(0, len(r), 3) if r[i] or r[i+1] or r[i+2])
print(f"wrote {out}  ({len(png)} bytes)   non-black: {nz} of {w*h}")
