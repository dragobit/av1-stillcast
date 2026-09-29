#!/usr/bin/env python3
"""VP9 show_existing_frame PoC: craft repeat-frame packets into an IVF
and check decodability with ffmpeg/libvpx."""
import struct, subprocess, sys

import os, subprocess as sp
IVF = "/tmp/vp9_2f.ivf"
if not os.path.exists(IVF):
    sp.run(["ffmpeg","-y","-f","lavfi","-i",
        "color=c=0x203040:size=640x360:rate=30,drawtext=text='vp9 still':fontsize=48:fontcolor=yellow:x=200:y=170",
        "-frames:v","2","-c:v","libvpx-vp9","-b:v","300k","-g","30",IVF],check=True)
data = open(IVF, "rb").read()
assert data[:4] == b"DKIF"
# header 32 bytes; packets: 4B size + 8B ts + payload
pos = 32
pkts = []
while pos + 12 <= len(data):
    sz, = struct.unpack("<I", data[pos:pos+4])
    ts, = struct.unpack("<Q", data[pos+4:pos+12])
    pkts.append((ts, data[pos+12:pos+12+sz]))
    pos += 12 + sz
print("packets:", [(t, len(p), hex(p[0])) for t, p in pkts])
# frame0 byte0: expect frame_marker=2, show_existing=0, frame_type=0, show_frame=1
b0 = pkts[0][1][0]
print("kf byte0:", bin(b0), "marker", b0>>6, "se", (b0>>3)&1, "ft", (b0>>2)&1, "sf", (b0>>1)&1)
b1 = pkts[1][1][0]
print("inter byte0:", bin(b1), "marker", b1>>6, "se", (b1>>3)&1, "ft", (b1>>2)&1, "sf", (b1>>1)&1)

# inter frame header parse (profile0, no se): after byte0 sync code etc.
# byte0 = marker(2) prof(2) se(1) ft(1) sf(1) er(1)
# then frame_context? refresh flags at bit offset... for inter (non-er):
# intra_only? no. reset_frame_context(2) if !er... then refresh_frame_context(1),
# frame_parallel_decoding(1), frame_context_idx(2), loopfilter... wait order:
# uncompressed_header: ft, sf, er -> if kf: sync; else: intra_only(1) if !sf;
# reset_frame_context(2) if !er; if intra_only: sync+refresh_flags+size+cs;
# else: refresh_frame_flags f(8), frame_size, ref select(3x3)...
f = pkts[1][1]
er = f[0] & 1
print("inter er:", er, "byte1:", bin(f[1]), "byte2:", bin(f[2]), "byte3:", bin(f[3]), "byte4:", bin(f[4]))

def write_ivf(path, packets):
    hdr = bytearray(data[:32])
    struct.pack_into("<I", hdr, 24, len(packets))
    out = bytes(hdr)
    for ts, p in packets:
        out += struct.pack("<IQ", len(p), ts) + p
    open(path, "wb").write(out)

def probe(payload, name):
    pk = [pkts[0], pkts[1]] + [(i+2, payload) for i in range(3)]
    write_ivf(f"/tmp/vp9_se_{name}.ivf", pk)
    r = subprocess.run(
        ["ffmpeg", "-v", "error", "-c:v", "libvpx-vp9", "-i",
         f"/tmp/vp9_se_{name}.ivf", "-f", "framemd5", "-"],
        capture_output=True, text=True)
    n = sum(1 for l in r.stdout.splitlines() if l.strip())
    print(f"{name}: payload={payload.hex()} decoded_frames={n} err={r.stderr.strip()[:200]}")

# candidate payloads for show_existing pointing at slot 0 (KF refreshed all 8)
probe(bytes([0x88]), "1B")
probe(bytes([0x88, 0x00]), "2B_z")
probe(bytes([0x88, 0x00, 0x00]), "3B_z")
probe(bytes([0x89]), "1B_s1")   # slot 1
probe(bytes([0x8f]), "1B_s7")   # slot 7
# with header_size_in_bytes=2? try {0x88, sz_hi, sz_lo}
probe(bytes([0x88, 0x00, 0x02]), "hsz2")
probe(bytes([0x88, 0x00, 0x01]), "hsz1")
