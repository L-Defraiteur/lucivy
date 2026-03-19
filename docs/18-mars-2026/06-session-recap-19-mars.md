# Session 18-19 mars 2026 — Récapitulatif

## Bugs corrigés

### 1. Contains default fuzzy distance (commit 5fc5871)

**Problème** : `build_contains_query` utilisait `distance.unwrap_or(1)`, causant
des matches fuzzy parasites ("unction" pour "function") dans les highlights.

**Fix** : `unwrap_or(0)` pour les queries contains (2 endroits dans query.rs).
Le default pour `fuzzy` reste 1. Tests de régression ajoutés.

### 2. Deadlock wait_blocking dans le scheduler

**Problème** : les shard actors appellent `writer.commit()` / `writer.commit_fast()`
qui font `prepare_commit()` → `wait_blocking()` pour flusher les indexer actors.
Quand appelé depuis un handler d'acteur (thread scheduler), le `wait_blocking`
bloque ce thread, empêchant l'indexer actor d'être dispatché → **deadlock**.

Le blocage "intermittent" qu'on observait (parfois bloqué à 45K, 85K docs)
était ce deadlock. Il se manifestait quand les 4 shards faisaient commit
simultanément et bloquaient suffisamment de threads scheduler.

**Diagnostic** : grâce à `wait_cooperative_named` + `dump_state()`, le warning
montre exactement quel acteur bloque :
```
[luciole] WARNING: "commit_fast_shard" waiting 1130.0s (warn #113)
  ActorId(11) "indexer": BUSY "processing" (1132.0s) | queue: 0
  ActorId(29) "shard-1": BUSY "processing" (1131.9s) | queue: 0
```

**Fix** : remplacer TOUS les `wait_blocking` dans `index_writer.rs` et
`segment_updater.rs` par `wait_cooperative_named`. Ça permet au thread de
continuer à pomper le scheduler pendant l'attente.

Fichiers modifiés :
- `src/indexer/index_writer.rs` : `prepare_commit()` et `rollback()`
- `src/indexer/segment_updater.rs` : `schedule_commit_with_rebuild()`,
  `schedule_garbage_collect()`, etc.

### 3. Race condition post-drain merges

**Problème** : après `drain_all_merges()` + `rebuild_deferred_sfx()` dans le
commit, le handler relançait `collect_merge_candidates()` → nouveaux merges
→ nouveaux segments deferred APRÈS le rebuild.

**Fix** : ne pas relancer de merges après un commit avec `rebuild_sfx: true`.

## Features implémentées

### commit_fast() / commit()

- `commit_fast()` : persist les données, les merges utilisent `merge_sfx_deferred`
  (skip le FST rebuild O(E log E)). Rapide pour l'ingestion bulk.
- `commit()` : drain les merges pending + `rebuild_deferred_sfx()` + persist.
  Tous les FST sont valides après. Prêt pour search.

Exposé dans : `IndexWriter`, `PreparedCommit`, `ShardedHandle`.

Pattern bulk :
```rust
for batch in docs.chunks(5000) {
    index(batch);
    handle.commit_fast();
}
handle.commit();  // rebuild FSTs, ready for search
```

### Observabilité luciole

#### wait_cooperative_named (reply.rs)
- Label sur chaque wait pour identifier ce qui bloque
- Warning périodique si wait > seuil (LUCIVY_WAIT_WARN_SECS, défaut 10s)
- Dump de l'état du scheduler inclus dans le warning
- Log "resolved" quand le wait finit après un warning

#### ActorActivity (scheduler.rs)
- Chaque actor slot a un `ActorActivity` (Mutex<Option<(&str, Instant)>>)
- Mis à jour par le scheduler avant/après chaque dispatch
- Label "processing" par défaut (TODO: labels granulaires par type de message)

#### dump_state() (scheduler.rs)
- `Scheduler::dump_state() -> String` : état de tous les acteurs
- Pour chaque acteur : nom, activité (BUSY/idle/TAKEN), durée, queue depth

#### lucivy_trace!() (lib.rs)
- Macro conditionnelle : `LUCIVY_DEBUG=1` pour activer
- Zero-cost quand désactivé (check AtomicU8 lazy-initialisé)

### merge_sfx_deferred (merger.rs)

Skip le SuffixFstBuilder (phases 1-2) pendant les merges. Copie seulement
gapmap + sfxpost avec doc_id remapping. Écrit .sfx avec FST vide
(num_suffix_terms=0). Le FST est reconstruit par `rebuild_deferred_sfx()`.

### Bench Linux kernel

Dataset : `/home/luciedefraiteur/linux_bench` (clone --depth 1 de torvalds/linux).
~91K fichiers texte, 2GB.

Résultats finaux (90K docs, 4 shards RR, release) :
```
Indexation : 84s (commit_fast + final commit 8s)
Queries    : 25-35ms pour contains/startsWith
Highlights : corrects
Deferred   : 0 segments après final commit
```

## Problèmes ouverts

### 1. Faible nombre de hits pour "mutex" (36/91K)

**Observation** : `contains 'mutex'` retourne 36 hits alors que 5259 fichiers
contiennent la string "mutex_lock". Tous les FST sont valides (0 deferred).

**Diagnostic actuel** :
```
contains 'mutex' (single):       36 hits
contains 'lock' (single):        640 hits
contains 'mutex_lock' (multi):   12 hits
contains_split 'mutex lock':     640 hits (= résultat de "lock" seul)
```

**Hypothèses** :
- Le tokenizer (`SimpleTokenizer + CamelCaseSplitFilter + LowerCaser`) split
  bien `mutex_lock` en `["mutex", "lock"]`. Donc "mutex" devrait apparaître
  dans >5000 docs.
- Peut-être un problème dans le suffix FST : les termes trop fréquents sont
  filtrés ? Ou le `max_docs_before_merge` cause une perte de termes lors des
  merges ?
- Peut-être un problème dans le PostingResolver : les ordinals post-merge ne
  correspondent plus aux sfxpost entries ?

**Diagnostics à ajouter** :
1. Compter le nombre de termes uniques dans le term dictionary de chaque segment
   et vérifier que "mutex" y est bien
2. Vérifier que le raw_ordinal de "mutex" dans le FST correspond bien au bon
   ordinal dans le term dict / sfxpost
3. Ajouter un mode "count all hits" (pas top-K) pour avoir le vrai doc_freq
4. Comparer single-shard vs 4-shard pour isoler si c'est le sharding qui cause
   la perte

### 2. Activity labels granulaires

Le dump montre "processing" pour tous les acteurs. Pour un diagnostic fin,
il faudrait des labels spécifiques :
- segment_updater : "merge_step:postings", "merge_step:sfx", "commit", etc.
- shard : "insert", "commit", "search"
- indexer : "flush", "index_doc"

**Implémentation** : exposer `ActorActivity` à travers `ActorState` pour que
les handlers puissent appeler `activity.set("commit")` au début de chaque
traitement.

### 3. Scaling au-delà de 90K docs

L'indexation à 90K prend 84s (linéaire globalement mais des sauts de 7-8s
pour les merges de 10-12K docs). Pour scaler à 1M+ docs :
- Envisager de désactiver les merges complètement pendant l'ingestion bulk
  (pas juste le FST — les postings aussi)
- Ou réduire le `max_docs_before_merge` pour limiter la taille des merges
- Ou dédier un thread séparé (hors scheduler) aux merges pour ne pas bloquer

### 4. contains_split retourne le résultat du dernier token seul

`contains_split 'mutex lock'` retourne 640 hits = même résultat que `lock` seul.
Le split crée un `boolean should` de deux contains indépendants. C'est le
comportement voulu (OR), mais l'utilisateur s'attend peut-être à un AND.
À clarifier.

## Fichiers modifiés (non committés)

### Core lucivy (ld-lucivy)
- `src/lib.rs` — lucivy_trace!() macro
- `src/indexer/merger.rs` — merge_sfx_deferred + instrumentation
- `src/indexer/merge_state.rs` — step_sfx → deferred + timing
- `src/indexer/segment_updater_actor.rs` — rebuild_deferred_sfx, SuCommitMsg flag,
  merge_policy trace, no-merge-after-rebuild
- `src/indexer/segment_updater.rs` — schedule_commit_with_rebuild, wait_cooperative
- `src/indexer/index_writer.rs` — commit_fast(), wait_cooperative
- `src/indexer/prepared_commit.rs` — commit_fast()
- `src/index/segment_reader.rs` — load_sfx_files skip deferred
- `src/suffix_fst/file.rs` — handle FST vide
- `src/suffix_fst/gapmap.rs` — raw_data()
- `src/query/phrase_query/suffix_contains.rs` — test régression

### luciole (scheduler)
- `luciole/src/reply.rs` — wait_cooperative_named
- `luciole/src/scheduler.rs` — ActorActivity, dump_state()

### lucivy_core
- `lucivy_core/src/query.rs` — contains default distance 0
- `lucivy_core/src/sharded_handle.rs` — ShardCommitMsg.fast, commit_fast(),
  wait_cooperative_named labels
- `lucivy_core/src/handle.rs` — MAX_DOCS_BEFORE_MERGE = 10_000
- `lucivy_core/benches/bench_sharding.rs` — commit_fast, diagnostic queries,
  symlink protection, Linux kernel dataset
- `lucivy_core/tests/test_cold_scheduler.rs` — test_single_handle_highlights

### Docs
- `docs/18-mars-2026/02-bug-highlight-offsets-pretokenized.md` — résolution
- `docs/18-mars-2026/03-investigation-highlight-offsets-suffix-contains.md`
- `docs/18-mars-2026/04-investigation-merge-sfx-bottleneck.md`
- `docs/18-mars-2026/05-design-luciole-observability.md`
- `docs/18-mars-2026/06-session-recap-19-mars.md` (ce fichier)
