# Design — Observabilité luciole (scheduler + acteurs)

Date : 18 mars 2026
Status : design

## Problème

Quand le bench bloque, on ne sait pas pourquoi :
- Le `wait_cooperative` boucle indéfiniment sans dire ce qu'il attend
- On ne sait pas quel acteur est occupé par quelle opération
- On ne sait pas combien de messages sont en queue
- Le seul diagnostic disponible c'est `ps aux` (CPU/RAM) et les eprintln manuels

Ça nous a bloqué plusieurs fois cette session :
- Blocage intermittent à 85K docs — cause inconnue
- Merge de 10K docs bloque le commit pendant >5min
- Impossible de distinguer un deadlock d'une opération lente

## Design proposé

### 1. Nommer les waits (`wait_cooperative_named`)

```rust
// Avant
rx.wait_cooperative(|| scheduler.run_one_step())

// Après
rx.wait_cooperative("commit shard_0", || scheduler.run_one_step())
```

Signature :
```rust
pub fn wait_cooperative<F>(self, label: &str, run_step: F) -> T
```

Quand le wait dépasse un seuil (configurable, défaut 10s), le warning
inclut le label + un dump de l'état du scheduler :
```
[luciole] WARNING: "commit shard_0" waiting >10s
  Scheduler dump:
    actor "segment_updater_0": BUSY "merge_step" (8.2s) | queue: 0
    actor "segment_updater_1": idle | queue: 0
    actor "shard_0": BUSY "handle_commit" (10.1s) | queue: 3
    actor "shard_1": idle | queue: 1
```

### 2. Actor activity tracking

Chaque slot d'acteur dans le scheduler a un champ activité :

```rust
struct ActorSlot {
    actor: Option<Box<dyn AnyActor>>,
    mailbox: AnyMailbox,
    // Nouveau :
    activity: AtomicActivity,
}

struct AtomicActivity {
    label: AtomicPtr<u8>,      // pointeur vers &'static str (ou null = idle)
    since: AtomicU64,          // Instant encodé en nanos depuis epoch
}
```

Le scheduler met à jour l'activité au début/fin de chaque dispatch :
```rust
// Dans scheduler.rs, run_one_step() :
slot.activity.set("merge_step");
let status = actor.handle_batch(...);
slot.activity.clear();
```

Les acteurs peuvent aussi mettre à jour l'activité eux-mêmes pour les
opérations longues :
```rust
// Dans merge_state.rs, do_step() :
actor_state.set_activity("merge_step:postings");
let result = self.step_postings();
actor_state.set_activity("merge_step:sfx");
let result = self.step_sfx();
```

### 3. Mailbox depth

`Mailbox::len()` retourne le nombre de messages en attente.
Déjà disponible via le receiver crossbeam/flume interne.

### 4. Scheduler dump

Fonction publique `scheduler.dump_state() -> String` qui retourne l'état
de tous les acteurs :

```rust
pub fn dump_state(&self) -> String {
    let mut out = String::new();
    for (id, slot) in &self.actors {
        let activity = slot.activity.get();
        let queue_len = slot.mailbox.len();
        let line = match activity {
            Some((label, since)) => {
                let elapsed = since.elapsed().as_secs_f64();
                format!("  actor {}: BUSY {:?} ({:.1}s) | queue: {}\n",
                    slot.name, label, elapsed, queue_len)
            }
            None => format!("  actor {}: idle | queue: {}\n", slot.name, queue_len),
        };
        out.push_str(&line);
    }
    out
}
```

### 5. Intégration avec wait_cooperative

```rust
pub fn wait_cooperative<F>(self, label: &str, run_step: F) -> T
where F: FnMut() -> bool
{
    let start = Instant::now();
    let warn_threshold = Duration::from_secs(
        std::env::var("LUCIVY_WAIT_WARN_SECS")
            .ok().and_then(|v| v.parse().ok())
            .unwrap_or(10)
    );
    let mut warned = false;
    loop {
        // ... existing check logic ...
        if !warned && start.elapsed() >= warn_threshold {
            let dump = global_scheduler().dump_state();
            eprintln!("[luciole] WARNING: {:?} waiting >{:.0}s\n{}",
                label, warn_threshold.as_secs_f64(), dump);
            warned = true;
        }
        // ... existing run_step logic ...
    }
}
```

### 6. LUCIVY_DEBUG intégration

Le dump n'est émis que si `LUCIVY_DEBUG=1` ou si le seuil est dépassé
(toujours émis en cas de warning, même sans debug).

Le `lucivy_trace!` existant est utilisé pour les logs normaux.
Les warnings de timeout utilisent `eprintln!` directement (toujours visibles).

## Implémentation — par ordre de priorité

### Phase 1 : wait_cooperative_named + timeout warning (30min)
- Modifier `ReplyReceiver::wait_cooperative` pour accepter un label
- Ajouter le warning avec timer
- Mettre à jour tous les appels dans sharded_handle.rs et segment_updater.rs

### Phase 2 : actor activity tracking (1h)
- Ajouter `AtomicActivity` au scheduler slot
- Mettre à jour au début/fin de dispatch
- Exposer `dump_state()`

### Phase 3 : mailbox depth (15min)
- Exposer `Mailbox::len()` et `AnyMailbox::len()`
- Inclure dans dump_state()

### Phase 4 : granular activity labels dans les acteurs (30min)
- MergeState : label par phase (init, postings, store, fast_fields, sfx, close)
- SegmentUpdaterActor : label par message (commit, add_segment, merge_step)
- ShardActor : label par message (insert, commit, search)

## Fichiers concernés

| Fichier | Changements |
|---------|-------------|
| `luciole/src/reply.rs` | wait_cooperative_named + timeout |
| `luciole/src/scheduler.rs` | AtomicActivity + dump_state() |
| `luciole/src/mailbox.rs` | len() exposé |
| `lucivy_core/src/sharded_handle.rs` | labels sur les wait_cooperative |
| `src/indexer/segment_updater_actor.rs` | labels sur les wait_cooperative |
| `src/indexer/merge_state.rs` | activity labels par phase |

## Impact attendu

Avec cette observabilité, quand le bench bloque on verra immédiatement :
```
[luciole] WARNING: "commit_fast shard_2" waiting >10s
  actor "segment_updater_2": BUSY "merge_step:postings" (12.3s) | queue: 1
  actor "shard_0": idle | queue: 0
  actor "shard_1": idle | queue: 0
  actor "shard_2": BUSY "handle_commit_fast" (10.1s) | queue: 0
  actor "shard_3": idle | queue: 0
```

→ On sait que shard_2 attend son commit, qui attend le segment_updater_2
  qui est en train de merger des postings depuis 12.3s. C'est pas un
  deadlock, c'est juste une opération lente. On peut décider : augmenter
  le timeout, réduire la taille des segments, ou optimiser les postings.
