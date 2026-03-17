# Session recap — Pipeline d'indexation + bench 212K

Date : 17 mars 2026 (soir)

## Commits de la session

| Commit | Description |
|--------|-------------|
| `e225df1` | Reader Actors Pipeline + Background Finalize |
| `a3b7563` | Pre-tokenized pipeline + unified SfxCollector (zero-copy) |
| `27dfdab` | Batch insert API + merge_sfx instrumentation |
| `692132b` | Bypass pipeline pour single-shard |
| `b8b2b92` | Garbage-collect zombie actors (channel disconnected) |
| `b4f9193` | Skip alive check dans merge_sfx + BENCH_MODE flags |

## Optimisations implémentées

### 1. Reader Actors Pipeline (e225df1)
- Pool de N ReaderActors tokenize+hash en parallèle
- RouterActor unique route séquentiellement
- `add_document()` non-bloquant (fire-and-forget)
- `drain_pipeline()` avant commit/close/search
- **Gain 5K docs : 3.05s → 1.67s (1.8x)**

### 2. Background Finalize (e225df1)
- FinalizerActor (GenericActor) exécute `finalize_segment()` en background
- IndexerActor démarre un nouveau segment immédiatement
- Pipeline depth = 2 : finalize[N-1] overlap avec batch[N]
- WASM compatible (scheduler global, pas de std::thread::spawn)

### 3. Pre-tokenized pipeline (a3b7563)
- `tokenize_for_pipeline()` produit hashes + `PreTokenizedData` en un seul pass
- `std::mem::take(&mut token.text)` — zero-copy depuis le TokenStream
- SegmentWriter fast path : postings ET SfxCollector depuis les mêmes tokens
- SfxCollector gère les offsets chevauchants (gap vide)
- `PreTokenizedData = Vec<(Field, Vec<PreTokenizedString>)>` — multi-valeur safe
- **Gain 5K docs : 1.67s → 1.39s (total 2.2x vs baseline)**

### 4. Bypass pipeline single-shard (692132b)
- Si 1 shard : `add_document()` utilise le path direct (tokenize + send)
- Élimine le overhead de 3 hops (reader → router → shard) sans parallélisme
- **1-shard revenu à la baseline (~2.7s)**

### 5. Zombie actor GC (b8b2b92)
- `Mailbox::is_disconnected()` détecte les channels fermés
- Le scheduler retire les acteurs zombies (senders droppés + mailbox vide)
- Résout l'accumulation d'acteurs morts entre les runs de bench

### 6. Phase A merge_sfx (b4f9193)
- Fast path : skip alive check quand pas de deletes
- Collecte les tokens directement depuis les term dictionaries
- Gain mesuré : ~90ms → ~2ms sur step 1 (mais step 2 domine à 470ms)

## Expérimentations non retenues

### Batch insert
- `add_documents(Vec)` distribue des sub-batches aux ReaderActors
- **Contre-productif** : un handler qui boucle sur 1250 docs monopolise le thread
- Le scheduler parallélise mieux avec des messages individuels
- API gardée mais pas utilisée par défaut

### N-way merge FST (Phase B)
- Merger les streams FST triés au lieu de rebuild O(E log E)
- **Plus lent** : les allocations de Vec<u8> par entry dans le heap dépassent le coût du sort
- Stashé pour investigation future sur de très gros index

### Skip merge_sfx
- Skipper le merge_sfx pendant les merges, reconstruire au commit
- **Problème** : les segments mergés sans .sfx crashent les queries contains/startsWith
- Nécessiterait un rebuild au commit ou lazy rebuild au search
- Abandonné car le merge des postings/fast fields est aussi un bottleneck

## Bench comparatif par commit (5K docs, release)

Moyenne des meilleurs runs, TA-4sh :

```
baseline (5dc9a2c)        : TA-4sh ~2.10s
pipeline+bgfin (e225df1)  : TA-4sh ~1.42s  (1.5x)
pretokenized (a3b7563)    : TA-4sh ~1.33s  (1.6x)
```

Note : variabilité importante (~20%) selon la charge système.

## Bench 212K docs — findings

### 1-shard (LucivyHandle direct)
```
212K docs en 167s (~0.8ms/doc)
Linéaire, pas d'explosion exponentielle
Peak mémoire raisonnable (~3GB)
```

### TA-4sh (ShardedHandle, token-aware)
```
25K docs en 33.9s — exponentiel, explose
Cause : merge_sfx × 4 shards en parallèle
SuffixFstBuilder alloue tout en mémoire pour le sort
Avec 4 shards : 4× la mémoire peak (~10GB+)
```

### RR-4sh (ShardedHandle, round-robin)
```
212K docs en 254s (sans merge_sfx: skip)
Avec merge_sfx : même problème que TA, exponentiel
Crash query : "no .sfx file" quand merge_sfx skippé
```

### Conclusion 212K
- Le sharding **gagne** sur les petits index (5K docs : 2.2x plus rapide)
- Le sharding **perd** sur les gros index (212K : 1.5x plus lent que 1-shard)
- Cause : le merge des postings + SfxCollector.build() × 4 shards
- Le bottleneck principal est le SuffixFstBuilder.build() O(E log E)
- Peak mémoire : ~10GB avec 4 shards (chaque shard merge en parallèle)

## Problème fondamental identifié

Le `SuffixFstBuilder` accumule TOUTES les suffix entries en mémoire :
- 50K docs × 100 tokens/doc × 10 suffixes/token = 50M entries
- Chaque entry = String (heap) + ParentEntry
- × 4 shards en parallèle = mémoire explosive

### Solutions possibles (non implémentées)

1. **Streaming SuffixFstBuilder** — écrire les entries sur disque au lieu de tout garder en mémoire, puis faire un merge-sort externe. O(E log E) mais avec mémoire bornée.

2. **Incremental suffix FST** — au lieu de rebuild from scratch, maintenir le FST incrémentalement (insert/delete). Plus complexe mais O(1) par opération.

3. **Réduire le nombre de merges** — commit_every plus grand (50K) ou un seul commit final. Trade-off : latence de commit vs coût de merge.

4. **Merger les .sfx séparément** — ne pas merger le .sfx avec les postings. Garder les .sfx des segments sources et les fusionner à la lecture (multi-segment sfx search). Trade-off : search un peu plus lent mais indexation beaucoup plus rapide.

5. **Cap mémoire sur SuffixFstBuilder** — quand la mémoire dépasse un seuil, flush sur disque et merger les runs triés (external sort).

## Fichiers modifiés (session complète)

```
lucivy_core/src/sharded_handle.rs   — pipeline actors, pre-tokenize, batch, 1-shard bypass
src/indexer/indexer_actor.rs         — background finalize (FinalizerActor)
src/indexer/index_writer.rs          — add_document_pre_tokenized()
src/indexer/operation.rs             — PreTokenizedData type
src/indexer/segment_writer.rs        — pre-tok fast path + normal path
src/indexer/merger.rs                — Phase A, timing instrumentation
src/suffix_fst/collector.rs          — overlapping offsets handling
luciole/src/mailbox.rs               — is_disconnected()
luciole/src/scheduler.rs             — zombie actor GC
lucivy_core/benches/bench_sharding.rs — BENCH_MODE, commit_every, progress
```

## Tests : 1318 green (51 luciole + 1185 ld-lucivy + 82 lucivy-core)
