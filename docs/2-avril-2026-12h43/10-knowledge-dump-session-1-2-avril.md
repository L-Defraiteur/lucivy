# 10 — Knowledge dump session 1-2 avril 2026

## Ce qui a été fait

### Highlights fuzzy byte-exact
- Fix highlight trop larges (40/286 FAIL → 0) via content_byte_starts table
- Propagation `si` dans LiteralMatch → `token_start = first_bf - first_si`
- Fix highlight proven : `hl_end = last_bt + remaining` au lieu de `query_len + distance`
- Fix highlight suffix : ne PAS soustraire `first_si` dans hl_start (first_bf est déjà le suffix start)
- Suppression merge agressif (gap ≤ 1 byte) → dedup strict

### Multi-token d=0 pipeline unification
- `cross_token_resolve_for_multi` remplacé par `resolve_token_for_multi` (literal_pipeline.rs)
- "use rag3weaver" : 4 → 8 résultats (= ground truth)
- Même briques FST + falling walk que le single-token contains

### Registry SFX unifié
- Suppression dead code : merge_sfx_legacy + merge_sfx_deferred (-505 lignes)
- Trait unifié `SfxIndexFile` avec `MergeStrategy` : EventDriven / OrMergeWithRemap / ExternalDagNode
- SepMap : `prebuilt_by_collector` + OR-merge au merge (fix 32s → 0ms)
- FreqMap : nouvel index dérivé (doc_freq + term_freq pour futur BM25 via SFX)
- Single-pass `build_derived_indexes()` pour segment initial ET merge
- `OrMergeNode` générique remplace MergeSiblingLinksNode

### DAG segment initial
- `SfxCollector::build()` → `into_data()` + DAG (BuildFstNode ∥ BuildSfxPostNode ∥ BuildPrebuiltNode → AssembleSfxNode)
- Même architecture que le merge DAG

### startsWith → contains
- `ContinuationMode` enum supprimé → `anchor_start: bool`
- `prefix_only` renommé `anchor_start` partout

### Perf fuzzy 30×
- HashMap `bf_to_pos: (doc_id, byte_from) → position` pré-construit
- "rag3db" d=1 : 2035ms → 68ms

### LiteralMatch enrichi (Plan A step 1)
- `ordinal: u32` ajouté à LiteralMatch et SuffixContainsMatch
- Propagé depuis FstCandidate.raw_ordinal et parent.raw_ordinal
- Fondation pour éliminer les lookups posmap/termtexts dans le DFA walk

## Scripts de test

### test_fuzzy_ground_truth
```bash
cargo test -p lucivy-core --test test_fuzzy_ground_truth -- --nocapture
```
- Indexe les fichiers du repo ld-lucivy (903 fichiers)
- Ground truth brute-force : tokenize CamelCaseSplit + lowercase, concat, semi-global Levenshtein
- Teste "rag3weaver" d=1 et "rak3weaver" d=1
- Vérifie recall, precision, ET chaque highlight (stripped dist ≤ d)
- 296/296 + 260/260 highlights valid

### test_merge_contains
```bash
cargo test -p lucivy-core --test test_merge_contains -- --nocapture
```
- Indexe les fichiers du repo ld-lucivy
- Compare WITH merge (drain_merges) vs WITHOUT merge
- Multi-token d=0 queries vs brute-force ground truth
- Miss analysis détaillée pour "use rag3weaver"

### test_luce_roundtrip
```bash
cargo test -p lucivy-core --test test_luce_roundtrip -- --nocapture
```
- Importe le playground .luce snapshot
- Teste contains exact, fuzzy d=1, regex
- Multi-token d=0 et d=1

### test_playground_repro (LE PLUS IMPORTANT)
```bash
# Clone le repo rag3db si pas déjà fait
git clone --depth 1 https://github.com/L-Defraiteur/rag3db.git /tmp/test_rag3db_clone

# Lancer le test (release mode recommandé pour 4300 fichiers)
RAG3DB_ROOT=/tmp/test_rag3db_clone cargo test -p lucivy-core --test test_playground_repro --release -- --nocapture
```
- **Reproduit exactement** le flow du playground WASM :
  - Même TEXT_EXTENSIONS que le playground JS
  - Même isBinaryContent check (null byte dans premiers 512 bytes)
  - Même MAX_FILE_SIZE = 100KB
  - Même COMMIT_EVERY = 200
  - Pas de drain_merges
  - Skip symlinks (le tarball GitHub ne les suit pas)
  - Fields : path (text, stored, not indexed) + content (text, stored, indexed)
- Utilise `RAG3DB_ROOT` env var ou clone automatiquement
- Clone dans `/tmp/test_rag3db_clone`
- 4307 fichiers ≈ 4308 du playground
- Teste : "rag3weaver" d=0, "rag3weaver" d=1, "rak3weaver" d=1, "rag3db" d=1
- Affiche highlights avec contexte et vérifie stripped distance

### Attention au symlink
Le repo rag3db a un symlink `tools/rust_api/rag3db-src → ../../` qui crée
une boucle infinie si suivi. Le test skip les symlinks (comme le tarball GitHub).

## Build WASM emscripten

```bash
cd packages/rag3db/extension/lucivy/ld-lucivy
bash bindings/emscripten/build.sh
```

- Compile en release pour wasm32-unknown-emscripten
- Copie automatiquement dans `playground/pkg/`
- PTHREAD_POOL_SIZE=8, scheduler_threads=4
- Le serveur playground : `node playground/serve.mjs` → http://localhost:9877/
- Le serveur sert les fichiers du disque, donc un rebuild + reload suffit

## Build .luce (snapshot pré-construit)

Le .luce dans `playground/dataset.luce` est construit via le binding Python :
```python
import lucivy
index = lucivy.Index.create("/tmp/luce_build", [
    {"name": "content", "type": "text", "stored": True}
])
# ... add documents ...
index.commit()
index.drain_merges()  # Important : merge tous les segments
data = index.export_snapshot()
open("playground/dataset.luce", "wb").write(data)
```

Le .luce pré-construit a 1 segment (drain_merges), le playground WASM a 7+ segments.

## Fichiers de timing/diag

Les logs `[fuzzy-timing]`, `[derive-timing]`, `[hl-diag]` sont dans le code Rust
(eprintln). Ils apparaissent dans la console Chrome pour le WASM et dans stderr
pour les tests natifs.

- `[fuzzy-timing]` : breakdown fst/resolve/intersect/dfa + proven/unproven/skipped
- `[derive-timing]` : temps de chaque index dérivé dans build_derived_indexes
- `[hl-diag]` : détail du highlight mapping (token spans, content bytes, etc.)

## État actuel des fichiers modifiés

### Fichiers principaux modifiés cette session
- `src/query/phrase_query/regex_continuation_query.rs` — fuzzy highlights, HashMap bf_to_pos, timing
- `src/query/phrase_query/literal_resolve.rs` — si, token_len, ordinal, last_tri_idx
- `src/query/phrase_query/literal_pipeline.rs` — resolve_token_for_multi, ordinal
- `src/query/phrase_query/suffix_contains.rs` — anchor_start, ordinal, pipeline unification
- `src/query/phrase_query/suffix_contains_query.rs` — anchor_start
- `src/suffix_fst/index_registry.rs` — MergeStrategy, OrMergeNode, build_derived_indexes
- `src/suffix_fst/collector.rs` — into_data(), sepmap prebuilt, DAG
- `src/suffix_fst/freqmap.rs` — NOUVEAU (FreqMap index)
- `src/suffix_fst/sepmap.rs` — OrMergeWithRemap, merge_from_sources
- `src/suffix_fst/sibling_table.rs` — OrMergeWithRemap, merge_from_sources
- `src/indexer/sfx_dag.rs` — OrMergeNode, AssembleSfxNode, build_initial_sfx_dag
- `src/indexer/merger.rs` — dead code cleanup (-505 lignes)

### Tests ajoutés
- `lucivy_core/tests/test_fuzzy_ground_truth.rs`
- `lucivy_core/tests/test_playground_repro.rs`

### Docs ajoutés (docs/1-avril-16h46/ et docs/2-avril-2026-12h43/)
- 01-audit-merge-query-registry.md
- 02-chemins-indexation-complet.md
- 03-unification-starts-with-contains.md
- 04-plan-freqmap-index-bm25-via-sfx.md
- 05-todo-verif-wasm.md
- 06-plan-registry-v2-merge-strategy.md
- 07-fuzzy-perf-analysis-rag3db.md
- 08-audit-fuzzy-data-flow-redundancies.md
- 09-plans-implementation-optim-fuzzy.md
- 10-knowledge-dump-session-1-2-avril.md (ce fichier)

## Prochaines étapes

### Plan A step 2 : utiliser ordinal dans le DFA walk
L'ordinal est propagé. Reste à l'utiliser pour construire le concat
sans posmap lookups (ord_to_term cache une seule fois par segment).

### Plan B : grouper candidats par doc_id
Un seul concat par doc, valider tous les candidats dessus.

### Plan C-E : trivials
- Gaps dans token_spans (évite relecture gapmap)
- query_text.contains(' ') precompute
- token_spans pos → idx index

### Virer le term dict
- FreqMap fournit doc_freq + term_freq
- Queries term/phrase pourraient passer par SFX
- Le term dict est redondant et incompatible avec CamelCaseSplit

### Fuzzy multi-token
- Distance par segment alphanum vs globale (design à trancher)
- Threshold trigram trop permissif pour queries courtes (bigrams)

### Highlights WASM
- Tester sur le repo rag3db indexé dans le playground
- Le fix "librag3weaver" highlight est poussé
- Vérifier qu'il n'y a plus de highlights faux
