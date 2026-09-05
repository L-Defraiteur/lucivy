"""Build the 3.0.8 compatibility fixture with the published wheel:
two small indexes (one shard, two shards) over a deterministic corpus, and
the wheel's own answers to a query panel."""
import json, os, pathlib, shutil, sys
import lucivy

OUT = pathlib.Path(sys.argv[1])
shutil.rmtree(OUT, ignore_errors=True)
OUT.mkdir(parents=True)

def pick(root, pattern, lo, hi, n):
    files = sorted(p for p in pathlib.Path(root).rglob("*") if p.is_file() and pattern(p) and lo < p.stat().st_size < hi)
    out = []
    for p in files:
        try:
            out.append((str(p.relative_to(root)), p.read_text(encoding="utf-8")))
        except UnicodeDecodeError:
            continue
        if len(out) >= n: break
    return out

corpus = pick("/tmp/lucivy-cmp", lambda p: p.suffix in (".rst", ".c", ".h", ".txt"), 2000, 6000, 12)
corpus += pick("/tmp/lucivy-cmp-90k", lambda p: "translations/zh_CN" in str(p) and p.suffix == ".rst", 2000, 12000, 2)
corpus += [
    ("synthetic/fold.txt", "//     'k' -> 'K'  (Kelvin symbol)\nİstanbul kelvin i̇stanbul plain\nDÉJÀ vu and déjà encore\n"),
    ("synthetic/seps.txt", "a________b   c  d\nrag3weaver rag3_weaver rag3-weaver\nstruct mutex lock;\n\tmutex_lock(&dev->lock); return -ENOMEM;\n"),
    ("synthetic/words.txt", "INIT2INIT init __init spin lock init spin_lock_init(x) SPIN_LOCK_INIT\nqueryResult.isSuccess() query_result_is_success\nprintk(\"hello %d\\n\", x); printk_once(y);\n"),
    ("synthetic/long.txt", "可以理" * 30 + "解。\n\n.. toctree::\n" + "免" * 100 + "免。\t\tThe end\n"),
]
print(f"corpus: {len(corpus)} documents, {sum(len(c.encode()) for _, c in corpus)} bytes")

fields = [{"name": "path", "type": "text", "stored": True}, {"name": "content", "type": "text", "stored": True}]
panel = [
    {"label": "strict", "q": {"type": "contains", "field": "content", "value": "mutex_lock", "strict_separators": True}},
    {"label": "relax", "q": {"type": "contains", "field": "content", "value": "mutex lock", "strict_separators": False}},
    {"label": "strict", "q": {"type": "contains", "field": "content", "value": "spin_lock", "strict_separators": True}},
    {"label": "term", "q": {"type": "term", "field": "content", "value": "sched"}},
    {"label": "sw", "q": {"type": "startsWith", "field": "content", "value": "printk"}},
    {"label": "relax", "q": {"type": "contains", "field": "content", "value": "return", "strict_separators": False}},
    {"label": "strict", "q": {"type": "contains", "field": "content", "value": "if (", "strict_separators": True}},
    {"label": "fz1", "q": {"type": "contains", "field": "content", "value": "schdule", "distance": 1, "strict_separators": False}},
    {"label": "fz2", "q": {"type": "contains", "field": "content", "value": "regsiter", "distance": 2, "strict_separators": False}},
    {"label": "rx", "q": {"type": "contains", "field": "content", "value": "spin_lock_[a-z]+", "regex": True, "strict_separators": True}},
    {"label": "relax-case", "q": {"type": "contains", "field": "content", "value": "Mutex", "strict_separators": False}},
    {"label": "relax", "q": {"type": "contains", "field": "content", "value": "i̇stanbul", "strict_separators": False}},
    {"label": "strict", "q": {"type": "contains", "field": "content", "value": "解。", "strict_separators": True}},
    {"label": "phrase", "q": {"type": "phrase", "field": "content", "value": "return -ENOMEM"}},
]

answers = {}
for name, shards in [("single", 1), ("sharded", 2)]:
    path = OUT / name
    idx = lucivy.Index.create(str(path), fields, shards=shards)
    for i, (p, c) in enumerate(corpus):
        idx.add(i + 1, path=p, content=c)
        if (i + 1) % 6 == 0:
            idx.commit()
    idx.commit()
    idx.wait_merges_quiet()
    res = []
    for entry in panel:
        hits = idx.search(entry["q"], limit=100000, highlights=True)
        docs = sorted(h.doc_id for h in hits)
        spans = sorted((h.doc_id, s, e) for h in hits for (s, e) in (h.highlights or {}).get("content", []))
        res.append({"label": entry["label"], "query": entry["q"], "docs": docs, "spans": spans})
        print(f"  {name:8} {entry['q']['value']:<18} {entry['label']:<10} {len(docs):>3} docs {len(spans):>4} spans")
    answers[name] = res
    idx.close()
    total = sum(f.stat().st_size for f in path.rglob("*") if f.is_file())
    print(f"{name}: {sum(1 for f in path.rglob('*') if f.is_file())} files, {total} bytes")

(OUT / "panel-3.0.8.json").write_text(json.dumps({"lucivy": "3.0.8", "documents": len(corpus), "panel": answers}, ensure_ascii=False, indent=1))
(OUT / "README.md").write_text("Index built by the published lucivy 3.0.8 (PyPI wheel) over 14 kernel files and four synthetic documents, with the wheel's own answers to a query panel (`panel-3.0.8.json`). Read by `lucivy_core/tests/test_compat_308.rs`: a v4 binary must answer the same, then convert the index on its first commit without losing a document or a span. Rebuild: `scratchpad/build-fixture-308.py` in a venv holding `lucivy==3.0.8`.\n")
