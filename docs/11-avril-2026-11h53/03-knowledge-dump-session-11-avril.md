# Knowledge Dump — 11 avril 2026

## Architecture globale

### Couches

1. **ld-lucivy** : moteur core (index, query, scoring, merger, segments)
2. **lucivy_core** : handle unifié (`LucivyHandle`), query builder, tokenizers,
   snapshot, blob store, search DAG
3. **6 bindings** :
   - CXX bridge rag3db (`lucivy_fts/rust/src/bridge.rs`)
   - WASM emscripten (`bindings/emscripten/src/lib.rs`)
   - WASM wasm-bindgen (`bindings/wasm/src/lib.rs`)
   - Node.js napi (`bindings/nodejs/src/lib.rs`)
   - Python PyO3 (`bindings/python/src/lib.rs`)
   - C++ standalone (`bindings/cpp/src/lib.rs`)

### SFX (Suffix FST)

Le SFX est l'index propriétaire de lucivy pour les recherches substring/contains.
Construit par `SfxCollector` pendant l'écriture d'un segment. Optionnel (`sfx: false`).

Fichiers par segment :
- `.sfx` — FST des suffixes (prefix_walk, falling_walk, fuzzy_walk)
- `.sfxpost` — postings par ordinal (doc_id, position, byte_from, byte_to)
- `.posmap` — ordinal par (doc_id, position) pour DFA walk
- `.bytemap` — bitmask de bytes par ordinal pour DFA pre-filter
- `.termtexts` — texte par ordinal (ord_to_term)
- `.gapmap` — séparateurs entre positions (pour DFA gap feeding)
- `.sibling` — table d'adjacence entre tokens (ordinal → successors)
- `.sepmap` — bitmap séparateurs par ordinal
- `.freqmap` — doc_freq + term_freq par ordinal

### Search DAG

Le DAG orchestre la recherche :
```
drain → flush → needs_prescan?
  ├── then → prescan_0..N ∥ → merge_prescan → build_weight → search_0..N ∥ → merge
  └── else → build_weight → search_0..N ∥ → merge
```

Le prescan calcule les résultats SFX/regex/fuzzy par segment, merge les stats
(IDF global), puis `build_weight` utilise ces stats pour le scoring BM25 correct.

**Important** : `searcher.search()` directement bypass le prescan. Seul le
chemin via LucivyHandle → DAG donne l'IDF globalisé.

## Pipelines de recherche

### Contains exact (d=0)

Route : `SuffixContainsQuery` → prescan SFX walk → BM25 scoring.
Split multi-token : whitespace → intersection ordonnée des tokens.

### Fuzzy contains (d>0) — NOUVEAU PIPELINE (11 avril 2026)

Fichier : `src/query/phrase_query/fuzzy_contains.rs`

Pipeline indépendant du regex, basé sur trigrams + positions :

1. **concat_query** : strip séparateurs, lowercase
2. **generate_trigrams** : sliding window (bigrams si court, trigrams sinon)
3. **Resolve sélectif** : FST walk + falling walk par trigram, rarest first,
   doc filter progressif
4. **build_hits_by_doc** : dico `DocId → position → Vec<TrigramHit>`
5. **find_matches** : two-pointer sur positions triées
6. **Highlights** : recalés depuis byte_from des trigrams extrêmes
7. **Coverage** : matched/total trigrams ratio pour scoring

Threshold : `max(total - n*d - (n-1)*boundaries, 2)`
- `n*d` : trigrams cassés par les edits
- `(n-1)*boundaries` : trigrams aux frontières de mots (falling walk contiguous
  ne les trouve pas)

Scoring : `coverage * 1000 + bm25_score`
- Coverage domine le BM25 → les résultats avec moins de miss sont toujours devant

### Regex contains

Route : `RegexContinuationQuery` → DFA walk via PosMap/GapMap.
Extraction de literals, DFA validation. Inchangé.

### Autres types de query

- **term** : exact token match, term dict standard
- **phrase** : multi-token exact sequence
- **fuzzy** (top-level) : Levenshtein sur term dict (pas SFX)
- **regex** (top-level) : regex sur term dict (pas SFX)
- **contains_split** / **startsWith_split** : whitespace split → boolean should
- **phrase_prefix** : autocomplétion
- **disjunction_max** : max score + tie_breaker
- **more_like_this** : find similar docs
- **boolean** : must/should/must_not

## Structures de données clés

### TrigramHit (fuzzy_contains.rs)
```rust
struct TrigramHit {
    tri_idx: usize,        // quel trigram de la query
    position: u32,         // token index dans le doc
    byte_from: u32,        // byte offset dans le contenu
    byte_to: u32,
    si: u16,               // suffix index dans le parent token
    token_parts: Vec<String>, // décomposition cross-token
}
```

### CachedPrescanResult (regex_continuation_query.rs)
```rust
struct CachedPrescanResult {
    doc_tf: Vec<(DocId, u32)>,
    highlights: Vec<(DocId, usize, usize)>,
    doc_coverage: Vec<(DocId, f32)>,  // fuzzy only
}
```

### LiteralMatch (literal_resolve.rs)
```rust
struct LiteralMatch {
    doc_id: DocId,
    position: u32,
    byte_from: u32,
    byte_to: u32,
    si: u16,
    token_len: u16,
    ordinal: u32,
}
```

### FstCandidate (literal_pipeline.rs)
```rust
struct FstCandidate {
    raw_ordinal: u64,
    si: u16,
    token_len: u16,
}
```

### CrossTokenChain (literal_pipeline.rs)
```rust
struct CrossTokenChain {
    ordinals: Vec<u64>,
    first_si: u16,
    prefix_len: usize,
}
```

### SiblingEntry (sibling_table.rs)
```rust
struct SiblingEntry {
    next_ordinal: u32,
    gap_len: u16,  // 0 = contiguous (CamelCase)
}
```

## Falling walk

`SfxFileReader::falling_walk(query)` : byte-by-byte FST walk, trouve les
points de split où `si + prefix_len == token_len` (fin de token atteinte).

`SfxFileReader::fuzzy_falling_walk(query, distance)` : DFS avec DFA
Levenshtein, prune via `can_match()`. Utilise `fst_depth` comme point de
split (pas query consumption).

`cross_token_falling_walk(literal, distance, ord_to_term)` : falling walk +
sibling chain DFS. Utilise `contiguous_siblings()` (gap=0).

`cross_token_falling_walk_any_gap(...)` : variante avec `siblings()` (tous
gaps). **Attention** : explose combinatoirement sur les tokens communs. Le
first-byte filter aide (~10%) mais pas suffisant. Non utilisé en prod.

**First-byte filter** (ajouté 11 avril) : dans le DFS sibling, skip les tokens
dont le premier byte ne matche pas le remainder. Réduit les lookups `ord_to_term`.

## Scoring BM25

`EnableScoring::Enabled` porte un `Arc<dyn Bm25StatisticsProvider>`.

IDF globalisé via prescan : `global_regex_doc_freq` calculé cross-segments dans
le DAG. Sans prescan, chaque segment utilise son IDF local → scores non
comparables entre segments.

**Coverage boost** (fuzzy) : `score = coverage * 1000 + bm25`.
- Activé par défaut (`fuzzy_coverage_boost: bool`)
- `SuffixContainsScorer.coverage_boost: HashMap<DocId, f32>`
- Coverage = best (highest) coverage across all matches dans le doc
- Si pas de coverage (regex, d=0) : score = bm25 pur

## Tests et benchmarks

### Tests unitaires
```bash
cd packages/rag3db/extension/lucivy/ld-lucivy
cargo test --lib --release -q
# 1209 passed, 0 failed, 7 ignored
```

### Ground truth fuzzy
```bash
cargo test -p lucivy-core --test test_fuzzy_ground_truth --release -- --nocapture > /tmp/gt.txt 2>&1
# 429/429 highlights valides, recall 82/82
```

### Monotonie fuzzy
```bash
git clone --depth 1 https://github.com/L-Defraiteur/rag3db.git /tmp/test_rag3db_clone
RAG3DB_ROOT=/tmp/test_rag3db_clone cargo test -p lucivy-core \
  --test test_fuzzy_monotonicity --release -- --nocapture > /tmp/mono.txt 2>&1
# 9/9 real repo OK, 50/50 SKU OK, 20/20 API keys OK
```

### Playground repro (reproduit le flow WASM exactement)
```bash
RAG3DB_ROOT=/tmp/test_rag3db_clone cargo test -p lucivy-core \
  --test test_playground_repro --release -- --nocapture > /tmp/repro.txt 2>&1
```

### Bench sharding (90K docs Linux kernel)
```bash
# Index persistés :
# /home/luciedefraiteur/lucivy_bench_sharding/single/ (1 shard)
# /home/luciedefraiteur/lucivy_bench_sharding/round_robin/ (4 shards)
cargo test -p lucivy-core --test bench_sharding --release -- --nocapture > /tmp/bench.txt 2>&1
```

### Build rag3db + extension
```bash
cd packages/rag3db/build/release
cmake ../.. -DCMAKE_BUILD_TYPE=Release -DBUILD_EXTENSION_TESTS=TRUE \
  -DBUILD_EXTENSIONS="lucivy_fts" -DBUILD_SHELL=FALSE -DBUILD_TESTS=FALSE
cmake --build . --target lucivy_fts_test -j$(nproc)
```

### Build WASM emscripten
```bash
cd packages/rag3db/build/wasm
source ~/emsdk/emsdk_env.sh
emcmake cmake ../.. -DCMAKE_BUILD_TYPE=Release -DBUILD_WASM=TRUE \
  -DBUILD_SHELL=FALSE -DBUILD_TESTS=FALSE -DBUILD_BENCHMARK=FALSE
emmake cmake --build . -j$(nproc)
# Fichiers produits : rag3db_wasm.js, rag3db_wasm.wasm
# lucivy_fts statiquement linké (pas d'extension dynamique en WASM)
```

### Tests E2E rag3weaver
```bash
cd packages/rag3db/extension/rag3weaver
./run_e2e.sh --test e2e_idempotent_registration  # 21 tests
./run_e2e.sh --test e2e_search                    # search tests
./run_e2e.sh --summary                            # tous les tests
```

### Build Rust ld-lucivy
```bash
cd packages/rag3db/extension/lucivy/ld-lucivy
cargo test --lib  # tests unitaires
cargo test -p lucivy-core  # tests integration
```

**IMPORTANT** : toujours rediriger la sortie vers un fichier
(`> /tmp/output.txt 2>&1`), jamais `| tail`. Les bench outputs peuvent
être très longs et le pipe cause des problèmes.

## Bugs connus et pièges

### cmake ne détecte pas les changements Rust
`add_custom_command` sans `DEPENDS` → build Rust manuellement après
modification de code Rust.

### miniconda LD_LIBRARY_PATH
Pollue avec vieux libstdc++ → forcer `/usr/lib/x86_64-linux-gnu`.

### WASM cxx exceptions
build.rs doit ajouter `-fexceptions -sDISABLE_EXCEPTION_CATCHING=0`.

### WASM atomics
Nightly + `-Z build-std` + `+atomics,+bulk-memory` + `-C panic=abort`.

### magicBytes buffer
MAGIC_BYTES="RAG3DB" (6 chars) mais buffer était [4] → stack smashing.

### getDatabasePath()
Retourne le fichier DB, pas le répertoire → utiliser `parent_path()`.

### rag3db ~Database()
Ne cascade pas la destruction des index d'extensions → `CLOSE_LUCIVY_INDEX`.

### Lock files
`find ... -name "*.lock" -delete` avant réouverture d'un index persisté.

### Test repo /tmp
Le repo cloné dans `/tmp/test_rag3db_clone` est nettoyé par le système après
quelques jours. Re-cloner avant les tests si nécessaire.

## Timings de référence (segment principal ~4000 docs, 11 avril 2026)

| Query | d=0 | d=1 (nouveau pipeline) |
|-------|-----|----------------------|
| rag3weaver | ~7ms | 47ms |
| rak3weaver | 0 docs | 35ms |
| 3db | ~5ms | 42ms |
| 3db_val | ~3ms | 143ms |
| rag3db_value_destroy | ~3ms | 242ms |
| alue_dest | ~3ms | 104ms |
| query_result_is_success | ~3ms | 139ms |
| API keys (30-45 chars) | <1ms | <1ms |

Ancien pipeline DFA : 877ms pour "3db_val" d=1.

## Fichiers clés

### Fuzzy contains (nouveau)
- `src/query/phrase_query/fuzzy_contains.rs` — pipeline complet

### Literal pipeline (briques réutilisables)
- `src/query/phrase_query/literal_pipeline.rs` — fst_candidates, resolve_candidates, cross_token_falling_walk, resolve_chains

### Regex continuation (DFA-based, pour regex et d=0)
- `src/query/phrase_query/regex_continuation_query.rs` — RegexContinuationQuery, CachedPrescanResult, run_fuzzy_prescan, run_regex_prescan

### Suffix contains (exact contains, scoring)
- `src/query/phrase_query/suffix_contains_query.rs` — SuffixContainsQuery, SuffixContainsScorer (avec coverage_boost)

### Literal resolve (intersection, validation)
- `src/query/phrase_query/literal_resolve.rs` — LiteralMatch, intersect_trigrams_with_threshold, validate_path

### SFX structures
- `src/suffix_fst/file.rs` — SfxFileReader, falling_walk, fuzzy_falling_walk
- `src/suffix_fst/sibling_table.rs` — SiblingTableReader, siblings(), contiguous_siblings()
- `src/suffix_fst/posmap.rs` — PosMapReader
- `src/suffix_fst/gapmap.rs` — GapMapReader, is_value_boundary()
- `src/suffix_fst/termtexts.rs` — TermTextsReader
- `src/suffix_fst/bytemap.rs` — ByteBitmapReader

### Core handle
- `lucivy_core/src/handle.rs` — LucivyHandle, writer Mutex<Option<IndexWriter>>
- `lucivy_core/src/search_dag.rs` — DAG nodes, prescan, merge

### Tests
- `lucivy_core/tests/test_fuzzy_monotonicity.rs` — monotonie real repo + SKU + API keys + ranking
- `lucivy_core/tests/test_fuzzy_ground_truth.rs` — recall + highlights validation
- `lucivy_core/tests/test_playground_repro.rs` — WASM flow reproduction

## Branches

- `feature/fuzzy-via-literal-resolve` — branche active (fuzzy contains rewrite)
- `feature/acid-postgres-tests` — BM25 fixes
- `feature/optional-sfx` — sfx:false mode

## Historique des sessions documentées

- `docs/22-mars-2026-12h58/` — sessions 01-06
- `docs/24-mars-2026-20h35/` — sessions 01-09, knowledge dump complet
- `docs/28-fevrier-2026-11h31/` — session initiale
- `docs/29-mars-2026-18h28/` — sessions 01-08, knowledge dump
- `docs/1-avril-16h46/` — audit merge/query/registry
- `docs/2-avril-2026-12h43/` — sessions 01-10, knowledge dump
- `docs/4-avril-2026-11h57/` — sessions 01-07, knowledge dump, plans 05-06
- `docs/11-avril-2026-11h53/` — plan fuzzy rewrite, recap session, CE knowledge dump

## Préférences utilisateur

- Ne PAS mentionner Claude dans les commits (pas de Co-Authored-By)
- lucivy est sa propre lib — ne jamais dire "fork de Tantivy"
- Toujours rediriger bench output vers un fichier
- Pas de concessions : corriger les bugs, pas les rationaliser
- Docs en français, code et commentaires en anglais
- Pas de "plus simple" → "le plus correct"
