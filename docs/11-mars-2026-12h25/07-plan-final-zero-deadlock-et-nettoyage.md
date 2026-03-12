# Doc 07 — Plan final : zéro deadlock structurel + nettoyage complet

**Date** : 11 mars 2026
**Branche** : `scheduler-beta`
**Réf** : doc `06` (EventBus générique), doc `01` (plan Phase 1-2-3)

---

## Objectif

Finir tout ce qui reste pour que l'architecture actor soit **complète,
cohérente, sans pattern pouvant deadlock, et sans dette technique**.
Pas de travail à moitié fait.

---

## État actuel — ce qui est fait

| Item | Statut |
|------|--------|
| poll_idle sémantique (étape 1) | ✅ |
| MergeState state machine (étape 2) | ✅ |
| Intégration actor / poll_idle (étape 3) | ✅ |
| DrainMerges (fix wait_merging_threads) | ✅ |
| Nettoyage census::Inventory | ✅ |
| EventBus<E> générique (étape 4) | ✅ |
| IndexEvent + émissions merge/commit | ✅ |
| Test intégration events | ✅ |

---

## Ce qui reste — 6 chantiers

### 1. StartMerge non-blocking — supprimer le dernier path blocking

**Problème** : `IndexWriter::merge(&segments)` envoie `StartMerge` à l'acteur.
Le handler appelle `run_merge_blocking()` qui bloque l'acteur pendant toute la
durée du merge. Pendant ce temps, l'acteur ne traite aucun message.

**Fichiers** :
- `src/indexer/segment_updater_actor.rs` : `handle_start_merge`, `run_merge_blocking`
- `src/indexer/segment_updater.rs` : `schedule_merge` (envoie `StartMerge`)

**Fix** : Le merge explicite passe par la même mécanique que les merges
automatiques — file + poll_idle. Mais avec une reply pour notifier le caller.

Nouveau message :
```rust
StartMerge {
    merge_operation: MergeOperation,
    reply: Reply<crate::Result<Option<SegmentMeta>>>,
}
```

Le handler :
1. Valide les segments (start_merge)
2. Crée un MergeState
3. Stocke le merge comme **prioritaire** (passe avant les merges auto)
4. Retourne `ActorStatus::Continue` sans bloquer

poll_idle exécute les steps. Quand le merge prioritaire se termine,
envoie la reply.

Le caller utilise déjà `wait_cooperative` → pas de changement côté caller.

Nouveau champ dans `SegmentUpdaterActor` :
```rust
/// Merge explicite en cours (prioritaire sur les merges auto).
explicit_merge: Option<ExplicitMerge>,
```
```rust
struct ExplicitMerge {
    merge_operation: MergeOperation,
    state: MergeState,
    start_time: Instant,
    reply: Reply<crate::Result<Option<SegmentMeta>>>,
}
```

**poll_idle** : traite d'abord `explicit_merge`, puis `active_merge`.

**Suppression** : `run_merge_blocking()` entièrement supprimé.

**Impact** : l'acteur reste réactif pendant les merges explicites. Le caller
attend toujours (via wait_cooperative) mais le scheduler peut intercaler
d'autres messages.

---

### 2. wait_blocking → wait_cooperative partout

**Problème** : `wait_blocking()` parke le thread. En contention (tests en
parallèle), le thread peut ne jamais se réveiller assez vite → timeout / hang.
En single-thread, c'est un deadlock garanti si l'acteur cible est sur le même
thread.

**Usages de `wait_blocking` en code de production** :

| Fichier | Ligne | Contexte |
|---------|-------|----------|
| `index_writer.rs:481` | `rollback()` | Flush des IndexerActors |
| `index_writer.rs:533` | `prepare_commit()` | Flush des IndexerActors |

Ces deux endroits flush les IndexerActors avant un rollback ou un commit.
Le caller est sur le thread de l'utilisateur, pas dans le scheduler → le
thread se parke au lieu de faire du travail utile.

**Fix** : remplacer par `wait_cooperative(|| scheduler.run_one_step())`.
L'IndexWriter a déjà accès au scheduler via `self.segment_updater.scheduler`.

Il faut exposer le scheduler dans IndexWriter ou passer par le
SegmentUpdater.

**Usages de `wait_blocking` dans les tests** :

| Fichier | Lignes | Test |
|---------|--------|------|
| `scheduler.rs:610` | `test_batch_processing` | |
| `scheduler.rs:637` | `test_multi_thread` | |
| `scheduler.rs:670,674` | `test_multiple_actors` | |
| `scheduler.rs:760` | `test_events_received` | |
| `scheduler.rs:794` | `test_poll_idle_incremental` | |
| `reply.rs:63,101` | tests unitaires reply | |

**Fix** : pour les tests du scheduler qui ont accès direct au scheduler,
remplacer par `wait_cooperative(|| scheduler.run_one_step())`.
Pour les tests de reply.rs qui n'ont pas de scheduler, garder
`wait_blocking` (c'est des tests unitaires simples avec un thread qui
envoie la reply — pas de risque de deadlock).

---

### 3. MergeStepCompleted — progression dans les events

**Problème** : `MergeStepCompleted` est défini dans `IndexEvent` mais jamais
émis. Pour l'émettre, il faut que `MergeState` expose des compteurs de
progression.

**Fichier** : `src/indexer/merge_state.rs`

`MergeState` a déjà `estimated_steps()`. Il faut ajouter :
- `docs_processed() -> u32` — compteur incrémenté à chaque step
- `docs_total() -> u32` — total calculé à l'init

**Fichier** : `src/indexer/segment_updater_actor.rs`

Émettre `MergeStepCompleted` dans `poll_idle` après chaque
`StepResult::Continue`, et dans `drain_all_merges` (optionnel — drain est
synchrone, mais les events restent utiles pour le monitoring).

---

### 4. Supprimer FutureResult et oneshot

**Problème** : `FutureResult` wrape un `oneshot::Receiver` de
`futures-channel`. Redondant avec le système `Reply` du framework actor.

**Usages restants** :

| Fichier | Usage |
|---------|-------|
| `src/future_result.rs` | Module entier — struct + tests |
| `src/lib.rs:170,177` | `mod future_result` + `pub use FutureResult` |
| `src/directory/watch_event_router.rs:77` | `broadcast() -> FutureResult<()>` |
| `src/directory/tests.rs:245,263` | `oneshot::channel()` |
| `src/core/executor.rs:115` | `oneshot::channel()` pour résultats search |

**Plan** :

a. `watch_event_router.rs` : `broadcast()` retourne un `Reply<()>` ou juste
   exécute synchrone + `Result<()>`. La sémantique actuelle crée un oneshot
   pour chaque callback — surdimensionné.

b. `executor.rs` : le search parallèle utilise `oneshot::channel()` pour
   collecter les résultats de chaque segment. Remplacer par `Reply` ou par
   un simple `crossbeam::channel::bounded(1)` (déjà en dépendance).

c. `directory/tests.rs` : adapter aux nouveaux patterns.

d. Supprimer `src/future_result.rs` et le `pub use` dans `lib.rs`.

e. Retirer `futures-channel` du `Cargo.toml` si plus utilisé.

**Note** : vérifier que `FutureResult` n'est pas utilisé dans le code
public (API utilisateur). Le `pub use` dans lib.rs suggère que oui →
c'est un breaking change si on le supprime. Si c'est le cas, on peut le
deprecate au lieu de le supprimer.

---

### 5. Nettoyage warnings

**Warnings actuels** (cargo check) :

| Warning | Fix |
|---------|-----|
| `num_merge_threads` dans `IndexWriterOptions` | Supprimer ou repurposer — les merges sont dans l'acteur, plus de threads dédiés |
| `estimated_steps` unused dans `MergeState` | Sera utilisé par MergeStepCompleted (chantier 3) |
| `merge_incremental` unused | Vérifier si encore référencé, sinon supprimer |
| `missing_docs` sur `MergeOperation` methods | Ajouter les docs |

---

### 6. Tests fiables

**Problème** : les tests se bloquent quand ils tournent en parallèle
(contention CPU entre les multiples schedulers/threads créés par chaque test).

**Fixes** :

a. **Chantier 2** (wait_cooperative partout) résout la majorité — les threads
   ne se parkent plus inutilement.

b. **Timeouts dans les tests** : helper `assert_completes_within` :
```rust
fn assert_completes_within<T>(
    duration: Duration,
    f: impl FnOnce() -> T
) -> T {
    // Lance f() dans un thread, panique si timeout
}
```

c. **Script de test** : garder `run_tests.sh` avec `--test-threads=4` et
   `stdbuf -oL` pour suivi temps réel.

d. **Documenter** : `cargo test -- --test-threads=4` recommandé dans le
   README ou CLAUDE.md du projet.

---

## Ordre d'exécution

```
1. StartMerge non-blocking          ← fix structurel (#1)
   └── cargo check
2. wait_blocking → wait_cooperative  ← fix contention (#2)
   └── cargo check
3. MergeStepCompleted               ← observabilité (#3)
   └── cargo check
4. Nettoyage warnings               ← cosmétique (#5)
   └── cargo check
5. Tests (run complet)              ← validation (#6)
6. FutureResult                     ← nettoyage (#4, peut être séparé)
```

Les chantiers 1-2 sont les plus impactants. Le 3 est du polish. Le 4
(FutureResult) est indépendant et peut être un commit séparé vu le scope
(search, directory, API publique).

---

## Règle structurelle — zéro deadlock

Après les chantiers 1-2, l'architecture respecte la règle :

> **Ne jamais bloquer dans `handle()`. Tout travail long passe par
> `poll_idle()` en steps incrémentaux.**

Conséquences :
- L'acteur reste réactif à tout moment
- Les callers utilisent `wait_cooperative` → font du travail utile en attendant
- Pas de dépendance circulaire entre acteurs (un seul SegmentUpdaterActor)
- Pas de condvar, pas de `wait_until_empty`, pas de thread parking dans le
  pipeline d'indexation

---

## Ce qui n'est PAS dans ce plan

- **Phase 2 step 6** (wait_cooperative spin → park/unpark) — optimisation
  CPU, pas un correctif. À faire après les benchmarks.
- **Phase 3** (backpressure, work-stealing, affinité) — conditionnel aux
  benchmarks.
- **Benchmarks** (Phase 1 step 5) — à faire quand tout le reste est clean.
