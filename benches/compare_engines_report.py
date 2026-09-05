#!/usr/bin/env python3
"""Assemble the engine comparison report from what `compare_engines.sh` ran.

Reads, in the work directory: `lucivy-{dict,dict-ram,v3}.log` and `.bytes`
(the ground-truth harness on each lucivy layout), `lucivy-stumble.log` (the
harness on the queries where the other engines' formulations differ),
`tantivy.json` and `elasticsearch.json` (each optional). Writes Markdown to
stdout. The truth in every row is the harness's scan of the files: a lucivy
row is `OK` only if its documents *and* byte spans match that scan.
"""
import json
import pathlib
import re
import sys

WORK = pathlib.Path(sys.argv[1] if len(sys.argv) > 1 else "/tmp/lucivy-compare")

ROW = re.compile(
    r"^(?P<text>.+?)\s+(?P<mode>strict|relax|term|terms|sw|sws|fz1|fz2|fz3|rx|jw1)\s+"
    r"(?P<truth>\S+)\s+(?P<docs>\d+)\s+(?P<status>OK|FAIL|n/a) \((?P<search>[0-9.]+)ms search, "
    r"(?P<fetch>[0-9.]+)ms \+fetch, (?:(?P<grep>[0-9.]+)ms grep|no grep)\) (?P<rest>.*)$")


def parse_log(path):
    """The harness rows of one log, keyed `value:mode` (V3_QUERIES syntax)."""
    rows, meta = {}, {}
    if not path.exists():
        return rows, meta
    for line in path.read_text(errors="replace").splitlines():
        m = ROW.match(line.rstrip())
        if m:
            d = m.groupdict()
            spans = re.search(r"spans (\d+) exact", d["rest"]) or re.search(r"spans gt=(\d+)", d["rest"])
            mode = {"terms": "term", "sws": "sw"}.get(d["mode"], d["mode"])
            key = f"{d['text'].strip().replace(' ', chr(92) + 's')}:{mode}"
            rows[key] = {"text": d["text"].strip(), "mode": mode, "truth": d["truth"], "docs": int(d["docs"]),
                         "status": d["status"], "search_ms": float(d["search"]),
                         "grep_ms": float(d["grep"]) if d["grep"] else None,
                         "spans": int(spans.group(1)) if spans else None,
                         "truncated": "truncated" in d["rest"]}
            continue
        m = re.search(r"Index time: ([0-9.]+)s", line)
        if m:
            meta["index_s"] = float(m.group(1))
        if "index reused" in line:
            meta["reused"] = True
        m = re.search(r"result cap (\d+)", line)
        if m:
            meta["files"] = int(m.group(1))
        m = re.search(r"(\d+) pass, (\d+) fail", line)
        if m:
            meta["pass"], meta["fail"] = int(m.group(1)), int(m.group(2))
    return rows, meta


def load_json(name):
    p = WORK / name
    return json.loads(p.read_text()) if p.exists() else None


def mb(n):
    return f"{n / 2**20:,.0f} MB".replace(",", " ")


def n(x):
    return f"{x:,}".replace(",", " ")


lucivy = {name: parse_log(WORK / f"lucivy-{name}.log") for name in ("dict", "dict-ram", "v3")}
sizes = {}
for name in lucivy:
    p = WORK / f"lucivy-{name}.bytes"
    if p.exists():
        sizes[name] = int(p.read_text().strip())
stumble, stumble_meta = parse_log(WORK / "lucivy-stumble.log")
tv = load_json("tantivy.json")
es = load_json("elasticsearch.json")

# The truth (and lucivy's own row) for a `value:mode` key: the dictionary panel
# first, then the stumble run, then any other layout.
def lrow(key):
    for src in (lucivy["dict"][0], stumble, lucivy["v3"][0], lucivy["dict-ram"][0]):
        if key in src:
            return src[key]
    return None


def truth(key):
    r = lrow(key)
    return r["truth"] if r else "—"


def mark(hits, key):
    """An engine's count against the truth: the number, and whether it agrees."""
    t = truth(key)
    if hits is None:
        return "—"
    try:
        return f"**{n(hits)}**" if int(t) == hits else n(hits)
    except ValueError:
        return n(hits)


files = None
text_bytes = None
for src in (es, tv):
    if src:
        files = src["corpus"]["files"]
        text_bytes = src["corpus"]["bytes"]
if files is None:
    files = lucivy["dict"][1].get("files")
text_mb = text_bytes / 2**20 if text_bytes else None

out = []
P = out.append
P(f"# lucivy against Elasticsearch and tantivy — one corpus, one truth")
P("")
corpus_line = f"{n(files)} files" if files else "corpus"
if text_mb:
    corpus_line += f", {n(round(text_mb))} MB of text"
P(f"Corpus: {corpus_line} (text files of 100 KB at most, no binaries, the same selection for every engine). "
  "The truth of every row is a byte-by-byte scan of the files by lucivy's ground-truth harness; "
  "a lucivy count is only reported `OK` when its documents **and** its byte spans match that scan. "
  "A count in bold equals the truth.")
P("")

# ── 1. size ──
P("## 1. Index size and indexing time")
P("")
P("| engine | how it answers a substring | index | × text | indexing |")
P("|---|---|---|---|---|")
def ratio(b):
    return f"×{b / text_bytes:.1f}" if text_bytes else "—"
if es:
    P(f"| Elasticsearch 8.19, standard analyzer | it does not (whole words) | {mb(es['indexing']['standard']['bytes'])} | {ratio(es['indexing']['standard']['bytes'])} | {es['indexing']['standard']['seconds']:.0f} s |")
    P(f"| Elasticsearch 8.19, trigram analyzer + `wildcard` field | trigram phrases; regex on the wildcard field | {mb(es['indexing']['ngram']['bytes'])} | {ratio(es['indexing']['ngram']['bytes'])} | {es['indexing']['ngram']['seconds']:.0f} s |")
if tv:
    P(f"| tantivy 0.25, default tokenizer | it does not (whole words) | {mb(tv['indexing']['default']['bytes'])} | {ratio(tv['indexing']['default']['bytes'])} | {tv['indexing']['default']['seconds']:.0f} s |")
    P(f"| tantivy 0.25, `NgramTokenizer` (trigrams) | trigram phrases (positions all 0: candidates only) | {mb(tv['indexing']['trigram']['bytes'])} | {ratio(tv['indexing']['trigram']['bytes'])} | {tv['indexing']['trigram']['seconds']:.0f} s |")
labels = {"v3": "lucivy 4.0, a dictionary per segment (`sfx_version` 3)",
          "dict": "lucivy 4.0, shared dictionary per shard",
          "dict-ram": "lucivy 4.0, shared dictionary + `derived_in_ram`"}
for name in ("v3", "dict", "dict-ram"):
    if name in sizes:
        meta = lucivy[name][1]
        t = "reused" if meta.get("reused") else (f"{meta['index_s']:.0f} s" if "index_s" in meta else "—")
        P(f"| {labels[name]} | suffix FST, exact spans | {mb(sizes[name])} | {ratio(sizes[name])} | {t} |")
P("")

# ── 2. the nine queries ──
P("## 2. The nine verified queries")
P("")
P("| query | mode | truth (scan) | lucivy | spans | lucivy | Elasticsearch | tantivy |")
P("|---|---|---|---|---|---|---|---|")
NINE = [("mutex_lock:strict", "substring"), ("mutex_lock:relax", "separators relaxed"),
        ("spin_lock:strict", "substring"), ("sched:term", "whole word"), ("sched:strict", "substring"),
        ("printk:sw", "start of token"), ("schdule:fz1", "fuzzy, 1 edit"), ("regsiter:fz2", "fuzzy, 2 edits"),
        ("spin_lock_[a-z]+:rx", "regex")]
es_by_truth = {r["truth"]: r for r in (es or {}).get("queries", []) if r.get("truth")}
tv_by_truth = {}
for r in (tv or {}).get("queries", []):
    tv_by_truth.setdefault(r["truth"], r)
for key, mode in NINE:
    r = lrow(key)
    if not r:
        continue
    e = es_by_truth.get(key)
    t = tv_by_truth.get(key)
    lucol = f"**{n(r['docs'])}** {r['status']}" if r["status"] == "OK" else f"{n(r['docs'])} {r['status']}"
    truth_col = n(int(r['truth'])) if r['truth'].isdigit() else r['truth']
    spans_col = n(r['spans']) if r['spans'] else '—'
    es_col = f"{mark(e['hits'], key)} · {e['took_ms']} ms" if e else '—'
    tv_ms = f"{t['ms']:.0f}" if t else ''
    tv_col = f"{mark(t['hits'], key)} · {tv_ms} ms" if t else '—'
    P(f"| `{r['text']}` | {mode} | {truth_col} | {lucol} | {spans_col} | {r['search_ms']:.0f} ms | {es_col} | {tv_col} |")
P("")
P("lucivy's time is the search alone (documents and every span); Elasticsearch's is its own `took`, first run of each query; "
  "tantivy's is the count, or for substrings the whole verified path (see §3). Whole-word and prefix counts depend on each "
  "engine's definition of a word: lucivy's harness counts `sched` bounded by separators on both sides; the standard analyzer "
  "keeps `sched_clock` as one term and splits on `/`, so its whole-word and prefix rows are close but not equal. "
  "Elasticsearch runs the substring rows on its trigram index and the whole-word, prefix and fuzzy rows on its standard one; "
  "tantivy likewise. A fuzzy row that is not bold is not a miscount: their fuzziness compares whole terms, lucivy's a substring "
  "that may cross a separator — the questions differ, and the row shows by how much.")
P("")

# ── 3. where the questions differ ──
P("## 3. Where the questions differ")
P("")
P("| what is asked | truth (scan) | lucivy | Elasticsearch | tantivy |")
P("|---|---|---|---|---|")
CASES = [
    ("spin_lock:strict", "`spin_lock`, separators strict"),
    ("spin_lock:relax", "`spin_lock`, separators relaxed — also `spin lock`, `spin-lock`, `spinlock`"),
    ("spinlokc:fz2", "`spinlokc`, two edits, across the token boundary"),
    ("spin_lock_[a-z]+:rx", "`spin_lock_[a-z]+`, a regex"),
    ("ude:strict", "`ude`, three characters"),
    ("de:strict", "`de`, two characters"),
    ("retur\\s-ENOMEM:fz1", "`retur -ENOMEM`, a fuzzy phrase (one edit: a letter missing)"),
]
es_st = {}
for r in (es or {}).get("stumble", []):
    es_st.setdefault(r["truth"], []).append(r)
tv_st = {}
for r in (tv or {}).get("queries", []):
    tv_st.setdefault(r["truth"], []).append(r)
for key, what in CASES:
    r = lrow(key)
    if not r:
        continue
    def cell(rows):
        if not rows:
            return "—"
        parts = []
        for x in rows:
            hits = x["hits"]
            ms = x.get("took_ms", x.get("ms"))
            label = x["query"]
            parts.append(f"{mark(hits, key)} ({label}{f', {ms:.0f} ms' if isinstance(ms, (int, float)) else ''})")
        return "<br>".join(parts)
    trunc = " (truncated: span cap)" if r["truncated"] else ""
    lucol = f"**{n(r['docs'])}** {r['status']}{trunc}, {n(r['spans']) if r['spans'] else '—'} spans, {r['search_ms']:.0f} ms"
    P(f"| {what} | {n(int(r['truth'])) if r['truth'].isdigit() else r['truth']} | {lucol} | {cell(es_st.get(key))} | {cell(tv_st.get(key))} |")
P("")
P("Read across a row: the same question, what each engine can make of it (an Elasticsearch time here may be a cache hit: "
  "the same query already ran in §2). "
  "(its trigrams carry them); tantivy's default tokenizer cannot keep them (the separator never enters the index), "
  "and its n-gram tokenizer emits every position as 0, so its substring rows are an AND of trigrams verified by "
  "reading each candidate's stored text — the application's work, timed here as such. "
  "Both engines' fuzziness stops at their token boundary. An n-gram index has nothing to look up below three characters. "
  "The fuzzy phrase is the case Elasticsearch handles well, with `span_near`.")
P("")

# ── 4. spans ──
P("## 4. The price of knowing where")
P("")
r = lrow("mutex_lock:strict")
P("| engine | documents | spans reported | in how many documents | time |")
P("|---|---|---|---|---|")
if r:
    P(f"| lucivy (every document, every span, verified) | {n(r['docs'])} | {n(r['spans']) if r['spans'] else '—'} | all {n(r['docs'])} | {r['search_ms']:.0f} ms |")
if es and es.get("highlight"):
    h = es["highlight"]
    P(f"| Elasticsearch, `highlight` on the top {h['highlighted']} | {n(h['docs'])} | {n(h['spans'])} (as marked by the engine) | {h['highlighted']} | {h['took_ms']} ms + {h['parse_ms']:.1f} ms to parse {h['bytes']/2**20:.1f} MB of markup |")
if tv and tv.get("highlight"):
    h = tv["highlight"]
    P(f"| tantivy, AND of trigrams verified on the stored text, occurrences in the first {h['highlighted']} | {n(h['docs'])} | {n(h['spans'])} | {h['highlighted']} | {h['ms']:.0f} ms (the whole path) |")
P("")
P("`mutex_lock`, separators strict. lucivy's spans come out of the index with the documents. Elasticsearch re-reads "
  "and re-analyses each hit's stored text (`highlight`), priced on the top 200. tantivy's trigram index has no usable "
  "positions (its n-gram tokenizer emits 0 for every token, so a trigram phrase matches nothing): the honest path is an "
  "AND of trigrams, then reading every candidate's stored text to verify the substring and count its occurrences — the "
  "time shown is that whole path, occurrences counted in the first 200 verified documents.")
P("")
P("## How this was produced")
P("")
P("`benches/compare_engines.sh <corpus>`: lucivy's ground-truth harness (`lucivy_core/tests/test_sfx_v3_ground_truth.rs`, "
  "`v3_ground_truth_demo`, then the same with `V3_QUERIES` for section 3), `lucivy_core/benches/compare_tantivy.rs` "
  "(tantivy 0.25 from crates.io, not the fork) and `benches/compare_elasticsearch.py` (Elasticsearch 8.19 in a container, "
  "configured at its best: trigram analyzer, `wildcard` field). Logs and JSON next to this file.")
sys.stdout.write("\n".join(out) + "\n")
