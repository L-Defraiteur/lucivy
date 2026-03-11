# Rapport 13 — Phase 3 : Debug du deadlock SegmentUpdaterActor

**Date** : 10 mars 2026, ~23h
**Branche** : `master`
**État** : Phase 3 code-complete, tests partiellement bloqués

---

## Résumé de la session

### Ce qui a été fait

1. **Fix du deadlock principal (wait_cooperative)** — FONCTIONNE
   - Problème : `IndexerActor` (sur thread scheduler) appelait `schedule_add_segment()` → `reply_rx.wait_blocking()` → bloquait le seul thread scheduler → `SegmentUpdaterActor` ne pouvait jamais traiter le message
   - Solution : `SegmentUpdater` stocke un `Arc<Scheduler>`, toutes les méthodes utilisent `reply_rx.wait_cooperative(|| scheduler.run_one_step())` au lieu de `wait_blocking()`
   - **Preuve que ça marche** : test `test_delete_during_merge` passe, les 8 commits du test `garbage_collect_works_as_intended` passent en 1 step chacun

2. **Fix de l'ordre dans `wait_merging_threads`**
   - Avant : shutdown scheduler → puis attendre les merges (deadlock car plus de scheduler pour traiter `EndMerge`)
   - Après : attendre les merges → puis shutdown scheduler

3. **Fichiers modifiés** (par rapport au rapport 12) :
   - `src/indexer/segment_updater.rs` : ajout `scheduler: Arc<Scheduler>` dans `SegmentUpdater`, `create()` prend `Arc<Scheduler>`, 4× `wait_blocking` → `wait_cooperative`
   - `src/indexer/index_writer.rs` : `scheduler: Arc<Scheduler>`, inversion ordre dans `wait_merging_threads`

### Ce qui bloque encore

**Le merge rayon ne termine pas.** Test `garbage_collect_works_as_intended` :
- 8 commits passent parfaitement (AddSegment + Commit via wait_cooperative, 1 step chacun)
- Au dernier commit, `consider_merge_options` lance un merge de 8 segments sur le `merge_thread_pool` (rayon)
- Le merge thread ne produit jamais de `EndMerge` → `wait_merging_threads()` → `census::Inventory::wait_until_empty()` bloque indéfiniment

**Pistes** :
1. Le merge rayon thread pourrait être bloqué dans la fonction `merge()` elle-même (I/O sur RamDirectory?)
2. Le merge thread pourrait être bloqué en essayant d'accéder à un état partagé verrouillé (RwLock sur merge_policy ou active_index_meta?)
3. Le rayon `ThreadPool` pourrait ne pas exécuter la closure (configuration du pool, nombre de threads = 0?)
4. 7 threads visibles dans `ps -eLf` : 1 main + 1 scheduler + 4 merge threads + 1 test runner — les merge threads semblent tous à 0% CPU → ils sont parkés, pas en train de travailler

**Diagnostic manquant** : on ne sait pas si la closure rayon `merge_thread_pool.spawn(...)` est effectivement exécutée. Il faudrait un `eprintln` au début de la closure.

---

## Pistes architecturales

### Le vrai problème de fond

On a un **système hybride** : le scheduler gère les acteurs (IndexerActor, SegmentUpdaterActor), mais les merges tournent encore sur un `rayon::ThreadPool` séparé qui communique avec le scheduler via `self_ref.send(EndMerge)`. Ce pont entre rayon et le scheduler est fragile :
- Le `MergeOperation` (TrackedObject de census) doit traverser la frontière rayon → actor
- Si le message `EndMerge` ne passe pas, le TrackedObject n'est jamais droppé, `wait_until_empty` bloque à jamais
- Le rayon thread pool a sa propre gestion de threads, indépendante du scheduler

### Solution propre : Phase 5 anticipée (MergerActor)

Selon le doc 07, Phase 5 remplace le `rayon::ThreadPool` merge par un `MergerActor`. Ça éliminerait le pont rayon↔scheduler et mettrait tout sous un même modèle. Mais c'est un gros changement.

### Observabilité — ce qui nous aurait aidé

Le `EventBus` existe déjà (doc 06/08) mais n'est pas utilisé pour le debug. Améliorations possibles :

1. **Events pour le SegmentUpdaterActor** : émettre un event quand un merge est lancé (`MergeStarted { segment_ids }`) et quand il finit (`MergeCompleted { segment_ids, duration }`). Actuellement on n'a aucune visibilité sur les merges en cours.

2. **Event "actor message received"** : le scheduler émet déjà `MessageHandled` (avec durée, profondeur mailbox), mais on ne voit pas le *contenu* du message. Un event `MessageReceived { actor_name, msg_type: &str }` serait utile.

3. **Health check / watchdog** : un mécanisme qui détecte quand un acteur n'a pas traité de message depuis X secondes. Ça aurait immédiatement signalé que le SegmentUpdaterActor était bloqué.

4. **Merge thread monitoring** : puisque les merge threads sont hors du scheduler, ils sont invisibles. Un simple `Arc<AtomicU32>` comptant les merges actifs + un event à chaque début/fin de merge sur rayon donnerait de la visibilité.

5. **Test tooling** : un helper `assert_completes_within(duration, || { ... })` pour les tests qui impliquent des merges, au lieu de bloquer indéfiniment.

### Approche pragmatique pour demain

Avant de tout restructurer, le bug immédiat est probablement simple :

1. Vérifier que la closure rayon s'exécute (ajouter un print au début)
2. Si elle s'exécute, vérifier que `merge()` ne bloque pas (print avant/après)
3. Si `merge()` finit, vérifier que `self_ref.send(EndMerge)` fonctionne (print avant/après)

Si le problème est que rayon ne spawn pas la closure, c'est un problème de configuration du pool. Si c'est `merge()` qui bloque, c'est un problème d'I/O ou de lock. Dans les deux cas, c'est un bug localisé, pas un problème d'architecture.

---

## État des fichiers (diff par rapport à master)

### Fichiers avec debug temporaire (à retirer)
- `src/indexer/segment_updater.rs` : eprintln dans `schedule_commit`
- `src/indexer/segment_updater_actor.rs` : eprintln dans `handle()` et `consider_merge_options`

### Fichiers modifiés (changements permanents)
- `src/actor/mailbox.rs` : manual Clone impl pour ActorRef
- `src/indexer/mod.rs` : ajout `pub(crate) mod segment_updater_actor`
- `src/indexer/segment_updater.rs` : SegmentUpdaterShared + SegmentUpdater facade + wait_cooperative
- `src/indexer/segment_updater_actor.rs` : nouveau fichier (~390 lignes avec debug)
- `src/indexer/index_writer.rs` : Arc<Scheduler>, inversion wait_merging_threads, suppression FutureResult
- `src/indexer/prepared_commit.rs` : suppression commit_future(), appel direct schedule_commit
- ~14 fichiers : suppression mécanique de `.wait()` sur les anciens FutureResult

---

## Décision : supprimer rayon, passer directement au MergerActor

Plutôt que debugger le pont rayon↔scheduler (état intermédiaire voué à disparaître), on passe directement à Phase 5 : remplacer le `merge_thread_pool` rayon par un `MergerActor` dans le scheduler.

**Conséquences** :
- Plus de `rayon::ThreadPool` dans le SegmentUpdaterActor
- Le merge tourne directement dans le handler du MergerActor (ou du SegmentUpdaterActor lui-même)
- Avec 1 thread : le merge bloque le scheduler (acceptable, les merges sont entre les commits — doc 07)
- Avec N threads : le merge prend un slot scheduler, les autres acteurs continuent sur les autres threads
- Plus de `self_ref` / `SelfRefSlot` nécessaire (plus de communication rayon → actor)
- Plus de `census::Inventory::wait_until_empty` comme mécanisme d'attente (on peut utiliser Reply directement)

**Plan pour demain** :
1. Supprimer `merge_thread_pool` du SegmentUpdaterActor
2. Le merge s'exécute dans le handler (blocking) ou dans un MergerActor dédié
3. `EndMerge` devient un appel direct interne au lieu d'un message inter-thread
4. Retirer les eprintln de debug
5. Lancer les tests

### Compilation
`cargo check` passe (23-24 warnings pré-existants framework actor).
