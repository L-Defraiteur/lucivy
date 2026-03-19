# Doc 15 — Plan : TOUT en DAG — observabilité totale

Date : 19 mars 2026

## Le principe

Chaque opération dans lucivy est un DAG (ou un sous-graphe d'un DAG).
Résultat : une seule source d'observabilité pour tout. Pas de env var,
pas de eprintln, pas de lucivy_trace!. Juste des events structurés
qu'on subscribe depuis n'importe quel thread.

## Ce qui est déjà en DAG

| Opération | Status | Nœuds |
|-----------|--------|-------|
| Commit (rebuild_sfx) | ✅ DAG | prepare → merge_N ∥ → finalize → save → gc → reload |
| Commit (fast) | ❌ inline | purge + save (pas de DAG, 2 lignes) |

## Ce qu'on veut mettre en DAG

### 1. Search DAG

Aujourd'hui la search fait tout inline dans `ShardedHandle::search()` :
flush, build weight, scatter shards, merge heap. Aucune observabilité
structurée — juste des timings manuels.

```
search DAG :
  drain_pipeline ── flush_uncommitted ── build_weight ──┬── search_shard_0 ──┐
                                                         ├── search_shard_1 ──┼── merge_results
                                                         └── search_shard_2 ──┘
```

**Nœuds :**

| Nœud | Type | Ce qu'il fait | Métriques |
|------|------|---------------|-----------|
| drain_pipeline | Node | reader_pool.drain + router drain | pipeline_items_drained |
| flush_uncommitted | Node | scatter Commit(fast) aux shards dirty | shards_flushed |
| build_weight | Node | parse query, aggregate BM25 stats, compile Weight | query_terms, stats_ms |
| search_shard_N | Node (parallel) | execute weight on shard, collect top-K | hits, scorer_ms |
| merge_results | Node | binary heap merge des résultats | total_hits, merge_ms |

**Avantages :**
- Chrono par étape (on saura si c'est le build_weight ou le scorer qui est lent)
- Tap sur l'edge build_weight → search_shard : inspecter le Weight compilé
- Tap sur l'edge search_shard → merge : voir les résultats par shard
- Parallélisme des shards garanti par le DAG (pas par du code ad-hoc)
- Le flush pré-search est un nœud → on peut le tapper pour vérifier les commits

### 2. MergeState comme GraphNode

Le MergeNode est un PollNode qui appelle MergeState::step(). Mais
les phases internes (Init, Postings, Store, FastFields, Sfx, Close)
sont opaques. On pourrait en faire un GraphNode avec des sous-nœuds.

```
merge GraphNode :
  init ── postings_field_0 ── postings_field_1 ── ... ── store ── fast_fields ── sfx ── close
```

**Nœuds internes :**

| Nœud | Métriques |
|------|-----------|
| init | doc_id_mapping_size, fieldnorms_ms |
| postings_field_N | field_name, terms, postings_ms |
| store | stored_fields_ms, bytes_written |
| fast_fields | columns, fast_fields_ms |
| sfx | tokens_collected, fst_build_ms, gapmap_docs, sfxpost_terms |
| close | segment_id, total_docs, total_ms |

**Avantage clé :** le nœud sfx est celui qui a le bug gapmap. En le
rendant observable avec des taps, on pourrait intercepter les données
AVANT et APRÈS le merge sfx pour diagnostiquer la corruption.

### 3. merge_sfx comme sous-graphe

merge_sfx est la fonction la plus complexe (~200 lignes). Ses étapes
internes pourraient être des nœuds :

```
sfx GraphNode :
  collect_tokens ── build_fst ── copy_gapmap ── merge_sfxpost ── validate ── write
```

| Nœud | Métriques |
|------|-----------|
| collect_tokens | unique_tokens, readers_with_sfx, readers_without_sfx |
| build_fst | fst_size_bytes, build_ms |
| copy_gapmap | docs_copied, empty_docs, total_bytes |
| merge_sfxpost | terms_merged, postings_remapped |
| validate | errors_found, docs_validated |
| write | sfx_bytes, sfxpost_bytes |

**Avantage clé :** le nœud validate est ce qu'on vient d'ajouter.
Comme nœud DAG, ses métriques (errors_found) apparaissent dans le
DagResult. Plus besoin de eprintln.

### 4. Commit fast comme DAG

Aujourd'hui c'est 2 lignes inline. Comme DAG trivial :

```
commit_fast DAG :
  purge_deletes ── save_metas
```

Avantage : même observabilité que le commit full. On voit le timing
du purge et du save dans le DagResult.

### 5. IndexerActor flush comme DAG

Le flush segment (IndexerActor → FinalizerActor) est opaque.
Comme DAG :

```
flush DAG :
  close_segment_writer ── write_fieldnorms ── write_postings ── write_store ── register_segment
```

C'est plus ambitieux — nécessite de refactorer SegmentWriter. À faire
en phase 2.

### 6. Delete comme DAG

Le delete est simple (broadcast un terme aux shards) mais comme DAG :

```
delete DAG :
  resolve_shard ──┬── delete_shard_0
                   ├── delete_shard_1  (si broadcast)
                   └── delete_shard_2
```

### 7. Ingestion pipeline comme DAG continu

Le pipeline Reader → Router → Shard est un StreamDag. Mais les
étapes internes du reader (tokenize) et du router (route + send)
pourraient être des nœuds observables.

Déjà géré par les acteurs typés + Pool. Le StreamDag ajoute la
topologie et le drain ordonné. Suffisant pour l'instant.

## Unification des events

### Aujourd'hui : 3 systèmes séparés

1. **SchedulerEvent** (scheduler.rs) → env var `LUCIVY_SCHEDULER_DEBUG`
2. **DagEvent** (runtime.rs) → `subscribe_dag_events()`
3. **IndexEvent** (events.rs) → `writer.subscribe_index_events()`

### Demain : 1 seul bus

Toutes les opérations étant des DAGs, tous les events sont des DagEvents.
Les IndexEvents (MergeStarted, CommitStarted, etc.) deviennent des
métriques/logs dans les nœuds correspondants.

Les SchedulerEvents restent utiles pour le debug bas-niveau (threads
parked, actor woken, etc.) mais ne sont plus nécessaires pour
l'observabilité métier.

```
subscribe_dag_events()  ← UNE SEULE souscription pour tout
  │
  ├── CommitDAG : NodeStarted("prepare"), NodeCompleted("merge_0"), ...
  ├── SearchDAG : NodeStarted("build_weight"), NodeCompleted("search_shard_0"), ...
  ├── MergeGraphNode : NodeStarted("sfx"), NodeCompleted("validate"), ...
  └── CommitFastDAG : NodeStarted("purge"), NodeCompleted("save"), ...
```

Plus besoin de :
- `LUCIVY_SCHEDULER_DEBUG=1` → les DagEvents suffisent
- `LUCIVY_DEBUG=1` → lucivy_trace! remplacé par NodeContext.info()
- `eprintln!("[merge_sfx]...")` → métriques dans les nœuds sfx

### display_progress pour tout

```rust
// Après un commit :
eprintln!("{}", dag_result.display_summary());
// prepare                    0ms  purged_segments=14
// merge_0                  120ms  docs_merged=2000 sfx_ms=80
// merge_1                  115ms  docs_merged=1800 sfx_ms=75
// finalize                   2ms  total_docs=3800
// save                       1ms
// gc                         5ms  deleted_files=12
// reload                     0ms

// Après une search :
eprintln!("{}", search_result.display_summary());
// drain_pipeline              0ms
// flush_uncommitted           3ms  shards_flushed=2
// build_weight                1ms  query_terms=3
// search_shard_0            45ms  hits=20
// search_shard_1            38ms  hits=15
// search_shard_2            42ms  hits=18
// merge_results               0ms  total_hits=20
```

## Le test qui fail (test_merge_single_filtered_segments)

Ce test utilise `merge_filtered_segments()` qui appelle `merger.write()`
directement — pas de MergeState, pas de DAG. La solution : faire passer
`merge_filtered_segments` par un DAG aussi.

```rust
// Aujourd'hui :
let merger = IndexMerger::open_with_custom_alive_set(...)?;
let num_docs = merger.write(serializer)?;

// Demain :
let mut dag = build_merge_dag(index, segment_entries, filters)?;
let result = execute_dag(&mut dag, None)?;
```

Le merger.write() fait exactement les mêmes phases que MergeState
(init, postings, store, fast_fields, sfx, close) mais en une seule
fonction. Si MergeState est un GraphNode, merger.write() utilise le
même GraphNode.

## Ordre d'implémentation

### Phase 1 : Search DAG (~150 lignes)
- Nœuds : DrainNode, FlushNode, BuildWeightNode, SearchShardNode, MergeResultsNode
- build_search_dag() factory dans sharded_handle.rs
- ShardedHandle::search() appelle execute_dag()
- Tests : search avec events, taps sur les résultats

### Phase 2 : Commit fast DAG (~30 lignes)
- 2 nœuds : PurgeNode, SaveNode
- Trivial, juste pour l'observabilité uniforme

### Phase 3 : MergeState → GraphNode (~100 lignes)
- 6 nœuds internes (init, postings_N, store, fast, sfx, close)
- Le MergeNode du commit DAG utilise ce GraphNode au lieu de PollNode
- Le nœud sfx émet les métriques de merge_sfx

### Phase 4 : merge_sfx → sous-graphe (~80 lignes)
- 6 nœuds (collect, fst, gapmap, sfxpost, validate, write)
- Le validate émet errors_found comme métrique (plus de eprintln)

### Phase 5 : Unification events (~50 lignes)
- Supprimer les eprintln dans merger.rs (remplacés par métriques nœuds)
- Supprimer lucivy_trace! (remplacé par NodeContext.info())
- Documenter : subscribe_dag_events() est la seule API d'observabilité

### Phase 6 : Fix test_merge_single_filtered_segments
- merger.write() → utilise le même GraphNode que MergeState
- Ou : merge_filtered_segments() construit un DAG

### Phase 7 : Delete DAG + Ingestion observabilité (optionnel)
- Delete comme mini-DAG
- StreamDag avec métriques par stage

## Estimation

```
Phase 1 (search DAG)            ~150 lignes
Phase 2 (commit fast DAG)       ~30 lignes
Phase 3 (MergeState GraphNode)  ~100 lignes
Phase 4 (merge_sfx sous-graphe) ~80 lignes
Phase 5 (unifier events)        ~50 lignes (surtout suppressions)
Phase 6 (fix test)              ~30 lignes
Phase 7 (delete + ingestion)    ~50 lignes

Total : ~490 lignes ajoutées
Supprimé : ~200 lignes (eprintln, lucivy_trace!, IndexEvent duplicates)
Net : ~290 lignes

Résultat : TOUT est observable via subscribe_dag_events()
```
