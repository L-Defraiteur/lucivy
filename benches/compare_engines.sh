#!/bin/bash
# lucivy against Elasticsearch and tantivy, on one corpus, judged by one scan.
#
# One command, one Markdown report (`work_dir/compare_engines.md`), four parts:
#
#   1. index size and indexing time — every engine configured to answer a
#      substring query, not a straw man (Elasticsearch: trigram analyzer plus a
#      `wildcard` field; tantivy: `NgramTokenizer`; lucivy: its three layouts);
#   2. the nine verified queries (substring, whole word, prefix, fuzzy at one
#      and two edits, regex): the truth is a byte-by-byte scan of the files, run
#      by lucivy's ground-truth harness; every engine's count sits next to it;
#   3. where the questions differ — separators relaxed, fuzzy across a token
#      boundary, a regex, a two-character needle, a fuzzy phrase: what each
#      engine can express, what it returns, against the same truth;
#   4. the price of knowing *where* it matched: spans, on both sides.
#
#     benches/compare_engines.sh /tmp/lucivy-cmp-90k [work_dir]
#
# Needs the Rust toolchain and python3. Elasticsearch is optional: with none at
# $ES_URL (http://localhost:9200) its columns stay empty. To have one:
#
#     docker run -d --name lucivy-es -p 9200:9200 \
#         -e discovery.type=single-node -e xpack.security.enabled=false \
#         -e ES_JAVA_OPTS="-Xms8g -Xmx8g" \
#         docker.elastic.co/elasticsearch/elasticsearch:8.19.0
#
# The corpus is a directory of text files, e.g. `git clone --depth=1
# https://github.com/torvalds/linux /tmp/lucivy-cmp-90k` (93 983 files, about
# 45 minutes all told on a 24-core machine, most of it the reference scans; a
# 10 000-file subset takes a few minutes). A lucivy index already in `work_dir`
# with the same shape is reused, not rebuilt: delete it to remeasure indexing.
set -u
export PATH="$HOME/.cargo/bin:$PATH"
CORPUS="${1:?corpus directory}"
WORK="${2:-/tmp/lucivy-compare}"
ES_URL="${ES_URL:-http://localhost:9200}"
HERE="$(cd "$(dirname "$0")/.." && pwd)"
mkdir -p "$WORK"

T="cargo test --release -p lucivy-core --test test_sfx_v3_ground_truth v3_ground_truth_demo -- --ignored --nocapture"
HARNESS="V3_CORPUS=$CORPUS V3_MAX_DOCS=1000000 V3_COMMIT_EVERY=10000"

# The lucivy indexes: three layouts, the default nine-query panel each.
for spec in "dict:V3_SFX_VERSION=4" "dict-ram:V3_SFX_VERSION=4 V3_DERIVED_IN_RAM=1" "v3:V3_SFX_VERSION=3"; do
    name="${spec%%:*}"; vars="${spec#*:}"
    echo "== lucivy $name"
    ( cd "$HERE" && eval env $HARNESS $vars V3_INDEX_DIR="$WORK/$name" $T ) > "$WORK/lucivy-$name.log" 2>&1
    echo "   $(grep -o '[0-9]* pass, [0-9]* fail' "$WORK/lucivy-$name.log" | tail -1), $(du -smL "$WORK/$name" | cut -f1) MB"
    du -sbL "$WORK/$name" | cut -f1 > "$WORK/lucivy-$name.bytes"
done

# The queries where the other engines' formulations differ, on the dictionary
# index, with the span cap lifted (`de` alone has millions of spans).
echo "== lucivy, where the questions differ"
( cd "$HERE" && eval env $HARNESS V3_SFX_VERSION=4 V3_INDEX_DIR="$WORK/dict" LUCIVY_HIGHLIGHT_SPAN_CAP=0 \
    V3_QUERIES="'mutex_lock:strict,spin_lock:strict,spin_lock:relax,spinlokc:fz2,spin_lock_[a-z]+:rx,ude:strict,de:strict,retur\\s-ENOMEM:fz1'" $T ) \
    > "$WORK/lucivy-stumble.log" 2>&1
echo "   $(grep -o '[0-9]* pass, [0-9]* fail' "$WORK/lucivy-stumble.log" | tail -1)"

# tantivy 0.25 from crates.io (not the fork), default and trigram tokenizers.
echo "== tantivy"
( cd "$HERE" && CMP_CORPUS="$CORPUS" CMP_OUT="$WORK/tantivy.json" \
    cargo test --release -p lucivy-core --test compare_tantivy compare_tantivy -- --ignored --nocapture ) > "$WORK/tantivy.log" 2>&1
[ -f "$WORK/tantivy.json" ] && echo "   $(grep -o 'indexing: .*' "$WORK/tantivy.log" | tail -1)" || echo "   failed (see $WORK/tantivy.log)"

# Elasticsearch, if one answers.
if curl -sf "$ES_URL" > /dev/null 2>&1; then
    echo "== Elasticsearch at $ES_URL"
    ( cd "$HERE" && ES_URL="$ES_URL" python3 benches/compare_elasticsearch.py "$CORPUS" ) > "$WORK/elasticsearch.log" 2>&1
    [ -f /tmp/es_compare.json ] && cp /tmp/es_compare.json "$WORK/elasticsearch.json"
    echo "   $(grep -o 'indexing: .*' "$WORK/elasticsearch.log" | tail -1)"
else
    echo "== no Elasticsearch at $ES_URL — its columns stay empty (see the header for the docker command)"
fi

echo "== report"
python3 "$HERE/benches/compare_engines_report.py" "$WORK" > "$WORK/compare_engines.md"
echo "   $WORK/compare_engines.md"
echo
cat "$WORK/compare_engines.md"
