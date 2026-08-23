#!/usr/bin/env python3
"""Live smoke test of the Python binding: query_warnings + search + highlights.

Build and run:
    cd bindings/python && PYO3_USE_ABI3_FORWARD_COMPATIBILITY=1 cargo build --release
    cp ../../target/release/liblucivy.so /tmp/lucivy_py/lucivy.so
    python3 tests/smoke_warnings.py /tmp/lucivy_py
"""
import sys; sys.path.insert(0, sys.argv[1] if len(sys.argv) > 1 else ".")
import lucivy, json, tempfile, os
d = tempfile.mkdtemp()
idx = lucivy.Index.create(os.path.join(d, "ix"), [{"name": "body", "type": "text", "stored": True}])
idx.add(1, body="the kmalloc call and spin_lock_init here")
idx.commit()
cases = [
    ({"type": "contains", "field": "body", "value": "kmalloc"}, 0),
    ({"type": "contains", "field": "body", "value": "__init"}, 1),
    ({"type": "regex", "field": "body", "value": "[0-9]{8}"}, 1),
    ({"type": "fuzzy", "field": "body", "value": "init"}, 1),
]
fails = 0
for q, expect in cases:
    w = idx.query_warnings(q)
    print(json.dumps(q["value"]), "->", w)
    if len(w) != expect:
        print("  EXPECTED", expect, "warnings"); fails += 1
r = idx.search({"type": "contains", "field": "body", "value": "spin_lock"}, highlights=True)
print("search spin_lock:", len(r), "hit(s), highlights:", r[0].highlights if r else None)
assert r, "search must find the doc"
print("FAILS", fails)
sys.exit(1 if fails else 0)
