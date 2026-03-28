# 06 — Knowledge dump : session 28 mars 2026

## Ce qui a été fait

### 1. BM25 scoring pour regex (prescan DAG)
- `RegexPrescanParam` + `CachedRegexResult` + `highlights_to_doc_tf`
- `PrescanShardNode` exécute le regex prescan en parallèle par shard
- `MergePrescanNode` fusionne les 4 maps (sfx_cache, sfx_freqs, regex_cache, regex_freqs)
- `BuildWeightNode` injecte les deux caches + freqs
- `RegexContinuationWeight` : two-path scorer (cache fast path + run_regex_fallback)
- `SuffixContainsScorer` réutilisé pour BM25 regex (pub(crate))
- `ExportableStats.regex_doc_freqs` pour le distribué
- `search_with_global_stats` injecte `regex_doc_freqs`
- `RegexContinuationQuery.prescan_segments()` pour le path distribué
- Branche : `feature/regex-contains-literal`

### 2. Fix regex non-prefix literal
- `ag3.*ver` ne trouvait pas car le DFA était feedé depuis byte 0 du token
- Fix : feed depuis `text.find(first_literal).unwrap_or(0)` pour multi-literal
- `MIN_LITERAL_LEN` baissé de 3 à 1
- Branche : `feature/merge-incremental-sfx`

### 3. Perf indexation
- Fusion des 3 boucles collector (sfxpost + posmap + bytemap) en 1 seul pass
- Buffer contigu pour SuffixFstBuilder (Vec<u8> au lieu de Vec<String>)
- Sibling table : flat Vec + sort_unstable/dedup au lieu de HashMap<u32, HashSet>
- Profiling sous feature flag `sfx-profile` (zero-cost quand désactivé)
- Merge N-way sur term dict streams (remplace BTreeSet + HashMap)
- ByteBitmapWriter::copy_bitmap() pour copie directe pendant merge
- Résultats : 6.3s release pour 5K docs (vs 36s debug)

### 4. Fuzzy via trigram pigeonhole
- `generate_ngrams()` : bigrams pour queries courtes, trigrams pour longues
- `intersect_trigrams_with_threshold()` : ordre + seuil + byte span
- Validation Levenshtein DFA via PosMap + validate_path sur les candidats
- Routing `contains + distance > 0` → `RegexContinuationQuery` dans `build_contains_query`
- Tests natifs 21/21 passent
- Branche : `feature/fuzzy-via-literal-resolve`

### 5. Docs
- `14-design-regex-prescan-final.md` — architecture prescan regex
- `01-design-merge-incremental-sfx.md` — merge N-way
- `02-bug-non-prefix-literal-dfa-offset.md` — bug fix DFA offset
- `03-profiling-indexation-bottlenecks.md` — résultats profiling
- `04-design-fuzzy-via-trigram-pigeonhole.md` — design fuzzy trigrams
- `05-bugs-connus-fuzzy-regex-highlights.md` — bugs restants
- `linkedin-regex-demo-fr.md` — post LinkedIn

## Scripts et commandes utiles

### Construire le .luce du playground
```bash
# Nécessite le binding Python compilé
cd bindings/python && source .venv/bin/activate && maturin develop --release
cd ../..
source bindings/python/.venv/bin/activate
python3 playground/build_dataset.py
# Produit playground/dataset.luce (~34 MB, ~885 docs)
```

### Tester en natif sur le .luce
```bash
cargo test -p lucivy-core --test test_luce_roundtrip -- --nocapture
# Test : import le .luce, search contains/fuzzy/regex, vérifie highlights
# Fichier : lucivy_core/tests/test_luce_roundtrip.rs
```

### Tester en Python sur le .luce
```bash
source bindings/python/.venv/bin/activate
python3 -c "
from lucivy import Index
idx = Index.import_snapshot_from('playground/dataset.luce', '/tmp/test')
# IMPORTANT : passer un dict, PAS json.dumps(dict)
r = idx.search({'type': 'contains', 'field': 'content', 'value': 'weaver'}, 10)
print(len(r))
# Fuzzy : passer distance comme int
r = idx.search({'type': 'contains', 'field': 'content', 'value': 'weavr', 'distance': 1}, 10)
"
```

### Bench 5K / 90K docs
```bash
# 5K docs (rapide, ~6s release)
find /home/luciedefraiteur/lucivy_bench_sharding -name "*.lock" -delete
BENCH_MODE="RR" MAX_DOCS=5000 cargo test -p lucivy-core --test bench_sharding \
  bench_sharding_comparison --release -- --nocapture > /tmp/bench.txt 2>&1

# 90K docs (lent, merge cascade)
BENCH_MODE="RR" MAX_DOCS=90000 cargo test -p lucivy-core --test bench_sharding \
  bench_sharding_comparison --release -- --nocapture > /tmp/bench_90k.txt 2>&1

# Score consistency (nécessite single + RR indexés)
cargo test -p lucivy-core --test bench_sharding test_score_consistency -- --nocapture

# Avec profiling SFX (debug uniquement, feature flag)
cargo test -p lucivy-core --test bench_sharding bench_sharding_comparison \
  --features sfx-profile -- --nocapture
```

### Build WASM
```bash
bash bindings/emscripten/build.sh
# Copie playground/pkg/lucivy.{js,wasm}
```

### Build Python binding
```bash
cd bindings/python
source .venv/bin/activate
unset CONDA_PREFIX  # IMPORTANT: sinon conflit venv/conda
maturin develop --release
```

### Servir le playground
```bash
node playground/serve.mjs
# → http://localhost:9877/
```

## Pièges et blocages rencontrés

### 1. Python binding : string vs dict
`idx.search(json.dumps({...}), 10)` traite le JSON comme un texte et fait
`contains_split` sur chaque mot. Il faut `idx.search({...}, 10)` (dict directement).
Le binding Python a un `parse_query` qui dispatch sur le type : str → contains_split,
dict → serde JSON parse.

### 2. Python binding pas rebuild
Le binding Python doit être rebuild avec `maturin develop --release` après chaque
changement dans ld-lucivy ou lucivy_core. Sinon les anciennes fonctions sont utilisées.
Un `.luce` construit avec un ancien binding peut ne pas avoir de SFX files.

### 3. CONDA_PREFIX conflit
`maturin develop` échoue avec "Both VIRTUAL_ENV and CONDA_PREFIX are set".
Fix : `unset CONDA_PREFIX` avant `maturin develop`.

### 4. Node.js WASM crash
Le test `test_regex_perf.mjs` crash avec "Program terminated with exit(0)" au
moment de l'import snapshot. C'est un problème ASYNCIFY + Node.js, pas un bug
lucivy. Le WASM fonctionne dans le browser (SharedArrayBuffer + pthreads).

### 5. tantivy-fst n'est PAS dans lucivy-core
`tantivy-fst` est une dépendance de `ld-lucivy` mais PAS de `lucivy-core`.
On ne peut pas utiliser `tantivy_fst::Regex` dans les DAG nodes. Solution :
`run_regex_prescan()` / `run_fuzzy_prescan()` dans ld-lucivy, appelés par
la DAG via l'export public.

### 6. Lock files bench
Les lock files des index de bench persistent entre les runs. Toujours faire
`find .../lucivy_bench_sharding -name "*.lock" -delete` avant de relancer.

### 7. Debug vs Release
Les timings en debug sont 3-10x plus lents que release. Le profiling en debug
montrait 36s pour 5K docs, vs 6.3s en release. Ne pas optimiser sur la base
de timings debug.

### 8. .luce ancien sans SFX
Un .luce construit avant l'ajout du SFX (ou avec un binding non rebuild)
n'a pas de fichiers .sfx/.sfxpost/.posmap/.bytemap. Les contains queries
tombent dans le fallback `ngram_contains_query` qui crash sur les caractères
multi-byte (é, ×, etc.). Fix : reconstruire le .luce avec le binding à jour.

### 9. Multi-segment et find_literal
Le `.luce` du playground a plusieurs segments (chaque commit crée un segment).
`find_literal` est per-segment. Un trigramme peut retourner 0 matches dans un
segment mais N dans un autre — c'est normal. Le fuzzy trigram est appelé
per-segment par le scorer.

### 10. SegmentReader API
`searcher.doc(addr)` retourne un `LucivyDocument`. Pour extraire le texte :
```rust
let doc: ld_lucivy::LucivyDocument = searcher.doc(addr).unwrap();
let val = doc.get_first(field);
// val est CompactDocValue, convertir en OwnedValue :
let owned: OwnedValue = val.into();
match owned { OwnedValue::Str(s) => ..., _ => ... }
```

## Fichiers clés modifiés

### ld-lucivy (core)
- `src/query/query.rs` — RegexPrescanParam, 4 nouveaux trait methods
- `src/query/mod.rs` — exports, pub mod automaton_weight + phrase_query
- `src/query/automaton_weight.rs` — SfxAutomatonAdapter pub
- `src/query/phrase_query/regex_continuation_query.rs` — fuzzy trigram, prescan, DFA validation
- `src/query/phrase_query/literal_resolve.rs` — intersect_trigrams_with_threshold
- `src/query/phrase_query/suffix_contains_query.rs` — SuffixContainsScorer pub(crate)
- `src/query/boolean_query/boolean_query.rs` — propagation prescan methods
- `src/suffix_fst/collector.rs` — single-pass, contiguous buffer, sfx-profile
- `src/suffix_fst/builder.rs` — contiguous key buffer
- `src/suffix_fst/sibling_table.rs` — flat Vec
- `src/suffix_fst/bytemap.rs` — copy_bitmap
- `src/indexer/merger.rs` — N-way merge sort
- `Cargo.toml` — sfx-profile feature

### lucivy_core (DAG + handle)
- `lucivy_core/src/search_dag.rs` — PrescanResult 4-tuple, regex prescan
- `lucivy_core/src/bm25_global.rs` — regex_doc_freqs
- `lucivy_core/src/sharded_handle.rs` — regex prescan injection
- `lucivy_core/src/query.rs` — routing contains+distance→RegexContinuationQuery
- `lucivy_core/tests/test_luce_roundtrip.rs` — test natif sur .luce
- `lucivy_core/benches/bench_sharding.rs` — contains regex queries

### Playground
- `playground/index.html` — aucun changement fonctionnel
- `playground/build_dataset.py` — utilisé pour reconstruire le .luce

## Branches

| Branche | Contenu | État |
|---------|---------|------|
| `feature/regex-contains-literal` | BM25 regex prescan, collector single-pass | poussé, mergeable |
| `feature/merge-incremental-sfx` | N-way merge, contiguous buffer, sibling flat, sfx-profile | poussé |
| `feature/fuzzy-via-literal-resolve` | Fuzzy trigram pigeonhole, routing contains+d>0 | poussé, bugs à fixer |

## Prochaines étapes

1. **Fix bug 1** : validation DFA cross-token pour fuzzy (le plus critique)
2. **Fix bug 2** : highlights fuzzy décalés
3. **Fix bug 3** : regex `sched[a-z]+` validation DFA
4. **Supprimer les eprintln debug** : [fuzzy-debug], [debug], [intersect-debug]
5. **Bench 90K release** : après merge des branches
6. **Score consistency** : ajouter fuzzy + regex contains au bench
7. **Merge incrémental** : implémenter le design doc 01 (N-way merge sfxpost)
