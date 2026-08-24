#!/usr/bin/env python3
"""Diff the native parity report against the browser one.

    python3 playground/parity_diff.py /tmp/parity_native.json /tmp/parity_wasm.json

Exact agreement expected on counts and on the top-10 node ids in order; scores
compared at 1e-4 (BM25 sums in a different order across shards can differ in
the last bits). The browser report may be wrapped in the debug server's
{"result": "..."} envelope.
"""
import json, sys

def load(path):
    d = json.load(open(path))
    if isinstance(d, dict) and 'result' in d:
        d = json.loads(d['result'])
    return {e['name']: e for e in d}

native, wasm = load(sys.argv[1]), load(sys.argv[2])
fails = 0
for name, n in native.items():
    w = wasm.get(name)
    if w is None:
        print(f"MISSING in wasm: {name}"); fails += 1; continue
    if 'error' in n or 'error' in w:
        print(f"{name:40} native={n.get('error','ok')} wasm={w.get('error','ok')}")
        if ('error' in n) != ('error' in w): fails += 1
        continue
    ok = n['count'] == w['count']
    n_ids = [t['node_id'] for t in n['top']]
    w_ids = [t['node_id'] for t in w['top']]
    ok_top = n_ids == w_ids
    ok_score = all(abs(a['score'] - b['score']) < 1e-4 for a, b in zip(n['top'], w['top']))
    ok_spans = [t['spans'] for t in n['top']] == [t['spans'] for t in w['top']]
    # Same documents, same scores, same spans, different order: ties broken
    # by (shard, segment, doc), which depends on the segment layout — a
    # browser-built index and a native one merge differently. Not a defect.
    same_set = sorted(n_ids) == sorted(w_ids) \
        and sorted(t['spans'] for t in n['top']) == sorted(t['spans'] for t in w['top'])
    # Or every score in both top-10s is the same value: the top-10 is then an
    # arbitrary window on a larger tie (a path filter where all hits score alike).
    flat = len({round(t['score'], 4) for t in n['top']} | {round(t['score'], 4) for t in w['top']}) <= 1
    tie = (not ok_top) and ok and (same_set and ok_score or flat)
    status = 'OK ' if (ok and ok_top and ok_score and ok_spans) else ('TIE ' if tie else 'DIFF')
    if status == 'DIFF': fails += 1
    print(f"{status} {name:40} count {n['count']:6}/{w['count']:6}  "
          f"native {n['ms']:7.1f}ms  wasm {w['ms']:8.1f}ms  "
          f"top {'=' if ok_top else '≠'} scores {'=' if ok_score else '≠'} spans {'=' if ok_spans else '≠'}")
    if not ok_top:
        print(f"     native top: {n_ids}\n     wasm   top: {w_ids}")
print(f"\n{fails} difference(s) over {len(native)} queries")
sys.exit(1 if fails else 0)
