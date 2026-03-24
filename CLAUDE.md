# ld-lucivy — Contexte projet

## Architecture

Fork avancé de Tantivy (moteur full-text search Rust). Trois couches :

- **ld-lucivy** : moteur core (index, query, scoring, merger, segments)
- **lucivy_core** : handle unifié (`LucivyHandle`), query builder, tokenizers, snapshot, blob store
- **Bindings** (6 crates) :
  - CXX bridge rag3db : `lucivy_fts/rust/src/bridge.rs`
  - WASM emscripten : `bindings/emscripten/src/lib.rs` (extern "C" + SharedArrayBuffer)
  - WASM wasm-bindgen : `bindings/wasm/src/lib.rs` (wasm_bindgen + MemoryDirectory)
  - Node.js napi : `bindings/nodejs/src/lib.rs` (napi-rs)
  - Python PyO3 : `bindings/python/src/lib.rs` (pyo3)
  - C++ standalone : `bindings/cpp/src/lib.rs` (cxx bridge namespace lucivy)

## Extension rag3db (lucivy_fts)

Le code C++ de l'extension rag3db est dans **deux endroits** :
- `lucivy_fts/rust/src/bridge.rs` — bridge CXX Rust (dans ce repo)
- `../../lucivy_fts/` — code C++ de l'extension (repo séparé, hors de ce repo git)

## Champs internes

Chaque champ `text` :
- Sans stemmer : utilise RAW_TOKENIZER (lowercase + split). Un seul champ dans le schema.
- Avec stemmer : utilise STEMMED_TOKENIZER. Un seul champ dans le schema.
- Le suffix FST (.sfx) est construit par le SfxCollector pendant l'écriture du segment, qui fait du double tokenization en RAW_TOKENIZER indépendamment du tokenizer principal.
- PAS de champs `._raw` ou `._ngram` séparés dans le schema lucivy core.
- Les bindings CXX (rag3db) PEUVENT ajouter `._raw` et `._ngram` comme champs séparés pour le bridge — c'est spécifique au binding, pas au core.

## BlobStore + BlobDirectory (nouveau, non committé)

Fichiers dans `lucivy_core/src/` :
- `blob_store.rs` — trait `BlobStore` (load/save/delete/exists/list) + `MemBlobStore` pour tests
- `blob_directory.rs` — `BlobDirectory<S: BlobStore>` implémente le trait `Directory` de tantivy

Pattern "DB stocke, mmap sert" : le BlobStore est la source de vérité durable, le cache local temp est utilisé pour les lectures mmap zero-copy. Au drop, le cache est nettoyé (ref-counted via Arc).

Implémentations externes prévues : `CypherBlobStore` (rag3db), `PostgresBlobStore`, `S3BlobStore`.

## LucivyHandle::close()

Fichier : `lucivy_core/src/handle.rs`. Le writer est `Mutex<Option<IndexWriter>>`. `close()` fait `guard.take()` pour dropper le writer explicitement et libérer le flock. Après close, les écritures retournent `Err("index is closed")`, les lectures continuent.

Nécessaire car le destructeur C++ de rag3db (`~Database()`) ne cascade pas la destruction des index d'extensions — le `LucivyHandle` n'est jamais droppé implicitement.

## Bindings — état actuel et mises à jour nécessaires

| Binding | close() | Mutex\<Option\> adapté | Blob store | startsWith_split |
|---------|---------|----------------------|------------|-----------------|
| CXX bridge rag3db | exposé (`close_index`) | oui | non exposé (StdFsDirectory en dur) | non |
| WASM emscripten | **manquant** (seulement `lucivy_destroy`) | **NON** (accède `writer` sans `.as_mut()`) | non (MemoryDirectory) | oui |
| WASM wasm-bindgen | **manquant** | oui (via `Option`) | non (MemoryDirectory) | non |
| Node.js napi | **manquant** | **NON** (accède `writer` sans `.as_mut()`) | non (StdFsDirectory) | non |
| Python PyO3 | **manquant** | **NON** (accède `writer` sans `.as_mut()`) | non (StdFsDirectory) | non |
| C++ standalone | **manquant** | **NON** (accède `writer` sans `.as_mut()`) | non (StdFsDirectory) | non |

### Actions prioritaires
1. **Emscripten, Node.js, Python, C++ standalone** : adapter tous les accès writer pour `Option` (`.as_mut().ok_or("index is closed")?`) — sinon crash à l'exécution si `close()` est appelé
2. **Tous sauf CXX bridge** : exposer `close()` qui appelle `handle.close()`
3. **Tous** : blob store non exposé — à décider si nécessaire par binding

### ngram/raw pairs — OK partout
Tous les 6 bindings passent correctement `handle.raw_field_pairs` et `handle.ngram_field_pairs` à `build_query()` et auto-dupliquent les textes à l'insertion.

## Features clés (au-dessus de Tantivy)

### Query types exposés via build_query()
- **term** : exact token match, term dict standard (0.2ms)
- **phrase** : multi-token exact sequence (1-5ms, 2.5x faster than tantivy on 4-shard)
- **fuzzy** : Levenshtein on term dict, standard tantivy behavior (2x faster on 4-shard)
- **regex** : regex on term dict, standard tantivy behavior
- **parse** : QueryParser ("mutex AND lock", "return error")
- **contains** : substring search via SFX (lucivy exclusive, ~700ms on 90K)
- **startsWith** : prefix search via SFX (lucivy exclusive)
- **contains_split** / **startsWith_split** : whitespace split → boolean should
- **phrase_prefix** : autocomplétion "mutex loc" → "mutex lock" (1ms)
- **disjunction_max** : max score among sub-queries + tie_breaker
- **more_like_this** : find similar docs by reference text (0.7ms)
- **boolean** : must/should/must_not sub-queries

Note: `fuzzy` et `regex` top-level = tantivy behavior (term dict).
Cross-token fuzzy/regex = `contains` + distance/regex params (SFX).

### SFX optionnel (`sfx: false`)
SchemaConfig accepte `sfx: false` pour skip SFX build.
- IndexSettings.sfx_enabled persisté dans meta.json
- SegmentWriter skip SfxCollector, merger skip SFX merge naturellement
- contains/startsWith retournent erreur explicite
- Réduit taille index ~3-5x, indexation plus rapide

### BM25 scoring — Arc<dyn Bm25StatisticsProvider>
`EnableScoring::Enabled` porte un `Arc<dyn Bm25StatisticsProvider + Send + Sync>`.
L'Arc est stockable dans les Weights, partageable across threads.
- TermQuery, PhraseQuery : utilisent stats via EnableScoring (natif)
- FuzzyTermQuery, RegexQuery, TermSetQuery : AutomatonWeight.stats (Arc)
  + global_doc_freq() per matched term
- MoreLikeThisQuery : stats_provider param for doc_freq/num_docs
- SuffixContainsQuery : prescan global_doc_freq (séparé)
- Score consistency : 5/5 single vs 4-shard (diff=0.0000)

### TermQuery — PAS de SFX
TermQuery n'utilise plus sfxpost ni SFX fallback. Le term dict standard suffit.
Highlights via WithFreqsAndPositionsAndOffsets du posting list standard.

### AutomatonWeight — SFX conditionnel
`collect_term_infos()` conditionné par `prefer_sfxpost`:
- false (fuzzy/regex top-level) → term dict standard (rapide)
- true (contains+regex) → SFX (nécessaire pour suffixes)
`collect_term_infos` retourne `Vec<(Vec<u8>, TermInfo)>` pour global doc_freq lookup.

### Search DAG conditionnel (BranchNode)
```
drain → flush → needs_prescan?
                  ├── then → prescan_0..N ∥ → merge_prescan ─→ build_weight → search_0..N ∥ → merge
                  └── else ──────────────────────────────────→ build_weight
```
Query construite AVANT le DAG (pas de DFA compilation dans le DAG).
BranchNode skip prescan pour term/phrase/fuzzy/regex.

### luciole — framework DAG/Actor (crate séparé dans luciole/)
Framework complet de threading :
- **Actor** : trait `Actor<Msg=MyEnum>`, `Pool<M>`, `Scope`, `DrainMsg`
- **DAG** : `Dag`, `Node`, `PollNode`, `execute_dag()`, `DagResult::take_output()`
- **Flow control** : `SwitchNode` (N-way), `BranchNode` (2-way fn alias), `GateNode` (pass/block)
- **Fan-out** : `MergeNode`, `Dag::fan_out_merge()`, `ScatterDAG`
- **Streaming** : `StreamDag` (pipeline topology + topo drain) — validé en prod
- **Services** : `ServiceRegistry` dans `NodeContext`, `Dag::with_services(Arc)`
- **Undo** : `can_undo()`, `undo()`, `undo_context()` sur Node + rollback dans runtime
- **Config** : `node_config()` pour checkpoint recovery
- **Observabilité** : `subscribe_dag_events()`, `TapRegistry`, `CheckpointStore`
- **Scheduler** : pool de threads persistants, WASM compatible
- **Utilitaire** : `add_node_boxed()` pour Box<dyn Node>

Note: `BranchNode` est une FONCTION pas un struct : `BranchNode(|| cond)` pas `BranchNode::new()`

### DiagBus
`src/diag.rs` — event bus zero-cost (atomic fast-path).
- `SearchMatch` + `SearchComplete` câblés dans `run_sfx_walk` (via segment_id param)
- `TokenCaptured` câblé dans segment_writer
- `SfxWalk`, `SfxResolve`, `SuffixAdded`, `MergeDocRemapped` : pas encore câblés

### Merger — offsets préservés
Fix critique : le merger écrivait les postings sans offsets (write_doc au lieu de write_doc_with_offsets), causant un panic avec highlights sur segments mergés. Fichier : `src/indexer/merger.rs`.

### WASM commit thread
Commit déplacé sur un pthread dédié pour contourner la limite ASYNCIFY stack. Status communiqué via SharedArrayBuffer + Atomics polling (pas de ccall). Ring buffer SAB pour logs temps réel côté JS.

### UTF-8 char boundary fix
Les NGramContainsQuery paniquaient sur les caractères multi-byte (accents, symboles). Fix : `floor_char_boundary()` / `ceil_char_boundary()` dans `src/query/phrase_query/ngram_contains_query.rs`.

## Tests

- `cargo test --lib` dans ld-lucivy : 1155 tests
- Bench sharding : `bench_sharding.rs` (90K docs Linux kernel)
  - `bench_query_times` : timing rapide toutes queries
  - `ground_truth_exhaustive` : 37/37 checks (7 termes × 4 variantes + 2 splits)
  - `test_score_consistency` : single vs 4-shard (5/5 diff=0.0000)
  - `test_sfx_disabled` : sfx:false mode (toutes queries + erreur contains)
  - `profile_regex_automaton_weight` : timing per-node AutomatonWeight
- Bench vs tantivy : `bench_vs_tantivy.rs` (tantivy 0.25 dev-dependency)
- IMPORTANT : toujours `> /tmp/fichier.txt 2>&1`, JAMAIS `| tail`
- Lock files : `find ... -name "*.lock" -delete` avant réouverture

Index persistés :
- `/home/luciedefraiteur/lucivy_bench_sharding/single/` (90K, 1 shard)
- `/home/luciedefraiteur/lucivy_bench_sharding/round_robin/` (90K, 4 shards)
- `/home/luciedefraiteur/lucivy_bench_vs_tantivy/tantivy/` (90K, tantivy 0.25)

## Docs

Les docs sont dans `docs/` organisés par dossier horodaté.
- `24-mars-2026-20h35/08-knowledge-dump-session-complete.md` — KNOWLEDGE DUMP COMPLET
- `24-mars-2026-20h35/` — docs de la session courante (01-08)
- `22-mars-2026-12h58/` — session précédente (01-06)

## Style

- Ne pas mentionner Claude dans les docs ou le code
- Docs en français
- Code et commentaires en anglais
