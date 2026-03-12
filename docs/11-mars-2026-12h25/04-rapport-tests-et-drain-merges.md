# Doc 04 — Rapport intermédiaire : tests + DrainMerges + nettoyage Inventory

**Date** : 11 mars 2026
**Branche** : `scheduler-beta`
**Réf** : doc `03` (progression), doc `13` du 9-mars (debug phase 3)

---

## Tests : 1092/1092 OK ✅

Après le fix DrainMerges, `cargo test` passe intégralement (exit 0, 1077 lignes "ok",
0 FAILED). Les ~15 lignes restantes sont des warnings "has been running for over 60
seconds" — proptests lents en debug mode, pas des échecs.

---

## Bug corrigé : `wait_merging_threads` bloquait indéfiniment

### Symptôme

`cargo test` tournait à 105% CPU pendant 6h+ sans terminer. Un processus test
en boucle infinie.

### Cause racine

`wait_merging_thread()` appelait `census::Inventory::wait_until_empty()` — un
condvar qui bloque le thread appelant jusqu'à ce que tous les `TrackedObject`
soient droppés. Avec les merges incrémentaux, les `MergeOperation` (TrackedObject)
vivent dans `active_merge` et `pending_merges` de l'acteur. Ils ne sont droppés
que quand `poll_idle()` les traite. Mais `wait_until_empty` ne fait pas tourner
le scheduler → deadlock.

C'est exactement le bug documenté dans le doc 13 du 9-mars :
> "Si le message EndMerge ne passe pas, le TrackedObject n'est jamais droppé,
> wait_until_empty bloque à jamais"

### Fix : message DrainMerges

**Nouveau message** : `SegmentUpdaterMsg::DrainMerges(Reply<()>)`

**Handler** : `drain_all_merges()` — exécute synchroniquement (en bouclant sur
`state.step()`) tous les merges actifs et en attente, puis répond.

**`wait_merging_thread()`** réécrit : envoie `DrainMerges` et attend via
`wait_cooperative(|| scheduler.run_one_step())`.

### Fichiers modifiés

- `src/indexer/segment_updater_actor.rs` : ajout `DrainMerges` + `drain_all_merges()`
- `src/indexer/segment_updater.rs` : réécriture de `wait_merging_thread()`

---

## État actuel : dette technique `census::Inventory`

### Problème

`census::Inventory` est encore utilisé pour 2 choses :

1. **`segment_in_merge()`** — pour exclure les segments en merge de
   `get_mergeable_segments()`. Redondant : l'acteur sait déjà quels segments
   sont en merge via `active_merge` et `pending_merges`.

2. **`MergeOperation::new(&self.merge_operations, ...)`** — pour créer des
   `TrackedObject`. Utilisé dans `collect_merge_candidates()` (actor) et
   `make_merge_operation()` (segment_updater, pour les merges explicites).

`wait_until_empty` n'est plus appelé (remplacé par DrainMerges), mais le
tracking Inventory continue de tourner — overhead inutile et source de confusion.

### Plan de nettoyage

1. **Simplifier `MergeOperation`** : struct simple sans `TrackedObject`, juste
   `target_opstamp` + `segment_ids`.

2. **Tracker les segments en merge dans l'acteur** : un `HashSet<SegmentId>`
   dans `SegmentUpdaterActor`, mis à jour par `start_merge` / `do_end_merge`.

3. **Exposer l'info via message** : `GetSegmentsInMerge(Reply<HashSet<SegmentId>>)`
   pour que `get_mergeable_segments()` puisse exclure les bons segments.
   Ou bien : internaliser `collect_merge_candidates` entièrement dans l'acteur
   (c'est déjà le cas — `get_mergeable_segments` est appelé depuis l'acteur).

4. **Supprimer `MergeOperationInventory`** et la dépendance `census`.

### Bénéfice

- Suppression d'une dépendance externe (`census`)
- Plus de risque de deadlock lié à `wait_until_empty`
- Le state des merges est entièrement dans l'acteur — single source of truth

---

## Prochaines actions

1. **Nettoyage Inventory** (ci-dessus)
2. **Étape 4** — Events d'observabilité merge
3. **Étape 5** — Benchmarks avant/après
