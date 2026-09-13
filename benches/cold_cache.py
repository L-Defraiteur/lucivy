"""Cold cache without root: evict an index's pages with posix_fadvise(DONTNEED),
then time the panel. Warm run first, cold run second, on the same process-free
state (each harness run opens the index itself)."""
import os, pathlib, subprocess, sys, time

def evict(root):
    n = bytes_ = 0
    for p in pathlib.Path(root).rglob("*"):
        if not p.is_file():
            continue
        try:
            fd = os.open(p, os.O_RDONLY)
        except OSError:
            continue
        try:
            size = os.fstat(fd).st_size
            os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)
            n += 1
            bytes_ += size
        finally:
            os.close(fd)
    return n, bytes_

HOME = os.path.expanduser("~")
BIN = subprocess.run(
    "ls -t /home/lucied/git_workspaces/lucivy/target/release/deps/test_sfx_v3_ground_truth-* | grep -v '\\.d$' | head -1",
    shell=True, capture_output=True, text=True).stdout.strip()
W = f"{HOME}/lucivy_bench/compare-4.1"
QUERIES = "mutex_lock:strict,sched:strict,schdule:fz1,spin_lock_[a-z]+:rx"

for layout, extra in (("dict", {}), ("dict-nopos", {"V3_POSITIONS": "0"})):
    for state in ("warm", "cold"):
        if state == "cold":
            n, b = evict(f"{W}/{layout}")
            print(f"-- {layout}: {n} fichiers évincés du cache ({b/2**30:.1f} Gio)", flush=True)
        env = {**os.environ, "V3_CORPUS": f"{HOME}/lucivy_bench/linux-7.2", "V3_MAX_DOCS": "1000000",
               "V3_COMMIT_EVERY": "10000", "V3_SFX_VERSION": "4", "V3_INDEX_DIR": f"{W}/{layout}",
               "LUCIVY_HIGHLIGHT_SPAN_CAP": "0", "V3_QUERIES": QUERIES, **extra}
        r = subprocess.run([BIN, "v3_ground_truth_demo", "--ignored", "--nocapture"],
                           env=env, capture_output=True, text=True)
        out = r.stdout + r.stderr
        keep = [l for l in out.splitlines() if "ms search," in l]
        if not keep:
            print(f"{layout:11} {state:5} | AUCUNE LIGNE (exit {r.returncode}) :: "
                  f"{out.strip().splitlines()[-1][:120] if out.strip() else 'sortie vide'}", flush=True)
        for line in keep:
            print(f"{layout:11} {state:5} | {line.strip()}", flush=True)
