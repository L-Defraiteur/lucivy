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

Chaque champ `text` génère un triple :
- `{name}` : tokenisé (stemmed si configuré)
- `{name}._raw` : lowercase only (precision pour term/fuzzy/regex/contains)
- `{name}._ngram` : trigrams (candidats rapides pour contains)

Les champs `string` ont aussi un `._ngram`.
Les bindings doivent auto-dupliquer les valeurs texte vers `_raw` et `_ngram` à l'insertion.

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
- **contains** : recherche substring via NGramContainsQuery. Cascade : trigram candidats (._ngram) → vérification stored text (._raw). Requiert les champs `._ngram` et `._raw`.
- **contains_split** : split par whitespace → boolean should de contains. Expansion faite dans chaque binding.
- **startsWith** : prefix search via FST natif (AutomatonPhraseQuery). Dernier token = prefix (range FST puis prefix fuzzy DFA), tokens précédents = exact/fuzzy. Plus rapide que contains car pas de trigrams. Routing : single-token → FuzzyTermQuery::new_prefix, multi-token → AutomatonPhraseQuery::new_starts_with().
- **startsWith_split** : même split que contains_split mais avec startsWith.
- **fuzzy, regex, term, parse** : types standard tantivy exposés.

### BM25 scoring pour AutomatonWeight
`AutomatonWeight` (utilisé par startsWith, fuzzy, regex) supporte maintenant le BM25 scoring opt-in via `with_scoring(bool)`. 3 paths dans `scorer()` :
- `highlight_sink` présent → WithFreqsAndPositionsAndOffsets + AutomatonScorer (le plus lent)
- `scoring_enabled = true` → WithFreqs + AutomatonScorer (BM25)
- `scoring_enabled = false` → Basic + ConstScorer (score = boost, fast path par défaut)

Activé automatiquement via `EnableScoring::is_scoring_enabled()` dans `Query::weight()`. Propagé dans FuzzyTermQuery, RegexQuery, TermSetQuery.

### Architecture Actor + Scheduler global
Remplacement des 6 patterns de threading (crossbeam, rayon, mpsc, atomic polling...) par un framework d'acteurs unifié :
- **Fichiers** : `src/actor/` (mod.rs, mailbox.rs, reply.rs, events.rs, scheduler.rs)
- **Acteurs** : trait `Actor` avec `handle()` + `priority()`, `Mailbox<M>` FIFO, `ActorRef` typé, `Reply/ReplyReceiver` oneshot
- **Scheduler global** : `global_scheduler()` lazily initialized, partagé entre tous les IndexWriters (comme rayon global pool). Configurable via `LUCIVY_SCHEDULER_THREADS`.
- **EventBus** : broadcast d'événements (MessageHandled, PriorityChanged, ActorIdle, ActorWoken, ThreadParked/Unparked, ActorStopped, ActorSpawned, MessageSent*, BatchStarted) pour observabilité
- **WASM compatible** : 1 thread coopératif (wait_cooperative avec condvar.wait_timeout 1ms) ou N threads natifs (wait_blocking)
- **"Take pattern"** : `slot.actor.take()` temporaire pendant handle_batch() pour éviter deadlock réentrant, puis remise en place avec `slot.actor = Some(actor_box)`

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
