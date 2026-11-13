#!/usr/bin/env python3
"""Benchmark a mounted pkg: sequential, random 4 KiB, game-like 64 KiB and 4 concurrent streams.
Usage: bench.py MOUNTPOINT"""
import os, random, sys, threading, time

mnt = sys.argv[1]
big = os.path.join(mnt, "data/big0.bin")
size = os.path.getsize(big)
random.seed(42)


def sequential(path, block=128 * 1024):
    fd = os.open(path, os.O_RDONLY | os.O_DIRECT)
    buf = bytearray(block)
    n = os.path.getsize(path)
    t = time.perf_counter()
    got = 0
    while got < n:
        want = min(block, n - got)
        got += os.preadv(fd, [memoryview(buf)[:want]], got)
    dt = time.perf_counter() - t
    os.close(fd)
    return n / 1e6 / dt


def random_4k(count=300):
    fd = os.open(big, os.O_RDONLY | os.O_DIRECT)
    buf = bytearray(4096)
    lat = []
    for _ in range(count):
        off = random.randrange(0, size - 4096) & ~4095
        t = time.perf_counter()
        os.preadv(fd, [memoryview(buf)], off)
        lat.append((time.perf_counter() - t) * 1e3)
    os.close(fd)
    lat.sort()
    return lat[len(lat) // 2], lat[-1]


def game_like(count=200):
    """64 KiB reads at pseudo-random places in several files, as a game streams assets."""
    files = []
    for d in sorted(os.listdir(os.path.join(mnt, "data"))):
        sub = os.path.join(mnt, "data", d)
        if os.path.isdir(sub):
            files.extend(os.path.join(sub, f) for f in sorted(os.listdir(sub))[:2])
    files = files[:8]
    fds = [os.open(p, os.O_RDONLY | os.O_DIRECT) for p in files]
    lat = []
    buf = bytearray(64 * 1024)
    for _ in range(count):
        k = random.randrange(len(fds))
        n = os.fstat(fds[k]).st_size
        off = random.randrange(0, max(1, n - 65536)) & ~4095
        t = time.perf_counter()
        os.preadv(fds[k], [memoryview(buf)], off)
        lat.append((time.perf_counter() - t) * 1e3)
    for fd in fds:
        os.close(fd)
    lat.sort()
    return sum(lat) / len(lat), lat[-1]


def concurrent(block=128 * 1024):
    out = []

    def run(p):
        out.append(sequential(p))

    files = [os.path.join(mnt, f"data/big{i}.bin") for i in range(4)]
    threads = [threading.Thread(target=run, args=(p,)) for p in files]
    t = time.perf_counter()
    for th in threads:
        th.start()
    for th in threads:
        th.join()
    return sum(out) / (time.perf_counter() - t)


seq = sequential(big)
p50, pmax = random_4k()
gavg, gmax = game_like()
conc = concurrent()
print(f"{seq:7.1f} MB/s seq | 4K random p50 {p50:6.2f} ms max {pmax:6.2f} | "
      f"64K game avg {gavg:6.2f} ms max {gmax:6.2f} | 4 streams {conc:6.1f} MB/s aggregate")
