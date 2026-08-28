#!/usr/bin/env python3
"""Elasticsearch against lucivy, on the same corpus, judged by the same grep.

Why this exists. A speed comparison between search engines is endlessly
arguable — configuration, hardware, caches, warm-up. A *correctness* comparison
is not: either an engine returns the documents that contain what you asked for,
or it does not, and a byte-level scan of the same files says which. So every
row here carries three numbers — what grep says is true, what each engine
returned, and how long each took.

The rule this harness is built around: **Elasticsearch is configured at its
best, not at its default.** It can do substring search, with an ngram analyzer,
and it can do regex, with a `wildcard` field. Comparing against a default
`text` field would be comparing against a straw man, and the first informed
reader would say so — rightly. What we measure instead is what that
configuration costs: index size and indexing time, reported alongside.

Usage:

    docker run -d --name lucivy-es -p 9200:9200 \\
        -e discovery.type=single-node -e xpack.security.enabled=false \\
        -e ES_JAVA_OPTS=-Xms8g -Xmx8g \\
        docker.elastic.co/elasticsearch/elasticsearch:8.19.0

    python3 benches/compare_elasticsearch.py /tmp/lucivy-cmp

The corpus must be a directory of text files — the same one lucivy's panel ran
on, so the two sets are identical by construction rather than by two
implementations of the same selection rules:

    V3_CORPUS=/tmp/lucivy-cmp cargo test --release -p lucivy-core \\
        --test test_sfx_v3_ground_truth v3_ground_truth_demo -- --ignored --nocapture
"""

import json
import os
import pathlib
import re
import sys
import time
import urllib.error
import urllib.request

ES = os.environ.get("ES_URL", "http://localhost:9200")
STANDARD = "cmp_standard"
NGRAM = "cmp_ngram"


def req(method, path, body=None, timeout=600):
    data = None
    headers = {}
    if body is not None:
        if isinstance(body, str):
            data = body.encode()
            headers["Content-Type"] = "application/x-ndjson"
        else:
            data = json.dumps(body).encode()
            headers["Content-Type"] = "application/json"
    r = urllib.request.Request(f"{ES}{path}", data=data, headers=headers, method=method)
    try:
        with urllib.request.urlopen(r, timeout=timeout) as resp:
            return json.loads(resp.read() or b"{}")
    except urllib.error.HTTPError as e:
        detail = e.read().decode()[:400]
        raise SystemExit(f"{method} {path} -> {e.code}\n{detail}")


# The selection rules of lucivy's ground-truth harness, repeated here so the
# script can be pointed straight at a source tree. Both engines must see the
# same set or the counts compare nothing: same size cap, same exclusions, same
# treatment of binaries.
EXCLUDE = {"target", "node_modules", ".git", "build", "__pycache__", "playground"}
MAX_FILE = 100_000


def collect(root):
    """The corpus, selected exactly as `collect_files` in the Rust harness does."""
    root = pathlib.Path(root)
    files = []
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = sorted(d for d in dirnames if d not in EXCLUDE)
        for name in sorted(filenames):
            p = pathlib.Path(dirpath) / name
            try:
                if not p.is_file():
                    continue
                size = p.stat().st_size
                if size == 0 or size > MAX_FILE:
                    continue
                raw = p.read_bytes()
                if b"\0" in raw:
                    continue
                text = raw.decode("utf-8")
                if not text.strip():
                    continue
            except Exception:
                continue
            files.append((str(p.relative_to(root)), text))
    return files


# ── Index definitions ───────────────────────────────────────────────────────
# Two indices, because no single Elasticsearch field does all of this. That is
# itself part of the answer: the capability exists, spread across field types
# you have to choose in advance, and each one costs storage.

def standard_index():
    """What everyone starts with: the standard analyzer, BM25, whole words."""
    return {
        "settings": {"number_of_shards": 1, "number_of_replicas": 0},
        "mappings": {"properties": {
            "path": {"type": "keyword"},
            "body": {"type": "text", "analyzer": "standard"},
        }},
    }


def ngram_index():
    """Elasticsearch doing substring and regex, as well as it can.

    `ngram` is a *tokenizer*, not a filter: it runs over the character stream,
    so its trigrams cross the boundaries the standard analyzer would have cut —
    which is what makes `spin_lock` findable inside `raw_spin_lock`.

    `wildcard` is the field type Elastic added for exactly the case a `text`
    field cannot serve: leading wildcards and regular expressions over a whole
    value, without the term-by-term explosion of a `regexp` on an analyzed
    field.
    """
    return {
        "settings": {
            "number_of_shards": 1,
            "number_of_replicas": 0,
            "index.max_ngram_diff": 2,
            "analysis": {
                "tokenizer": {"tri": {
                    "type": "ngram", "min_gram": 3, "max_gram": 3,
                    # Everything is content, including separators: `spin_lock`
                    # has to stay findable across the underscore.
                    "token_chars": [],
                }},
                "analyzer": {"trigram": {
                    "tokenizer": "tri", "filter": ["lowercase"],
                }},
            },
        },
        "mappings": {"properties": {
            "path": {"type": "keyword"},
            "body": {"type": "text", "analyzer": "trigram"},
            "raw": {"type": "wildcard"},
        }},
    }


def build(name, mapping, files):
    req("DELETE", f"/{name}", None) if index_exists(name) else None
    req("PUT", f"/{name}", mapping)
    t0 = time.time()
    batch, sent = [], 0
    for i, (path, text) in enumerate(files):
        doc = {"path": path, "body": text}
        if "raw" in mapping["mappings"]["properties"]:
            doc["raw"] = text
        batch.append(json.dumps({"index": {"_index": name, "_id": str(i)}}))
        batch.append(json.dumps(doc))
        if len(batch) >= 400:
            req("POST", "/_bulk", "\n".join(batch) + "\n")
            sent += len(batch) // 2
            batch = []
            print(f"    {sent}/{len(files)}", end="\r", flush=True)
    if batch:
        req("POST", "/_bulk", "\n".join(batch) + "\n")
    req("POST", f"/{name}/_refresh")
    elapsed = time.time() - t0
    stats = req("GET", f"/{name}/_stats/store")
    size = stats["indices"][name]["total"]["store"]["size_in_bytes"]
    print(f"    {len(files)} docs in {elapsed:.1f}s, {size/2**20:.0f} MB on disk")
    return elapsed, size


def index_exists(name):
    try:
        req("GET", f"/{name}")
        return True
    except SystemExit:
        return False


def count(index, query):
    """Documents matched, and how long Elasticsearch says it took.

    `took` is the engine's own figure, which is the fair one to compare against
    lucivy's search time: it leaves out the HTTP round trip that lucivy, being
    in-process, never pays.
    """
    t0 = time.time()
    res = req("POST", f"/{index}/_search",
              {"query": query, "size": 0, "track_total_hits": True})
    wall = (time.time() - t0) * 1000
    return res["hits"]["total"]["value"], res["took"], wall


# ── The panel ───────────────────────────────────────────────────────────────
# The same queries as lucivy's demo panel, each expressed the best way
# Elasticsearch can express it. Where it has no way at all, that is the result.

def panel():
    return [
        # (label, what it asks, index, query, note)
        ("mutex_lock (whole words)", STANDARD,
         {"match_phrase": {"body": "mutex_lock"}},
         "the standard analyzer splits it into two terms; a phrase query puts them back"),

        ("mutex_lock (substring)", NGRAM,
         {"match_phrase": {"body": "mutex_lock"}},
         "trigrams, so it also matches inside a longer token"),

        ("spin_lock (substring)", NGRAM,
         {"match_phrase": {"body": "spin_lock"}},
         "must find raw_spin_lock, spin_lock_irqsave"),

        ("sched (whole word)", STANDARD,
         {"match": {"body": "sched"}}, ""),

        ("sched (substring)", NGRAM,
         {"match_phrase": {"body": "sched"}},
         "must also find sched_clock, schedule"),

        ("printk (start of token)", STANDARD,
         {"prefix": {"body": "printk"}},
         "prefix on analyzed terms — token start only, like lucivy's sw"),

        ("schdule (fuzzy, 1 edit)", STANDARD,
         {"match": {"body": {"query": "schdule", "fuzziness": 1}}},
         "Levenshtein on whole terms, not across boundaries"),

        ("regsiter (fuzzy, 2 edits)", STANDARD,
         {"match": {"body": {"query": "regsiter", "fuzziness": 2}}}, ""),

        ("spin_lock_[a-z]+ (regex)", NGRAM,
         {"regexp": {"raw": {"value": ".*spin_lock_[a-z]+.*",
                             "flags": "ALL", "case_insensitive": True}}},
         "on the wildcard field, the only one that can run a regex over a whole value"),
    ]


def main():
    root = sys.argv[1] if len(sys.argv) > 1 else "/tmp/lucivy-cmp"
    print(f"=== corpus: {root} ===")
    files = collect(root)
    total = sum(len(t.encode()) for _, t in files)
    print(f"{len(files)} files, {total/2**20:.1f} MB of text\n")
    if not files:
        raise SystemExit("empty corpus")

    print(f"=== indexing {STANDARD} (standard analyzer) ===")
    t_std, s_std = build(STANDARD, standard_index(), files)
    print(f"\n=== indexing {NGRAM} (trigrams + wildcard field) ===")
    t_ng, s_ng = build(NGRAM, ngram_index(), files)

    print(f"\n{'query':<34} {'index':<10} {'hits':>7} {'took':>8} {'wall':>9}")
    print("-" * 74)
    rows = []
    for label, index, query, note in panel():
        hits, took, wall = count(index, query)
        print(f"{label:<34} {index.replace('cmp_',''):<10} {hits:>7} {took:>6}ms {wall:>7.1f}ms")
        if note:
            print(f"{'':<34} {note}")
        rows.append({"query": label, "index": index, "hits": hits,
                     "took_ms": took, "wall_ms": round(wall, 1)})

    out = {
        "corpus": {"root": root, "files": len(files), "bytes": total},
        "indexing": {
            "standard": {"seconds": round(t_std, 1), "bytes": s_std},
            "ngram": {"seconds": round(t_ng, 1), "bytes": s_ng},
        },
        "queries": rows,
    }
    dest = pathlib.Path("/tmp/es_compare.json")
    dest.write_text(json.dumps(out, indent=2))
    print(f"\nindexing: standard {t_std:.1f}s / {s_std/2**20:.0f} MB — "
          f"trigram+wildcard {t_ng:.1f}s / {s_ng/2**20:.0f} MB "
          f"({s_ng/max(s_std,1):.1f}x the standard index)")
    print(f"written to {dest}")


if __name__ == "__main__":
    main()
