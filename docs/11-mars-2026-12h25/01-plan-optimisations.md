# Doc 01 — Plan d'implémentation des optimisations

**Date** : 11 mars 2026
**Branche** : `scheduler-beta`
**Prérequis** : Architecture actor complète (Phases 1-5 du doc `9-mars/07`), 1085 tests OK
**Référence** : `9-mars-2026-19h34/14-suite-optimisations-observabilite.md` (analyse d'impact)

---

## État des lieux

L'architecture actor est en place : zéro `thread::spawn`, zéro rayon dans le
pipeline d'écriture. Les merges s'exécutent en blocking dans le handler du
SegmentUpdaterActor. C'est le bottleneck #1 : en single-thread, un merge
bloque tout (indexation, commits, GC) pendant toute sa durée.

---

## Organisation en phases

Les optimisations du doc 14 se regroupent en 3 phases par cohérence technique.
Ordre = impact benchmark décroissant.

### Phase 1 — Merge incrémental + découplage commit/merge

**Objectif** : le merge ne bloque plus l'acteur. Le commit retourne immédiatement.
L'indexation et le merge se chevauchent (pipeline overlap).

Ces trois items du doc 14 (#1, #2, #3) forment un bloc indissociable — le
découplage commit/merge et le pipeline overlap découlent directement du merge
incrémental.

**Étapes** :

1. **Corriger sémantique `poll_idle`** (doc 08 §2)
   - Aujourd'hui `poll_idle` retourne `Poll::Pending` (= rien à faire) par
     défaut. Mais la convention dans le scheduler est inversée par rapport à
     ce qu'on attend pour le merge incrémental.
   - Clarifier : `Poll::Ready(())` = "j'ai fait du travail, rappelle-moi",
     `Poll::Pending` = "rien à faire, ne me rappelle pas".

2. **Créer `MergeState` — state machine incrémentale**
   - `MergeState::new(readers, serializer, segments)` — initialise l'état
   - `MergeState::step(&mut self, budget: usize) -> StepResult`
     - `StepResult::Continue` — budget épuisé, rappeler plus tard
     - `StepResult::Done(SegmentEntry)` — merge terminé
   - Le coeur : refactorer `IndexMerger::write()` pour exposer la boucle
     interne en version itérative. Les `SegmentReader` et `SegmentSerializer`
     sont déjà stateful — il faut juste ne pas cacher la boucle dans un seul
     appel.

3. **Intégrer dans SegmentUpdaterActor**
   - Nouveau champ : `merge_state: Option<MergeState>`
   - `consider_merge_options()` ne lance plus `run_merge()` en blocking —
     il crée un `MergeState` et le stocke
   - `poll_idle()` appelle `merge_state.step(BUDGET)` et traite le résultat
   - Le scheduler intercale les messages (AddSegment, Commit) entre les steps
   - `handle_commit()` retourne immédiatement après save_metas + GC

4. **Events d'observabilité merge** (doc 14 §Observabilité)
   - `MergeStarted { segment_ids, target_opstamp }`
   - `MergeStepCompleted { segment_ids, docs_processed, docs_total }`
   - `MergeCompleted { segment_ids, duration, result_num_docs }`
   - `MergeFailed { segment_ids, error }`

5. **Benchmark avant/après**
   - Throughput single-thread : indexation de N docs avec merges
   - Latence commit : temps entre appel et retour
   - Comparer 1 thread vs 2 threads vs 4 threads

**Impact attendu** : ×2 throughput single-thread, latence commit divisée par
10-100× (plus d'attente merge), WASM ne gèle plus pendant les merges.

---

### Phase 2 — Qualité scheduler + nettoyage

**Objectif** : réduire la consommation CPU, supprimer la dette technique,
ajouter du tooling de test.

**Étapes** :

6. **Réduire le spin de `wait_cooperative`** (doc 14 #4)
   - Remplacer le tight loop `run_one_step() → yield_now()` par `park/unpark`
     ou condvar avec timeout
   - Le thread se parke quand il n'y a rien à faire, se réveille quand la
     Reply arrive ou quand un acteur a du travail
   - Impact : consommation CPU/batterie, pertinent en WASM

7. **Supprimer `FutureResult` et `oneshot`** (doc 14 §Nettoyage)
   - `src/future_result.rs` — le module entier + tests
   - `src/directory/watch_event_router.rs` — `broadcast()` → utiliser Reply
   - `src/core/executor.rs` — `oneshot::channel()` → Reply
   - Bénéfice : supprimer la dépendance `futures-channel`

8. **Test tooling**
   - `assert_completes_within(duration, || { ... })` — timeout pour tests
     impliquant des merges
   - Subscriber de test qui collecte les events dans un `Vec` pour assertions

9. **Health check / watchdog**
   - Si un acteur n'a pas traité de message depuis N secondes, émettre
     `ActorStalled { actor_name, idle_since }`
   - Implémentable via timestamp par acteur mis à jour à chaque `handle()`

10. **Nettoyage warnings**
    - `num_merge_threads` dans `IndexWriterOptions` — repurposer ou retirer
    - `scheduler` field dans `IndexWriter` — vérifier si toujours nécessaire

---

### Phase 3 — Optimisations conditionnelles (si benchmarks le justifient)

**Objectif** : micro-optimisations à ne faire que si les mesures montrent un
problème réel.

11. **Backpressure adaptative** (doc 14 #5)
    - Retourner un signal de backpressure au lieu de bloquer quand la mailbox
      est pleine
    - Devrait être largement résolu par le merge incrémental (l'acteur reste
      réactif)

12. **Work-stealing / contention Mutex** (doc 14 #6)
    - Marginal avec ~6 acteurs et N≤4 threads
    - À mesurer si on monte à N=8+ threads

13. **Affinité acteur-thread** (doc 14 #7)
    - Micro-optimisation cache locality
    - Improbable que ce soit un bottleneck avec si peu d'acteurs

---

## Ordre d'implémentation

```
Phase 1 (merge incrémental)
  ├── 1. poll_idle sémantique     ← prérequis
  ├── 2. MergeState               ← coeur du travail
  ├── 3. Intégration actor        ← câblage
  ├── 4. Events observabilité     ← parallélisable avec 3
  └── 5. Benchmark                ← validation

Phase 2 (qualité)
  ├── 6. wait_cooperative spin    ← indépendant
  ├── 7. FutureResult cleanup     ← indépendant
  ├── 8. Test tooling             ← après events (Phase 1.4)
  ├── 9. Health check             ← après test tooling
  └── 10. Warnings                ← trivial

Phase 3 (conditionnel)
  └── 11-13. Selon benchmarks Phase 1.5
```

Phase 1 est le gros morceau (80% de l'impact). Phase 2 est du nettoyage qui
peut se faire en parallèle ou après. Phase 3 ne se fait que si les chiffres
le demandent.

---

## Prochaine action

Commencer par l'étape 1 : clarifier la sémantique de `poll_idle` dans le
scheduler et le trait Actor. C'est le prérequis pour que le merge incrémental
puisse utiliser `poll_idle()` correctement.
