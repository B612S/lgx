#!/usr/bin/env python3
"""Feed an HEVC Annex-B file to lgx-client over the lgx wire protocol.

Development helper: it exercises the client decoder/renderer without a guest.

    ffmpeg -f lavfi -i testsrc2=size=1920x1080:rate=160 -t 2 \
        -c:v libx265 -x265-params aud=1 -pix_fmt yuv420p -f hevc /tmp/test.h265
    ./scripts/fake-server.py /tmp/test.h265 --port 5999 --fps 160 &
    lgx-client --connect 127.0.0.1:5999 --decoder software
"""
import argparse
import re
import socket
import struct
import sys
import time

AUTH = 1
HELLO = 2
VIDEO = 3
HEVC = 1
AUD = re.compile(b"\x00\x00\x00\x01\x46|\x00\x00\x01\x46")  # HEVC AUD (NAL 35)


def recv_exact(sock, n):
    out = b""
    while len(out) < n:
        chunk = sock.recv(n - len(out))
        if not chunk:
            raise ConnectionError("client closed the connection")
        out += chunk
    return out


def access_units(data):
    cuts = [m.start() for m in AUD.finditer(data)]
    if cuts:
        return [data[cuts[i]:(cuts[i + 1] if i + 1 < len(cuts) else len(data))]
                for i in range(len(cuts))]
    starts = [m.start() for m in re.finditer(b"\x00\x00\x00\x01|\x00\x00\x01", data)]
    packets, cur = [], b""
    for i, start in enumerate(starts):
        end = starts[i + 1] if i + 1 < len(starts) else len(data)
        nal = data[start:end]
        off = 4 if nal.startswith(b"\x00\x00\x00\x01") else 3
        header = nal[off] if off < len(nal) else 0
        typ = (header >> 1) & 0x3F
        if typ <= 31 and cur:  # VCL NAL starts a new access unit
            packets.append(cur)
            cur = b""
        cur += nal
    if cur:
        packets.append(cur)
    return packets


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("file")
    ap.add_argument("--bind", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=5999)
    ap.add_argument("--width", type=int, default=1920)
    ap.add_argument("--height", type=int, default=1080)
    ap.add_argument("--fps", type=float, default=160.0)
    ap.add_argument("--repeat", type=int, default=1)
    ap.add_argument("--token", default=None)
    args = ap.parse_args()

    packets = access_units(open(args.file, "rb").read())
    if not packets:
        sys.exit("no HEVC access units found")
    print(f"{len(packets)} access units", flush=True)

    listener = socket.socket()
    listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    listener.bind((args.bind, args.port))
    listener.listen(1)
    print(f"listening on {args.bind}:{args.port}", flush=True)
    conn, peer = listener.accept()
    conn.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    print(f"client {peer}", flush=True)
    try:
        typ, flags, code, a, b = struct.unpack("<BBHii", recv_exact(conn, 12))
        token = recv_exact(conn, a).decode(errors="replace") if typ == AUTH else ""
        if args.token is not None and token != args.token:
            print("authentication failed", flush=True)
            return 1
        conn.sendall(
            struct.pack("<BBHii", HELLO, 0, HEVC, args.width, args.height)
        )
        period = 1.0 / max(args.fps, 1.0)
        start = time.monotonic()
        sent = 0
        for _ in range(args.repeat):
            for packet in packets:
                conn.sendall(struct.pack("<BBHii", VIDEO, 0, 0, len(packet), 0))
                conn.sendall(packet)
                sent += 1
                time.sleep(period)
        elapsed = time.monotonic() - start
        print(f"sent {sent} packets in {elapsed:.2f}s ({sent / elapsed:.1f}/s)", flush=True)
        time.sleep(0.3)
    except (BrokenPipeError, ConnectionError):
        pass
    finally:
        conn.close()
        listener.close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
