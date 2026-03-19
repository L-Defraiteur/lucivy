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

## Features clés (ajouts récents au-dessus de Tantivy)

### Query types
- **contains** : recherche substring via SuffixContainsQuery. Utilise le suffix FST (.sfx) pour trouver tous les termes contenant le substring, puis le sfxpost pour les positions exactes. PAS de ngrams — le suffix FST gère tout. Cherche sur le champ RAW (lowercase only, pas stemmé).
- **contains_split** : split par whitespace → boolean should de contains. Expansion faite dans chaque binding.
- **startsWith** : prefix search via FST natif (AutomatonPhraseQuery). Dernier token = prefix (range FST puis prefix fuzzy DFA), tokens précédents = exact/fuzzy. Routing : single-token → FuzzyTermQuery::new_prefix, multi-token → AutomatonPhraseQuery::new_starts_with().
- **startsWith_split** : même split que contains_split mais avec startsWith.
- **fuzzy, regex, term, parse** : types standard lucivy exposés.

### Suffix FST (.sfx) — moteur du contains
Le suffix FST est le mécanisme central pour les recherches substring. Pas de ngrams.

**Construction** (segment_writer / merge) :
- Pour chaque terme du term dict, génère TOUS les suffixes (offsets 0 à len)
- Chaque suffixe est stocké avec `(raw_ordinal, si)` : ordinal du terme parent + offset
- Partitionné en SI=0 (début de mot = startsWith) et SI>0 (substring)
- Ex: "function" → suffixes: "function"(SI=0), "unction"(SI=1), "nction"(SI=2), etc.

**Recherche** (suffix_contains_query.rs) :
- `prefix_walk(query)` sur les deux partitions du FST
- Pour chaque match : resolve `raw_ordinal` → posting entries (doc_id, token_index, byte_from, byte_to)
- Byte offsets ajustés : `byte_from + si` pour pointer dans le texte original

**Fichiers** :
- `.sfx` : suffix FST + parent list + gapmap (par champ, par segment)
- `.sfxpost` : posting entries indexées par ordinal (doc_id, token_index, offsets)
- Pas de champs `._raw` ou `._ngram` séparés dans le schema
- Le SfxCollector fait du double tokenization dans le segment_writer (RAW_TOKENIZER)

**GapMap** : stocke les séparateurs inter-tokens par doc pour la reconstruction du texte original lors des highlights multi-token.

### BM25 scoring pour AutomatonWeight
`AutomatonWeight` (utilisé par startsWith, fuzzy, regex) supporte maintenant le BM25 scoring opt-in via `with_scoring(bool)`. 3 paths dans `scorer()` :
- `highlight_sink` présent → WithFreqsAndPositionsAndOffsets + AutomatonScorer (le plus lent)
- `scoring_enabled = true` → WithFreqs + AutomatonScorer (BM25)
- `scoring_enabled = false` → Basic + ConstScorer (score = boost, fast path par défaut)

Activé automatiquement via `EnableScoring::is_scoring_enabled()` dans `Query::weight()`. Propagé dans FuzzyTermQuery, RegexQuery, TermSetQuery.

### luciole — framework de coordination (crate séparé dans luciole/)
Framework complet de threading avec pool de threads persistants unifié :
- **Actor** : trait `Actor<Msg=MyEnum>`, typed messages, `Pool<M>`, `Scope`, `DrainMsg`
- **DAG** : `Dag`, `Node`, `PollNode`, `GraphNode`, `execute_dag()`, `DagResult::take_output()`
- **Observabilité** : `subscribe_dag_events()`, `TapRegistry`, `display_progress()`, `CheckpointStore`
- **Scheduler** : pool de threads persistants, `submit_task()`, `WorkItem` (Actor + Task unifié)
- **WASM** : tout compatible via `wait_cooperative` + `run_one_step()`
- **Commit** : DAG structurel (prepare → merges ∥ → finalize → save → gc → reload)
- **Search** : DAG (drain → flush → build_weight → search_shard_N ∥ → merge_results)
- **Merge sfx** : steps parallélisés (build_fst, copy_gapmap, merge_sfxpost via submit_task)

### Merger — offsets préservés
Fix critique : le merger écrivait les postings sans offsets (write_doc au lieu de write_doc_with_offsets), causant un panic avec highlights sur segments mergés. Fichier : `src/indexer/merger.rs`.

### WASM commit thread
Commit déplacé sur un pthread dédié pour contourner la limite ASYNCIFY stack. Status communiqué via SharedArrayBuffer + Atomics polling (pas de ccall). Ring buffer SAB pour logs temps réel côté JS.

### UTF-8 char boundary fix
Les NGramContainsQuery paniquaient sur les caractères multi-byte (accents, symboles). Fix : `floor_char_boundary()` / `ceil_char_boundary()` dans `src/query/phrase_query/ngram_contains_query.rs`.

## Tests

- `cargo test` dans ld-lucivy : 1113+ tests
- `cargo test` dans lucivy_core : 71+ tests (dont blob_directory 7 tests, blob_store 3 tests)
- Les tests blob_directory utilisent `MemBlobStore` et vérifient create/search, close/reopen, WORM, isolation multi-index, survie après cleanup cache, multiple commits

## Docs

Les docs sont dans `docs/` organisés par dossier horodaté. Le plus récent : `13-mars-2026-16h47/`.
Doc clé : `12-mars-2026-12h28/17-investigation-lock-file-et-architecture-blob-store.md`.

## Style

- Ne pas mentionner Claude dans les docs ou le code
- Docs en français
- Code et commentaires en anglais
