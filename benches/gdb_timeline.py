#!/usr/bin/env python3
"""The pool over time, from the backtraces dumped by `gdb_sample.sh`: how
many scheduler threads are busy at each sample, what the busy ones do when
the pool is nearly idle (the serial path), and where the main thread waits.

    python3 benches/gdb_timeline.py samples.txt [--main NAME] [--window S] [--valley N] [--to S]

`--main` is the thread name of the caller (the harness: `v3_ground_truth`),
`--valley` the busy count at or under which a sample is a valley (default
2), `--window` the width of the coarse histogram (default 2.5 s).

What it found on 13 September 2026 (whole kernel, 16 writer threads): the
pool alternates between 16-24 busy threads and valleys of one busy thread,
2-2.5 s at every commit — the serial path of the commit (the pending
texts' `retain` and `String` drops, 6.7 s over the run), which the epoch
table removed (48.2 → 39.9 s).
"""
import argparse
import re
from collections import Counter, defaultdict

FRAME = re.compile(r"^#(\d+)\s+(?:0x[0-9a-f]+ in )?(.+?)(?: \(\))?(?: at [^ ]+:\d+| from [^ ]+)?$")
THREAD = re.compile(r'^Thread (\d+) \(Thread 0x[0-9a-f]+ \(LWP (\d+)\)(?: "([^"]*)")?\):')
IDLE = re.compile(r"futex|condvar|Condvar|park|epoll|nanosleep|clock_nanosleep|__GI___poll|__libc_read|pthread_cond|syscall|wait4|sched_yield")
NOISE = re.compile(r"^(core::|std::|alloc::|__|\?\?|<|free$|realloc$|malloc$|dealloc|drop)")


def clean(name):
    name = re.sub(r"::h[0-9a-f]{16}$", "", name)
    return re.sub(r"<.*", "", name)


def parse(path):
    samples = []
    cur = None
    thread = None
    for line in open(path, errors="replace"):
        line = line.rstrip("\n")
        if line.startswith("=== sample"):
            cur = {}
            samples.append([None, cur])
            thread = None
            continue
        m = re.match(r"^T=([0-9.]+)", line)
        if m and samples:
            samples[-1][0] = float(m.group(1))
            continue
        m = THREAD.match(line)
        if m and cur is not None:
            thread = (m.group(3) or f"lwp{m.group(2)}")
            cur.setdefault(thread, [])
            continue
        m = FRAME.match(line)
        if m and thread is not None and cur is not None:
            cur[thread].append(clean(m.group(2)))
    return [s for s in samples if s[0] is not None]


def busy_threads(threads):
    return [(name, fr) for name, fr in threads.items()
            if name.startswith("scheduler") and not IDLE.search(" ".join(fr[:3]))]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("dump")
    ap.add_argument("--main", default="v3_ground_truth")
    ap.add_argument("--window", type=float, default=2.5)
    ap.add_argument("--valley", type=int, default=2)
    ap.add_argument("--to", type=float, default=None, help="ignore samples after this many seconds")
    a = ap.parse_args()
    samples = parse(a.dump)
    if not samples:
        print("no samples")
        return
    t0 = samples[0][0]
    if a.to is not None:
        samples = [s for s in samples if s[0] - t0 <= a.to]
    print(f"{len(samples)} samples over {samples[-1][0] - t0:.1f} s")

    counts = [len(busy_threads(thr)) for _, thr in samples]
    print("\nbusy scheduler threads per sample:")
    for i in range(0, len(counts), 40):
        print(f"{samples[i][0] - t0:6.1f}s " + " ".join(f"{b:2d}" for b in counts[i:i + 40]))
    print(f"\nmean busy {sum(counts) / len(counts):.1f}; valleys (<= {a.valley} busy): {sum(1 for b in counts if b <= a.valley)} of {len(counts)}")

    win = defaultdict(lambda: [0, 0, Counter()])
    for t, thr in samples:
        w = int((t - t0) // a.window)
        win[w][0] += 1
        win[w][1] += len(busy_threads(thr))
        mf = thr.get(a.main, [])
        key = next((f for f in mf if not NOISE.search(f)), mf[0] if mf else "?")
        win[w][2][key] += 1
    print(f"\nwindow ({a.window} s)   samples  busy/sample  main thread")
    for w in sorted(win):
        n, b, c = win[w]
        print(f"{w * a.window:6.1f}-{(w + 1) * a.window:6.1f}  {n:4d}  {b / n:5.1f}  {c.most_common(1)[0][0][:70]}")

    stacks = Counter()
    for t, thr in samples:
        busy = busy_threads(thr)
        if len(busy) <= a.valley:
            for name, fr in busy:
                sig = " > ".join(f for f in fr[:16] if not NOISE.search(f))
                stacks[sig] += 1
    print(f"\nwhat the busy threads do in the valleys (<= {a.valley} busy):")
    for s, c in stacks.most_common(12):
        print(f"{c:4d}  {s[:220]}")


if __name__ == "__main__":
    main()
