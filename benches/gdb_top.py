#!/usr/bin/env python3
"""Aggregate the backtraces dumped by `gdb_sample.sh`: self time (leaf
frame) and inclusive time per function, over the busy threads only.

    python3 benches/gdb_top.py samples.txt [--top N] [--inclusive] [--from S --to S]
                                           [--thread REGEX] [--idle]

A thread is idle when its leaf frames are a wait (futex, condvar, park,
epoll, nanosleep, read on a pipe): those samples are counted separately.
"""
import argparse
import re
from collections import Counter

FRAME = re.compile(r"^#(\d+)\s+(?:0x[0-9a-f]+ in )?(.+?)(?: \(\))?(?: at [^ ]+:\d+| from [^ ]+)?$")
THREAD = re.compile(r'^Thread (\d+) \(Thread 0x[0-9a-f]+ \(LWP (\d+)\)(?: "([^"]*)")?\):')
SAMPLE = re.compile(r"^=== sample")
STAMP = re.compile(r"^T=([0-9.]+)")
IDLE = re.compile(r"futex|condvar|Condvar|park|epoll|nanosleep|clock_nanosleep|__GI___poll|__libc_read|pthread_cond|syscall|wait4|sched_yield")


def clean(name):
    # Drop the hash suffix of Rust symbols and generic noise.
    name = re.sub(r"::h[0-9a-f]{16}$", "", name)
    name = re.sub(r"::\{\{closure\}\}", "::{closure}", name)
    return name


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("dump")
    ap.add_argument("--top", type=int, default=40)
    ap.add_argument("--inclusive", action="store_true")
    ap.add_argument("--from", dest="t_from", type=float, default=None)
    ap.add_argument("--to", dest="t_to", type=float, default=None)
    ap.add_argument("--thread", default=None)
    ap.add_argument("--idle", action="store_true", help="count idle threads too")
    ap.add_argument("--threads", action="store_true", help="busy samples per thread name")
    args = ap.parse_args()
    rx = re.compile(args.thread) if args.thread else None

    self_t, incl_t, per_thread = Counter(), Counter(), Counter()
    busy = idle = 0
    t = None
    t0 = None
    keep = True
    frames = []
    tname = None

    def flush():
        nonlocal frames, busy, idle, tname
        if frames and keep and (rx is None or rx.search(tname or "")):
            leaf = frames[0]
            if IDLE.search(" ".join(frames[:3])) and not args.idle:
                idle += 1
            else:
                busy += 1
                per_thread[tname or "?"] += 1
                self_t[leaf] += 1
                for f in set(frames):
                    incl_t[f] += 1
        frames = []

    with open(args.dump, encoding="utf-8", errors="replace") as fh:
        for line in fh:
            line = line.rstrip("\n")
            if SAMPLE.match(line):
                flush()
                continue
            m = STAMP.match(line)
            if m:
                if t0 is None:
                    t0 = float(m.group(1))
                t = float(m.group(1)) - t0
                keep = (args.t_from is None or t >= args.t_from) and (args.t_to is None or t <= args.t_to)
                continue
            m = THREAD.match(line)
            if m:
                flush()
                tname = m.group(3) or ("lwp" + m.group(2))
                continue
            m = FRAME.match(line)
            if m:
                frames.append(clean(m.group(2)))
    flush()

    if args.threads:
        for name, n in per_thread.most_common():
            print(f"{n:>7}  {name}")
        print(f"{busy:>7}  busy thread-samples, {idle} idle")
        return
    table = incl_t if args.inclusive else self_t
    print(f"{busy} busy thread-samples ({idle} idle), {'inclusive' if args.inclusive else 'self'} time")
    for name, n in table.most_common(args.top):
        print(f"{n:>7} {100.0 * n / max(busy, 1):5.1f}%  {name[:160]}")


if __name__ == "__main__":
    main()
