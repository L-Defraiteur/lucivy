# Doc 06 — Plan : EventBus générique + events d'observabilité

**Date** : 11 mars 2026
**Branche** : `scheduler-beta`
**Réf** : doc `01` (étape 4), doc `14` du 9-mars (§Observabilité)

---

## Objectif

Rendre l'EventBus générique (`EventBus<E>`) pour séparer proprement les events
scheduler (infrastructure) des events indexer (métier), puis ajouter les events
d'observabilité merge/commit.

---

## Analyse du code existant

### `src/actor/events.rs` — EventBus actuel

L'EventBus est hardcodé sur `SchedulerEvent` :

```rust
pub(crate) struct EventBus {
    subscriber_count: AtomicUsize,
    subscribers: Mutex<Vec<channel::Sender<SchedulerEvent>>>,
}
```

La mécanique (crossbeam channels, atomic subscriber count, broadcast, zero-cost
sans listeners) est entièrement indépendante du type d'event. Le seul couplage
est le type concret `SchedulerEvent`.

**Traits requis sur E** : `Clone` (pour broadcast) + `Send` (cross-thread).

### Problème dans `Drop` de `EventReceiver`

```rust
impl Drop for EventReceiver {
    fn drop(&mut self) {
        self.bus.subscriber_count.fetch_sub(1, Ordering::Relaxed);
        let mut subs = self.bus.subscribers.lock().unwrap();
        subs.retain(|s| !s.is_empty() || s.send(SchedulerEvent::ThreadParked { thread_index: 0 }).is_ok());
    }
}
```

Le nettoyage des senders déconnectés envoie un **event dummy** (`ThreadParked`)
pour tester si le sender est encore actif. Ça fonctionne par effet de bord mais :
- Ça émet un faux event aux autres subscribers
- Ça couple le Drop au type concret (impossible en générique)

**Fix** : utiliser `s.is_disconnected()` de crossbeam-channel (non disponible)
ou simplement retirer les senders dont le receiver est droppé en vérifiant
`s.send()` avec un mécanisme propre. Solution la plus simple : **ne pas
nettoyer les senders** dans le Drop — le compteur atomique est déjà décrémenté,
et les `send()` sur un sender dont le receiver est droppé sont des no-op
silencieux (crossbeam retourne `Err`). Les senders morts seront élagués
naturellement au prochain `subscribe()` ou via un `emit()` périodique.

### Points d'utilisation actuels

| Fichier | Usage |
|---------|-------|
| `scheduler.rs:36` | `SharedState.events: Arc<EventBus>` |
| `scheduler.rs:177` | `EventBus::new()` dans constructeur |
| `scheduler.rs:153,229,...` | `shared.events.emit(SchedulerEvent::...)` |
| `scheduler.rs:264` | `subscribe_events() -> EventReceiver` |
| `scheduler.rs:765` (test) | `events.try_recv()` |

### Comment les acteurs accèdent-ils au bus ?

Actuellement, les acteurs n'accèdent **pas** au bus — seul le scheduler émet.
Pour que le `SegmentUpdaterActor` puisse émettre des `IndexEvent`, il faut lui
passer un `Arc<EventBus<IndexEvent>>` à la construction. Le chemin :

```
IndexWriter → SegmentUpdater::new(scheduler, ...) → SegmentUpdaterActor::new(shared)
```

Le `SegmentUpdaterShared` ou le constructeur de l'acteur doit recevoir le bus.
Option la plus propre : stocker le `Arc<EventBus<IndexEvent>>` dans
`SegmentUpdaterShared` (il est déjà `Arc<SegmentUpdaterShared>`).

---

## Plan d'implémentation

### Étape 1 — Rendre `EventBus<E>` générique

**Fichier** : `src/actor/events.rs`

Changements :

```rust
pub(crate) struct EventBus<E> {
    subscriber_count: AtomicUsize,
    subscribers: Mutex<Vec<channel::Sender<E>>>,
}

impl<E: Clone + Send + 'static> EventBus<E> {
    pub fn new() -> Self { ... }
    pub fn has_subscribers(&self) -> bool { ... }  // inchangé
    pub fn emit(&self, event: E) { ... }           // E au lieu de SchedulerEvent
    pub fn subscribe(self: &Arc<Self>) -> EventReceiver<E> { ... }
}

pub(crate) struct EventReceiver<E> {
    receiver: channel::Receiver<E>,
    bus: Arc<EventBus<E>>,
}
```

**Drop** : simplifier — juste décrémenter le compteur, laisser les senders
morts (no-op). Pas de dummy event.

```rust
impl<E> Drop for EventReceiver<E> {
    fn drop(&mut self) {
        self.bus.subscriber_count.fetch_sub(1, Ordering::Relaxed);
        // Les senders morts sont des no-op silencieux dans crossbeam.
        // Ils seront élagués au prochain subscribe() si nécessaire.
    }
}
```

**Impact** : aucun changement de comportement. Les tests existants compilent
en remplaçant `EventBus` par `EventBus<SchedulerEvent>` partout.

### Étape 2 — Adapter le scheduler

**Fichier** : `src/actor/scheduler.rs`

- `SharedState.events: Arc<EventBus<SchedulerEvent>>` — ajout du paramètre
- `EventBus::new()` → `EventBus::<SchedulerEvent>::new()` (ou inférence)
- `subscribe_events() -> EventReceiver<SchedulerEvent>` — ajout du paramètre
- Tests : type annotation si inférence insuffisante

Changements minimes — essentiellement ajouter `<SchedulerEvent>` là où c'est
nécessaire.

### Étape 3 — Définir `IndexEvent`

**Nouveau fichier** : `src/indexer/events.rs`

```rust
use std::time::Duration;
use crate::index::SegmentId;
use crate::Opstamp;

/// Events métier émis par l'indexer.
#[derive(Debug, Clone)]
pub enum IndexEvent {
    MergeStarted {
        segment_ids: Vec<SegmentId>,
        target_opstamp: Opstamp,
    },
    MergeStepCompleted {
        segment_ids: Vec<SegmentId>,
        docs_processed: u32,
        docs_total: u32,
    },
    MergeCompleted {
        segment_ids: Vec<SegmentId>,
        duration: Duration,
        result_num_docs: u32,
    },
    MergeFailed {
        segment_ids: Vec<SegmentId>,
        error: String,
    },
    CommitStarted {
        opstamp: Opstamp,
    },
    CommitCompleted {
        opstamp: Opstamp,
        duration: Duration,
    },
}
```

Visibilité : `pub` (on veut que les utilisateurs de la lib puissent s'abonner).

### Étape 4 — Intégrer dans SegmentUpdaterActor

**Fichier** : `src/indexer/segment_updater.rs`

- Ajouter `event_bus: Arc<EventBus<IndexEvent>>` dans `SegmentUpdaterShared`
- Créer le bus dans `SegmentUpdater::new()` :
  `event_bus: Arc::new(EventBus::new())`
- Exposer `pub fn subscribe_index_events(&self) -> EventReceiver<IndexEvent>`
  sur `SegmentUpdater` (pour les utilisateurs)

**Fichier** : `src/indexer/segment_updater_actor.rs`

Émettre aux bons endroits :

| Event | Où émettre |
|-------|------------|
| `MergeStarted` | `start_next_incremental_merge` (après start_merge OK) |
| `MergeStarted` | `run_merge_blocking` (après start_merge OK) |
| `MergeStepCompleted` | `poll_idle` (après chaque `state.step()` qui retourne Continue) |
| `MergeCompleted` | `finish_incremental_merge` (avant do_end_merge) |
| `MergeCompleted` | `run_merge_blocking` (branche Ok(Ok(...))) |
| `MergeFailed` | `run_merge_blocking` (branches Ok(Err) et Err(panic)) |
| `MergeFailed` | `start_next_incremental_merge` (branche Err) |
| `CommitStarted` | `handle_commit` (début) |
| `CommitCompleted` | `handle_commit` (fin, avec duration) |

L'accès au bus se fait via `self.shared.event_bus` — zero-cost si personne
n'écoute grâce au check `has_subscribers()`.

Pattern d'émission :

```rust
if self.shared.event_bus.has_subscribers() {
    self.shared.event_bus.emit(IndexEvent::MergeStarted {
        segment_ids: merge_op.segment_ids().to_vec(),
        target_opstamp: merge_op.target_opstamp(),
    });
}
```

### Étape 5 — Exposer à l'utilisateur

**Fichier** : `src/indexer/index_writer.rs`

Ajouter une méthode publique :

```rust
pub fn subscribe_index_events(&self) -> EventReceiver<IndexEvent> {
    self.segment_updater.subscribe_index_events()
}
```

Ceci permet aux utilisateurs de la lib (et à nous pour les tests/benchmarks)
de s'abonner aux events métier.

### Étape 6 — MergeStepCompleted : progression

Pour émettre `docs_processed` / `docs_total`, il faut que `MergeState::step()`
retourne des métriques de progression. Aujourd'hui `StepResult::Continue`
ne porte pas d'info.

Option : enrichir `StepResult::Continue` :

```rust
enum StepResult {
    Continue { docs_processed: u32, docs_total: u32 },
    Done(Option<SegmentEntry>),
}
```

Ou : méthodes accesseurs sur `MergeState` (`docs_processed()`, `docs_total()`).
La seconde option est plus souple — on lit l'état quand on veut, pas seulement
au retour de step().

---

## Ordre d'exécution

```
1. EventBus<E> générique        ← events.rs (pas de changement de comportement)
2. Adapter scheduler            ← scheduler.rs (ajout <SchedulerEvent>)
3. cargo check                  ← valider que rien ne casse
4. IndexEvent enum              ← indexer/events.rs (nouveau fichier)
5. Bus dans SegmentUpdaterShared ← segment_updater.rs
6. Émissions dans l'acteur      ← segment_updater_actor.rs
7. API publique                 ← index_writer.rs
8. cargo test                   ← valider
```

Étapes 1-3 sont indépendantes des events métier — on peut les merger
séparément si besoin.

---

## Ce qui ne change pas

- `SchedulerEvent` et ses variants — inchangés
- Le scheduler émet toujours ses events via `shared.events`
- Les tests existants du scheduler continuent de fonctionner
- `EventReceiver` garde la même API (try_recv, recv, recv_timeout, Iterator)
- Le `SegmentUpdaterActor` garde la même structure — on ajoute juste des
  `.emit()` aux endroits clés

---

## Risques

- **Aucun risque de régression** : le changement `EventBus` → `EventBus<E>` est
  purement mécanique (ajout de paramètre de type)
- **Performance** : zero-cost garanti par `has_subscribers()` check (AtomicUsize
  load Relaxed, pas de lock)
- **Le Drop simplifié** perd le nettoyage des senders morts. Impact : quelques
  bytes de mémoire pour des senders orphelins. Négligeable — et élimine le
  bug du dummy event
