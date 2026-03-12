# Doc 08 — Rapport de progression : chantiers 1-3 + global scheduler

**Date** : 11 mars 2026
**Branche** : `scheduler-beta`
**Réf** : doc `07` (plan final zéro deadlock)

---

## Chantiers terminés

### Chantier 1 : StartMerge non-blocking ✅

**Fichier** : `src/indexer/segment_updater_actor.rs`

Les merges explicites (IndexWriter::merge()) passent maintenant par `poll_idle`
au lieu de bloquer le handler.

Modifications :
- Nouveau struct `ExplicitMerge` avec `reply`, `state`, `start_time`
- `handle_start_merge` crée un `MergeState` et le stocke sans bloquer
- `poll_idle` traite `explicit_merge` en priorité, puis `active_merge`
- `finish_explicit_merge` envoie le résultat via la reply
- `drain_all_merges` gère aussi les merges explicites
- `run_merge_blocking()` et `extract_panic_message()` supprimés

### Chantier 2 : wait_blocking → wait_cooperative ✅ (puis → wait_blocking, voir ci-dessous)

**Fichiers** : `index_writer.rs`, `scheduler.rs` (tests)

Initialement converti `wait_blocking` → `wait_cooperative` dans `rollback()`
et `prepare_commit()`. Puis annulé au profit de `wait_blocking` avec le
global scheduler (voir section dédiée).

### Chantier 3 : MergeStepCompleted ✅

**Fichiers** : `merge_state.rs`, `segment_updater_actor.rs`, `events.rs`

- `MergeState` a maintenant `steps_completed: u32` (incrémenté à chaque step)
- `estimated_steps()` retourne `u32`
- `IndexEvent::MergeStepCompleted` avec `steps_completed` / `steps_total`
- `poll_idle` émet `MergeStepCompleted` via `emit_step_completed()` helper
- Zero-cost quand personne n'écoute (check `has_subscribers`)

---

## Diagnostic des tests qui bloquent

### Problème identifié : spin-wait dans `wait_cooperative`

`wait_cooperative` était un spin pur :
```rust
loop {
    match try_recv() {
        Ok(val) => return val,
        Err(_) => run_step(), // spin infini si pas de travail
    }
}
```

`run_one_step_impl` faisait `thread::yield_now()` quand pas de travail —
quasi-noop. Résultat : le thread caller consommait 100% CPU en attendant
la reply, affamant les threads du scheduler qui faisaient le vrai travail.

**Preuve** : le code pré-actor (commit `27b266a`) passait 1066 tests en 77s
avec `--test-threads=4`. Le code actor bloquait après ~217 tests.

### Cause racine : multiplication des threads

Chaque `IndexWriter::new` créait son propre `Scheduler::new(N)` avec N threads.
Avec 4 tests en parallèle, chacun créant un IndexWriter :
- 4 schedulers × 2 threads = 8 scheduler threads
- 4 test threads en spin-wait
- = 12 threads pour ~4 cores → famine CPU

L'ancien code (rayon) utilisait un **pool global partagé** — même design qu'on
veut atteindre.

---

## Solution : Global Scheduler

### Implémentation

**Fichier** : `src/actor/scheduler.rs`

```rust
static GLOBAL_SCHEDULER: OnceLock<GlobalSchedulerState> = OnceLock::new();

pub(crate) fn global_scheduler() -> &'static Arc<Scheduler> {
    // Lazy init, num_threads = available_parallelism()
}
```

- Un seul pool de threads pour tout le process (comme rayon)
- Initialisé lazy au premier usage
- Les acteurs de tous les IndexWriters cohabitent dans le même scheduler
- Les threads persistent — `IndexWriter::drop` ne les touche plus

### Changements dans IndexWriter

**Fichier** : `src/indexer/index_writer.rs`

- `scheduler` et `scheduler_handle` supprimés du struct
- `IndexWriter::new` utilise `global_scheduler()` au lieu de `Scheduler::new()`
- `Drop` envoie Kill/Shutdown aux acteurs sans toucher au scheduler

### Changements dans SegmentUpdater

**Fichier** : `src/indexer/segment_updater.rs`

- Champ `scheduler` supprimé du struct
- Toutes les attentes passent de `wait_cooperative` à `wait_blocking`
- Le caller parke pour de vrai (condvar crossbeam) — zero CPU gaspillé
- Les threads du global scheduler traitent les messages

### wait_cooperative : rôle réduit

**Fichier** : `src/actor/reply.rs`

`wait_cooperative` reste disponible pour les tests unitaires du scheduler
qui créent des schedulers locaux sans threads. Le code production utilise
`wait_blocking` exclusivement.

`run_one_step()` retourne maintenant `bool` (travail effectué ou non).
Quand pas de travail, `wait_cooperative` fait `recv_timeout(1ms)` au lieu
de spin.

---

## État des tests

Tests en cours d'exécution avec le global scheduler. Résultats attendus
significativement meilleurs car :
1. Un seul pool de threads (pas de multiplication)
2. `wait_blocking` dans le code production (pas de spin)
3. `recv_timeout(1ms)` dans `wait_cooperative` (tests scheduler)

Le code pré-actor faisait 1066 tests / 77s / 0 failures.

---

## Chantiers restants (du doc 07)

| # | Chantier | Statut |
|---|----------|--------|
| 1 | StartMerge non-blocking | ✅ |
| 2 | wait_blocking → wait_cooperative | ✅ (devenu wait_blocking + global scheduler) |
| 3 | MergeStepCompleted | ✅ |
| 4 | Supprimer FutureResult | ❌ à faire |
| 5 | Nettoyage warnings | ❌ à faire |
| 6 | Tests fiables | 🔄 en cours (global scheduler résout la cause racine) |

### Nouveau chantier identifié : Global Scheduler

Non prévu dans le doc 07 mais critique. Résout les chantiers 2 et 6
simultanément. Le design "un scheduler par IndexWriter" était la cause
racine des problèmes de tests et de la famine CPU.

---

## Fichiers modifiés (depuis le commit 62b6b14)

| Fichier | Changements |
|---------|-------------|
| `src/actor/scheduler.rs` | EventBus générique, global_scheduler(), run_one_step → bool |
| `src/actor/reply.rs` | wait_cooperative avec backoff (recv_timeout) |
| `src/actor/events.rs` | EventBus<E> générique |
| `src/indexer/segment_updater_actor.rs` | ExplicitMerge, poll_idle prioritaire, events, drain |
| `src/indexer/segment_updater.rs` | wait_blocking, suppression champ scheduler |
| `src/indexer/index_writer.rs` | Global scheduler, suppression scheduler/handle |
| `src/indexer/events.rs` | Nouveau — IndexEvent enum |
| `src/indexer/merge_state.rs` | steps_completed, estimated_steps u32 |
| `src/indexer/mod.rs` | pub mod events |

---

## NOTE POUR LA SUITE (post-compression)

### Problème non résolu : tests d'agrégation bloquent encore

Avec le global scheduler + `wait_cooperative` avec `recv_timeout(1ms)`, on
passe 58/1093 tests puis ça bloque. Les tests d'agrégation lourds
(`test_aggregation_flushing_variants`, etc.) restent stuck.

**Approches tentées et résultats :**

| Approche | Résultat |
|----------|----------|
| wait_cooperative spin pur | 217 OK puis bloque (famine CPU) |
| wait_cooperative + yield_now | non testé complètement (killé) |
| wait_cooperative + recv_timeout(100µs) + global scheduler | 43 OK puis bloque |
| wait_blocking pur (global scheduler) | 24 OK puis deadlock (threads occupés) |
| wait_cooperative + recv_timeout(1ms) + global scheduler | 58 OK puis bloque |

**Pré-actor (commit 27b266a)** : 1066 OK en 77s avec --test-threads=4.

### Hypothèse à investiguer

Le problème est probablement que les **merges incrémentaux dans `poll_idle`**
occupent les threads du scheduler en continu. Chaque step retourne
`Poll::Ready(())` → le batch handler continue la boucle → le thread ne se
libère jamais pour traiter d'autres messages (Flush, Commit).

Dans le code original (commit 62b6b14), `consider_merge_options()` faisait
les merges synchrones DANS le handler, AVANT d'envoyer la reply du commit.
C'était blocking mais cohérent — le commit n'était "fait" qu'après les merges.

Avec nos merges incrémentaux, le commit reply est envoyé AVANT les merges.
Les merges tournent en background via poll_idle. Mais poll_idle monopolise
les threads via la boucle batch (BATCH_SIZE=32 steps avant de yield).

**Piste de fix** : dans `handle_batch`, après chaque `poll_idle() → Ready`,
yield le thread (return HasMore) pour laisser d'autres acteurs être schedulés.
Ou réduire le BATCH_SIZE pour les poll_idle steps.

### État du code actuel

Le code compile et est cohérent. Les changements structurels (chantiers 1-3,
global scheduler, events) sont tous en place. Le seul problème restant est
la contention des threads du scheduler pendant les merges incrémentaux.

**Fichiers à relire en priorité après compression :**
- `src/actor/scheduler.rs` (run_loop, handle_batch, poll_idle integration)
- `src/actor/reply.rs` (wait_cooperative)
- `src/indexer/segment_updater_actor.rs` (poll_idle, explicit/active merge)

**Tests en cours** : run `bv3thyusr`, `tail -f /tmp/lucivy_tests7.log`
