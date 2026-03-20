# Doc 04 — Contexte technique pour la prochaine session

Date : 20 mars 2026

## Comment fonctionne le suffix FST (SFX)

### Le principe
Lucivy a un système de recherche substring (contains) basé sur un FST de suffixes.
Pour chaque terme du term dict (ex: "function"), on génère TOUS les suffixes :
- SI=0: "function" (le terme complet)
- SI=1: "unction"
- SI=2: "nction"
- SI=3: "ction"
- etc.

Ces suffixes sont stockés dans un FST avec un prefix byte (0x00 pour SI=0, 0x01 pour SI>0).
La recherche "contains X" fait un `prefix_walk(X)` sur le FST → trouve tous les termes
contenant X comme substring.

### Les fichiers par segment
Chaque segment a, pour chaque champ text indexé :
- `.{field_id}.sfx` — le FST de suffixes + parent list + gapmap
- `.{field_id}.sfxpost` — les postings par ordinal (doc_id, token_index, byte_from, byte_to)

Le `.sfx` permet de trouver les termes qui matchent.
Le `.sfxpost` permet de trouver les documents et positions exactes.

### Le GapMap
Le gapmap stocke les séparateurs inter-tokens par document. Pour le highlight :
quand on matche "struct device" (2 tokens), on vérifie que le séparateur entre
les deux tokens est bien un espace (pas un saut de ligne ou un autre séparateur).
Format : per-doc, header (num_tokens, num_values), puis les gaps encodés.

### Le SfxCollector
Dans `segment_writer.rs`, le `SfxCollector` est un collecteur qui :
1. Reçoit les tokens bruts (RAW_TOKENIZER : lowercase + split, pas de stemming)
2. Stocke les gapmaps (séparateurs entre tokens)
3. Stocke les postings (doc_id, token_index, byte_from, byte_to)
4. À la finalisation (`build()`), construit le FST + gapmap + sfxpost en une fois

Le SfxCollector fait du **double tokenization** : le champ text est tokenizé
une fois par le tokenizer configuré (potentiellement stemmé) pour les postings BM25,
et une deuxième fois par RAW_TOKENIZER pour le SFX. C'est inefficace mais fonctionnel.

### Le merge SFX
Quand des segments sont mergés, le merge SFX :
1. `collect_tokens` — lit les term dicts de tous les segments source
2. `build_fst` — construit un nouveau FST avec tous les tokens uniques
3. `copy_gapmap` — copie les gapmaps byte-par-byte dans le nouvel ordre des docs
4. `merge_sfxpost` — fusionne les sfxpost avec remapping des doc_ids
5. `validate` — vérifie l'intégrité du gapmap et sfxpost
6. `write` — assemble et écrit les fichiers

Les étapes 2, 3, 4 sont indépendantes et parallélisées via `submit_task`.

## Le tokenizer "raw_code"

Le RAW_TOKENIZER est `SimpleTokenizer → CamelCaseSplitFilter → LowerCaser`.
- SimpleTokenizer split sur les non-alphanumériques
- CamelCaseSplitFilter split `getElementById` → `getElement`, `ById`
  et merge les chunks < 4 chars avec le suivant
- LowerCaser met tout en minuscules

Le champ text utilise RAW_TOKENIZER par défaut (sans stemmer).
Avec stemmer configuré : le champ principal utilise STEMMED_TOKENIZER,
le SfxCollector continue d'utiliser RAW_TOKENIZER.

## Les benchmarks

### Dataset
Linux kernel source : ~91K fichiers C/H dans `/home/luciedefraiteur/linux_bench`.
Le bench utilise `MAX_DOCS=N` pour limiter (5K pour les tests rapides).

### Structure du bench (`lucivy_core/benches/bench_sharding.rs`)
1. Index single shard (LucivyHandle directement)
2. Index 4 shards token-aware (ShardedHandle, balance_weight=0.2)
3. Index 4 shards round-robin (ShardedHandle, balance_weight=1.0)
4. Queries comparatives sur les 3 modes
5. Post-mortem : inspect_term avec ground truth verification

### Queries testées
- contains 'mutex_lock', 'function', 'sched', 'printk'
- contains_split 'struct device'
- startsWith 'sched', 'printk'
- contains 'drivers' (sur le champ path)

### Ground truth
`LUCIVY_VERIFY=1` active la vérification : itère les stored docs,
cherche le substring dans le texte brut, compare avec le résultat
de la search. Le `inspect_term_sharded_verified` fait cette comparaison.

### Index préservé
Les index sont dans `/home/luciedefraiteur/lucivy_bench_sharding/`
(single/, token_aware/, round_robin/). Pas supprimés après le bench.
Attention aux lock files (`.lucivy-writer.lock`) laissés si le bench crash.

### Résultats du dernier bench 5K (avant le fix SegmentComponent)
- mutex: 610/610 MATCH
- lock: 2454/2455 (diff=1)
- function: 1285/1305 (diff=20)
- printk: 178/178 MATCH
- sched: 420/424 (diff=4)

Les docs manquants sont causés par le sfxpost merge qui perd des docs
(segments source sans sfxpost). Le fix SegmentComponent devrait résoudre ça
car les fichiers sfxpost ne seront plus supprimés par le GC.

## Le DAG commit

### Structure
```
PrepareNode : purge_deletes + commit segment_manager + start_merge pour chaque merge op
  ↓ (fan-out par merge op)
MergeNode_0, MergeNode_1, ... (parallèle, PollNode wrapping MergeState::step())
  ↓ (fan-in)
FinalizeNode : end_merge pour chaque résultat + advance_deletes
  ↓
SaveMetasNode : écriture atomique meta.json
  ↓
GCNode : garbage_collect_files
  ↓
ReloadNode : no-op (reader lit meta.json au prochain search)
```

### MergeNode émet des métriques par phase
init_ms, postings_ms, store_ms, fast_fields_ms, sfx_ms, close_ms

### Le search DAG
```
DrainNode → FlushNode → BuildWeightNode → SearchShardNode_0..N (parallèle) → MergeResultsNode
```
Résultats extraits via `DagResult::take_output::<Vec<ShardedSearchResult>>("merge", "results")`

## Les acteurs typés

ShardedHandle utilise maintenant des acteurs typés au lieu de GenericActor<Envelope> :

```rust
enum ShardMsg { Search{..}, Insert{..}, Commit{..}, Delete{..}, Drain(..) }
enum RouterMsg { Route{..}, Drain(..) }
enum ReaderMsg { Tokenize{..}, Batch{..}, Drain(..) }
```

Pool<ShardMsg> pour les shards (key-routed par shard_id).
Pool<ReaderMsg> pour les readers (round-robin).
ActorRef<RouterMsg> pour le router (single actor).

drain_pipeline() : `reader_pool.drain()` → `router_ref.request(Drain)`.

## Le SegmentComponent refactoring

### Avant
- `SegmentComponent` enum avec SuffixFst (pour le manifest)
- Les per-field .sfx/.sfxpost n'étaient PAS dans l'enum
- Un manifest .sfx listait les field_ids
- Le GC devait lire le manifest + hardcoder des ranges de field_ids

### Après
- `SegmentComponent::SuffixFst { field_id }` et `SuffixPost { field_id }`
- `InnerSegmentMeta.sfx_field_ids: Vec<u32>` persisté dans meta.json
- `list_files()` retourne TOUS les fichiers y compris per-field sfx
- Manifest supprimé (no-op), backward compat via legacy reader
- `segment_writer::finalize()` retourne `(doc_opstamps, sfx_field_ids)`
- `SegmentMeta::with_sfx_field_ids()` pour propagation

## La question en suspens

Lucie demande : pourquoi tout ne passe pas par le SfxCollector ?

Réponse : le SfxCollector est dans le segment_writer qui est dans ld-lucivy (le core).
Il EST appelé pour chaque segment créé par le segment_writer. MAIS les tests
d'aggregation internes de tantivy créent des segments via d'autres chemins
(merge_indices, merge_filtered_segments) qui n'utilisent pas le segment_writer
complet — ils utilisent directement IndexMerger.

Le SfxCollector est appelé dans :
- `segment_writer::finalize()` → chaque nouveau segment écrit a son sfxpost ✓
- `merger::merge_sfx()` → chaque segment mergé reconstruit le sfxpost ✓

Le problème c'est quand un merge source n'a PAS de sfxpost (parce que le GC
l'a supprimé, ou parce que le segment a été créé avant le SfxCollector).
Le fix SegmentComponent devrait résoudre le GC. Les vieux segments sans
SfxCollector sont un edge case de backward compat.

## Commits de la session (~35 commits)
Tous sur la branche `feature/luciole-dag`, poussés sur origin.
