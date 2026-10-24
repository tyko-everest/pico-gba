#!/usr/bin/env python3
"""Sample the PC of a GDB remote server (e.g. an emulator running Emerald) as
fast as possible: continue, interrupt, read registers, repeat.

Usage: gdb_trace.py [--host H] [--port 2345] [--out samples.log] [--max N]
                    [--delay SECONDS] [--regs]
Ctrl-C stops sampling and detaches (the target is left running).
"""
import argparse
import signal
import socket
import struct
import sys
import time

def pkt(body: str) -> bytes:
    b = body.encode()
    return b"$" + b + b"#%02x" % (sum(b) & 0xFF)


class Gdb:
    def __init__(self, host, port):
        self.s = socket.create_connection((host, port), timeout=3)
        self.s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        self.buf = b""
        self.noack = False

    def _byte(self):
        while not self.buf:
            d = self.s.recv(65536)
            if not d:
                raise ConnectionError("server closed connection")
            self.buf = d
        b, self.buf = self.buf[:1], self.buf[1:]
        return b

    def send(self, data: str):
        body = data.encode()
        pkt = b"$" + body + b"#%02x" % (sum(body) & 0xFF)
        self.s.sendall(pkt)
        if not self.noack:
            while True:
                c = self._byte()
                if c == b"+":
                    break
                if c == b"-":
                    self.s.sendall(pkt)

    def recv(self) -> str:
        while self._byte() != b"$":
            pass
        body = b""
        while True:
            c = self._byte()
            if c == b"#":
                break
            body += c
        self._byte(), self._byte()  # checksum
        if not self.noack:
            self.s.sendall(b"+")
        return body.decode(errors="replace")

    def cmd(self, data: str) -> str:
        self.send(data)
        return self.recv()


def le32(hexstr):
    return struct.unpack("<I", bytes.fromhex(hexstr))[0]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=2345)
    ap.add_argument("--out", default="samples.log")
    ap.add_argument("--max", type=int, default=0, help="stop after N samples (0 = unlimited)")
    ap.add_argument("--delay", type=float, default=0.0,
                    help="seconds to let the target run between samples")
    ap.add_argument("--step", action="store_true",
                    help="single-step every instruction (pipelined) instead of sampling")
    ap.add_argument("--batch", type=int, default=8, help="steps in flight per batch (--step)")
    ap.add_argument("--regs", action="store_true", help="also log r0-r15 and cpsr")
    args = ap.parse_args()

    signal.signal(signal.SIGTERM, signal.default_int_handler)  # clean detach on `timeout`
    g = Gdb(args.host, args.port)
    if g.cmd("QStartNoAckMode") == "OK":
        g.noack = True

    n = 0
    t0 = time.perf_counter()
    running = False
    with open(args.out, "w", buffering=1 << 16) as out:
        try:
            g.cmd("?")
            while args.step and (not args.max or n < args.max):
                k = min(args.batch, args.max - n) if args.max else args.batch
                # pipeline k x (step, read regs); the stub answers them in order
                g.s.sendall(b"".join(pkt(c) for _ in range(k) for c in ("s", "g")))
                got = 0
                while got < k:
                    try:
                        regs = g.recv()
                    except TimeoutError:
                        print(f"stub dropped replies at step {n} (batch {k}); try a smaller --batch",
                              file=sys.stderr)
                        return
                    if len(regs) < 17 * 8:  # stop reply (duplicates happen); skip
                        if regs[:3] not in ("S05", "S02", "S00"):
                            print("unexpected reply:", regs, file=sys.stderr)
                            return
                        continue
                    got += 1
                    pc = le32(regs[15 * 8:16 * 8])
                    cpsr = le32(regs[16 * 8:17 * 8])
                    line = f"{n:010d} {pc:08x} {'T' if cpsr & 0x20 else 'A'}"
                    if args.regs:
                        line += " " + " ".join(regs[i * 8:i * 8 + 8] for i in range(17))
                    out.write(line + "\n")
                    n += 1
            while not args.step and (not args.max or n < args.max):
                g.send("c")
                running = True
                if args.delay:
                    time.sleep(args.delay)
                g.s.sendall(b"\x03")  # interrupt
                g.recv()  # stop reply
                running = False
                regs = g.cmd("g")
                pc = le32(regs[15 * 8:16 * 8])
                cpsr = le32(regs[16 * 8:17 * 8])
                line = f"{time.perf_counter() - t0:10.6f} {pc:08x} {'T' if cpsr & 0x20 else 'A'}"
                if args.regs:
                    line += " " + " ".join(regs[i * 8:i * 8 + 8] for i in range(17))
                out.write(line + "\n")
                n += 1
        except KeyboardInterrupt:
            pass
        finally:
            dt = time.perf_counter() - t0
            print(f"{n} samples in {dt:.2f}s ({n / max(dt, 1e-9):.0f}/s) -> {args.out}", file=sys.stderr)
            try:
                if running:
                    g.s.sendall(b"\x03")
                    g.recv()
                g.send("D")  # detach: target resumes
            except OSError:
                pass


if __name__ == "__main__":
    main()
