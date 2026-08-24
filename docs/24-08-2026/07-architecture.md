# Architecture — tout ce qu'il faut savoir, état du 24 août 2026 au soir

Autonome. Le knowledge dump algorithmique du moteur v3 reste
`docs/22-aout-2026-19h47/09` ; les tests et benchs sont dans
`08-knowledge-dump-tests-benchs.md`.

## 1. Le workspace

```
lucivy/                       (Cargo workspace, branche v3-recovery)
├── src/                      ld-lucivy : le moteur (fork tantivy) + SFX v3        MIT
├── lucivy_core/              handles, compat de requêtes, DAG de recherche, ACID  MIT
├── luciole/                  acteurs + scheduler + DAG, WASM-safe                 MIT
├── lucistore/                persistance partagée : BlobStore, ShardStorage,      MIT
│                             ShardRouter, snapshot/delta/delta_sharded, sync
├── sparse_vector/            index sparse (WAND) sur lucistore, shardé via        MIT
│                             luciole — crate ami, code original (NOTICE)
├── bindings/{python,nodejs,cpp,emscripten}/
├── lucivy_fts/rust/          pont cxx de l'ancienne extension rag3db (obsolète chez eux)
├── playground/               démo navigateur (wasm), dataset.luce v2
└── docs/JJ-MM-AAAA/          docs datées ; BENCHMARKS.md à la racine de docs/
```

Dépendances : `lucivy_core → ld-lucivy, luciole, lucistore` ;
`sparse_vector → lucistore, luciole` (pas de dépendance à ld-lucivy ni
lucivy_core) ; `lucistore` ne dépend que de serde/serde_json ; `luciole` de
rien du workspace. Le tout est publié séparément sur crates.io (2.1.0 à
faire).

## 2. luciole — acteurs, scheduler, DAG

- **Scheduler global** (`global_scheduler()`) : pool de threads persistants,
  acteurs à priorités (Idle → Critical), tâches (`submit_task`,
  `task_pipe_to`). Un seul par process, partagé par tous les handles.
- **Acteurs** : trait `Actor { type Msg; handle(msg, ctx) -> ActorStatus }`
  (`Continue`/`Yield`/`Suspend`/`Stop`). `Pool<M>` = N acteurs identiques :
  `send` (round-robin), `send_to(key)`, `broadcast`, `request`/`request_to`
  (réponse attendue), `scatter` (à tous), **`scatter_to(targets, msg_par_worker)`**
  (à un sous-ensemble), `drain`, `shutdown`. Depuis le 24 août : un worker
  parti ne fait plus paniquer `drain`/`shutdown`/`scatter` (ils tournent
  dans des destructeurs).
- **Reply** : oneshot `(Reply<T>, ReplyReceiver<T>)`. `Scheduler::wait` (panique
  si l'émetteur meurt) et **`try_wait`** (`Err(ReplyClosed)`) ; `request`
  passe par `try_wait`. Tout `Reply` lâché sans `send` **avertit sur stderr**
  (`LUCIOLE_REPLY_TRACE=1` = backtrace). `pipe_to` / `collect_replies_to` :
  request-reply sans thread bloqué (le résultat revient en message) —
  jamais de wait bloquant dans un handler (assertion fatale).
- **DAG** (`Dag`, `execute_dag`, `DagExecutor` async) : nœuds `Node`, ports
  typés, niveaux topologiques exécutés en parallèle. Depuis le 24 août les
  nœuds sont sortis de leur slot par `mem::replace` contre une sentinelle
  `TakenNode` et exécutés sous `catch_unwind` : une panique de nœud est une
  erreur de DAG, plus un double free. `BranchNode(|| cond)` est une fonction,
  pas une struct. `WaitGraph` : dump mermaid/texte des attentes ; warning
  automatique après 10 s d'attente avec l'état des threads et acteurs.
- **Règles WASM** : jamais de `thread::spawn`, tout par le scheduler ; I/O
  différée hors handlers.

## 3. lucistore — persistance partagée

- `BlobStore` trait (`load`, `save`, `delete`, `exists`, `list`, +
  `blob_len`/`load_range` à défaut `None`) ; `MemBlobStore` ; blanket
  `impl BlobStore for Arc<T>` (donc `Arc<dyn BlobStore>` direct).
- `ShardStorage` (lucistore) : chemins de shards + fichiers racine ;
  `FsShardStorage`, `BlobShardStorage<S>` (namespace `{name}/shard_i`).
  lucivy_core et sparse_vector ont chacun **leur** trait de storage qui
  construit leurs handles par shard au-dessus de ceux-là (à unifier, point 6
  du plan).
- `ShardRouter` (déplacé ici le 24 août) : `route(hachés) -> shard`
  (`balance_weight` 1.0 = round-robin, < 1 = co-localisation par jetons,
  `df_threshold`), `record_node_id` / `shard_for_node_id` / `remove_node_id`,
  `to_bytes`/`from_bytes`, `resync`. Utilisé par les deux index.
- `blob_cache::BlobCache` (cache local jetable, write-through),
  `snapshot` (LUCE), `delta` (LUCID), `delta_sharded` (LUCIDS : seuls les
  shards changés), `sync_server`, `version`.

## 4. ld-lucivy — le moteur

Fork tantivy : index, segments, merges (policy plafonnée à 10 000 docs par
segment en entrée et en sortie — un segment de 50k docs coûte 13× plus),
docstore, fast fields, BM25, collecteurs, `BooleanQuery`/union bufferisée.
Le writer : 8 indexeurs (acteurs), **dispatch collant par tranches de 64
documents** (un petit lot fait un segment par shard, un lot massif utilise
tous les indexeurs), finalisation de segment en tâche, `SegmentUpdater` acteur.

**SFX v3** (défaut depuis le 23 août ; `meta.json` sans `sfx_version` = v2) —
par champ texte et par segment, 8 sidecars : `.sfx` (FST, partitions 0x00
débuts de chunk / 0x01 suffixes / 0x02 mots dépouillés), `.sfxpost`,
`.termtexts` (textes + META + **STATS versionné** : plus long mot, layout 1),
`.posmap`, `.bytemap`, `.word_sfxpost`, `.word_pos_map`, `.sibling_v3`.
Tokenizer `equal_chunk` : mots (`is_content_char` = alphanumérique ASCII ou
tout non-ASCII) découpés en chunks avec séparateur attaché et overlap ;
**tous** les mots vont dans la partition 0x02 (correction du 24 août :
le dernier mot d'une valeur sans séparateur final en était absent).

Requêtes v3 (`src/suffix_fst/briques/`) : `contains` strict (chaînes de
chunks : falling walk + sibling DFS + « second token anchored » pour les
têtes courtes ; `build_chains_from_splits` explore **toutes** les branches —
une clé qui avale le reste n'exclut plus les formes découpées) ; relaxed
(pipeline mots, chaînes de chunks sautées si le segment n'a pas de mot long,
B2 bis) ; fuzzy (pièces/pivot/ngram → régions → fenêtre → `fuzzy_spans`) ;
regex (littéraux prouvés + fenêtre, sinon document entier). Tous vérifient
contre le texte reconstruit (`verify_literal`, `verify_boundaries`). Les
prescans rendent un `doc_tf` **trié par doc** (`CachedPrescan::new` l'asserte
en debug) : un `SfxScorer` est un `DocSet` monotone.

## 5. lucivy_core — handles et requêtes

- **`SchemaConfig`** (JSON, `deny_unknown_fields`, `validate()`) : `fields`
  (`text`/`string`/`u64`/`i64`/`f64`, `stored`/`indexed`/`fast`), `tokenizer`,
  `shards`, `balance_weight`, `df_threshold`, `sfx_version` (3). Réouverture
  tolérante (`from_stored_json`).
- **`QueryConfig`** (JSON) : `type` + `field` (singulier, sauf `parse`/`boolean`/
  `disjunction_max` qui lisent `fields`) + `value`. `contains` est la primitive
  (`strict_separators`, `distance`, `anchor_start`, `exact_match`, `regex`) ;
  alias v2 routés dessus (`term`, `fuzzy`, `regex`, `phrase`, `startsWith`,
  `*_split`, `phrase_prefix`) ; `parse` = OU de contains par mot×champ, ou —
  syntaxe booléenne (`AND`/`OR`/`NOT`, `+`/`-`, guillemets, parenthèses
  autonomes) — traduction en `boolean` de contains, **highlights dans les
  deux cas** ; `boolean` (`must`/`should`/`must_not`, `filters` =
  `{field, op, value}`) ; `disjunction_max` ; `more_like_this` (TF-IDF, pas
  SFX). `query_warnings(json)` dit ce qui va tourner (branche de parse,
  `fields` ignoré, regex sans littéral, fuzzy trop lâche, segments v2).
- **`LucivyHandle`** : un index ; `close()` = commit + drain des merges +
  libération ; `reopen_writer_after` pour les deltas.
- **`ShardedHandle`** : N `LucivyHandle` + acteurs (readers/tokenizers →
  routeur → shards) + `ShardRouter`. `add_document(doc, node_id)` estampille
  `_node_id` ; `add_document_json` ; `search` / `search_filtered(allowed_ids)`
  / `search_with_docs` ; **`node_ids_of(&results)`** (fast field, sans
  document) ; `shard_for_node_id` ; `delete_by_node_id` ; `commit` ; `close()`
  **rend le handle inerte** (drain, commit, arrêt des pools, drapeau
  `closed` : tout appel rend « handle is closed ») ; `drop_index()` ;
  `export/apply_sharded_delta` ; `export_stats`/`search_with_global_stats`
  (distribué, statistiques BM25 fusionnées). Recherche = DAG luciole :
  drain → flush → prescan par segment (parallèle, tous les shards) → poids
  unique avec stats globales → **un nœud de recherche par shard actif** →
  fusion (ex æquo par `(shard, segment, doc)`). Avec `allowed_ids`, le
  routeur groupe les ids par shard : les shards sans id n'ont pas de nœud.
- **Persistance** : `FsShardStorage` (mmap), `RamShardStorage` (tests),
  `BlobShardStorage<S>` → `BlobDirectory` (ACID : blobs = vérité, cache mmap
  jetable, **jamais fsync** sur le cache, `.managed.json` poussé au point de
  commit seulement ; `Eager` ou `Lazy` avec `load_range` ≤ 64 Ko et
  matérialisation au 4e accès). LUCE/LUCID/LUCIDS = l'autre topologie (copie
  locale durable tenue par deltas).
- Coût d'un commit sale (MemBlobStore, 9 docs / 2 shards) : ~6 ms, ~91
  appels au store (un par fichier de segment + 2 registres + 2 meta).

## 6. sparse_vector — l'index sparse

- `SparseVector { indices: Vec<u32>, values: Vec<f32> }` (poids nuls ignorés
  à l'insertion, dims dupliquées d'une requête sommées).
- `src/wand/` : `Posting {id, weight, tail_max}` (plafond suffixe inclusif),
  `PostingCursor` (peek/advance/seek/remaining/last_id/upper_bound),
  `SliceCursor` (RAM), `MmapCursor` (sur `MmapPostingData::entries`),
  `Postings`/`PostingsBuilder` (upsert/delete avec réparation des plafonds
  sur le préfixe changé seulement), `Frontier` (lanes triées, pivot WAND,
  `score_window`), `ScoreSink` (`TopKSink`, `CollectAll`), `search_with`
  (fenêtre 4096, élagage seulement quand le seuil bouge, `score > floor`
  avant filtre) et **`search_ids`** (mode seek pour un `allowed` petit).
- `SparseIndex` (RAM, serde compatible `sparse.bin`), `mmap_index`
  (`sparse.mmap` plat + `write_mmap_file` + `search_mmap[_allowed]`),
  `SparseHandle` (fs ou blob : `Sparse_{name}`, 3 fichiers, cache tmp),
  **`ShardedSparseHandle`** (`sharded.rs`) : `ShardedSparseConfig` strict,
  `SparseShardStorage` (fs / blob), acteurs `Insert`/`Remove`/`Search`/
  `Commit`/`Drain`/`Shutdown`, routage par dimensions hachées, `search`
  (scatter + fusion score desc / id asc), `search_filtered` routé par le
  routeur, `commit`, `close` inerte, `drop_index`. Pas de stats globales :
  le distribué est une fusion de `(id, score)`.

## 7. Bindings et consommateurs

Python (pyo3), Node (napi), C++ (cxx) : prêts v2, JSON `QueryConfig`
passthrough, snapshot/delta ; à rejouer après les changements de `parse`.
Emscripten : builde, ne tourne pas encore sous Node. **rag3weaver** (le vrai
client) : Rust direct sur `ShardedHandle` + `BlobShardStorage` +
`CypherBlobStore` (+ tampon write-back, une requête `UNWIND` par flush), et
`SparseHandle` — bientôt `lucivy/sparse_vector` par chemin.

## 8. Contrats à ne pas casser

1. Spans = octets exacts du texte source, vérifiées contre le disque.
2. Un `DocSet` est monotone ; un prescan rend un `doc_tf` trié.
3. `close()` rend inerte ; aucun appel au store après (test-sentinelle).
4. Un `Reply` répond ou avertit ; une panique de nœud est une erreur de DAG.
5. Pas de fsync sur un cache jetable ; le store est la vérité.
6. Config stricte à l'entrée, tolérante à la réouverture ; erreurs qui
   nomment les clés valides.
7. Format : bump `v=` de `index_shape_key` dans le harnais 50k et, si le
   contenu des sidecars change de sens, la version de la section STATS.
