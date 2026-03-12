# Doc 09 — Self-messages, diagnostic deadlock, debug events

**Date** : 12 mars 2026
**Branche** : `scheduler-beta`
**Réf** : doc `08` (rapport progression chantiers)

---

## Résumé de la session

Objectif : résoudre les tests qui bloquent (58/1093 puis 24/1093 selon config).
On a identifié **trois causes racine distinctes**, implémenté des fixes, et ajouté
un système de debug par events pour diagnostiquer le deadlock restant.

---

## Cause racine 1 : handle_batch monopolise les threads (BATCH_SIZE=32)

**Fichier** : `src/actor/scheduler.rs`

Dans `handle_batch`, quand la mailbox est vide et `poll_idle()` retourne `Ready`,
la boucle continuait pour 32 itérations → un seul acteur monopolisait le thread.

**Fix** : `break` après chaque `poll_idle Ready` au lieu de `{}` (continue).
Résultat : 58 → 78 tests OK. Insuffisant.

De plus, le check post-boucle appelait `actor.poll_idle().is_ready()` avec
side effect (exécutait un vrai merge step juste pour checker). Remplacé par
`BatchResult::HasMore` inconditionnel.

---

## Cause racine 2 : schedule_add_segment bloque dans un handler d'acteur

**Fichier** : `src/indexer/segment_updater.rs` — `schedule_add_segment()`

`schedule_add_segment()` faisait `wait_cooperative(|| scheduler.run_one_step())`
pour attendre la reply de `AddSegment`. Mais cette méthode est appelée depuis
`IndexerActor::handle_flush` → **depuis un thread du scheduler**.

Avec 4 tests parallèles, 4 IndexerActors font handle_flush → schedule_add_segment
→ bloquent 4 threads du scheduler. Aucun thread libre pour traiter les messages
→ **deadlock**.

**Fix** : `AddSegment` est maintenant **fire-and-forget** (pas de reply).
Le message `SegmentUpdaterMsg::AddSegment` n'a plus de champ `reply`.

Changements :
- `segment_updater.rs` : `schedule_add_segment()` fait juste `send()` + `Ok(())`
- `segment_updater_actor.rs` : `AddSegment { entry }` sans reply
- `handle_add_segment(&mut self, entry)` sans reply

---

## Cause racine 3 : poll_idle monopolise les threads (design fondamental)

Même avec le fix BATCH_SIZE, les merges dans `poll_idle` monopolisent les threads.
L'acteur retourne `HasMore` → remis dans la ready queue → immédiatement repris.
Avec priorité `Medium`, le SegmentUpdaterActor passe avant les IndexerActors idle (`Low`).

### Solution : self-messages

**Design** : les merges ne passent plus par `poll_idle` mais par des self-messages
`MergeStep` dans la mailbox normale.

```rust
enum SegmentUpdaterMsg {
    // ... existant ...
    MergeStep,  // self-message : avance un step de merge
}
```

L'acteur stocke un `self_ref: Option<ActorRef<SegmentUpdaterMsg>>` initialisé
dans `on_start()`.

Quand un merge démarre → `self.schedule_merge_step()` → `self_ref.send(MergeStep)`.
Dans `handle(MergeStep)` → fait un step → se re-envoie `MergeStep` si pas fini.

Les merge steps passent par la mailbox normale → interleaved avec les autres
messages (Flush, Commit, AddSegment) → plus de monopolisation.

`poll_idle()` ne fait plus rien → retourne toujours `Poll::Pending`.

**Changement API du trait Actor** :

```rust
// Avant :
fn on_start(&mut self) {}

// Après :
fn on_start(&mut self, self_ref: ActorRef<Self::Msg>) {}
```

Le scheduler passe un clone de l'`ActorRef` à `on_start()` après que le
`wake_handle` soit attaché (ligne 234 de scheduler.rs).

### Race condition identifiée : self-messages et is_idle

Quand l'acteur s'envoie un self-message via `self_ref.send()`, le `is_idle` flag
est `false` (l'acteur est en cours de traitement). Donc le `send()` ne fait **pas**
de wake. Le message est dans la mailbox mais le scheduler ne sait pas qu'il est là.

Si le batch se termine et que `try_handle_one()` retourne `None` + `poll_idle()`
retourne `Pending`, l'acteur passe en `Idle` avec un message en attente → **deadlock**.

**Fix** : dans `handle_batch`, avant de conclure `Idle`, vérifier `has_pending()` :

```rust
None => {
    if actor.has_pending() {
        break; // → HasMore (il y a un self-message en attente)
    }
    match actor.poll_idle() {
        Poll::Ready(()) => break,
        Poll::Pending => return BatchResult::Idle,
    }
}
```

### wait_blocking partout (sauf schedule_add_segment qui est fire-and-forget)

Tous les `wait_cooperative(|| scheduler.run_one_step())` dans `segment_updater.rs`
et `index_writer.rs` ont été remplacés par `wait_blocking()`.

Raison : ces appels sont faits depuis le **test/caller thread**, pas depuis un
thread du scheduler. Les scheduler threads sont là pour traiter les messages.
Le test thread n'a pas besoin de pomper le scheduler.

---

## Debug events : diagnostic du deadlock restant

### Problème : tests toujours stuck à 24 OK avec wait_blocking

Malgré tous les fixes ci-dessus, les tests d'agrégation bloquent encore.
12 scheduler threads + 4 test threads, tous en `futex_do_wait` (sleeping).
La ready queue est vide. Personne ne travaille.

### Outil : logger d'events du scheduler

**Fichier** : `src/actor/scheduler.rs` — `global_scheduler()`

Activé par variable d'environnement :
- `LUCIVY_SCHEDULER_DEBUG=1` → log sur stderr
- `LUCIVY_SCHEDULER_DEBUG=/path/to/file` → log dans un fichier

```rust
if let Ok(debug_val) = std::env::var("LUCIVY_SCHEDULER_DEBUG") {
    let events = scheduler.subscribe_events();
    std::thread::Builder::new()
        .name("scheduler-debug".into())
        .spawn(move || {
            use std::io::Write;
            let mut out: Box<dyn Write + Send> = if debug_val == "1" {
                Box::new(std::io::stderr())
            } else {
                Box::new(std::fs::OpenOptions::new()
                    .create(true).append(true).open(&debug_val)
                    .expect("cannot open scheduler debug log"))
            };
            while let Some(event) = events.recv() {
                let _ = writeln!(out, "[sched] {event:?}");
                let _ = out.flush();
            }
        })
        .expect("failed to spawn scheduler debug thread");
}
```

Events disponibles (`SchedulerEvent`) :
- `ActorSpawned { actor_id, actor_name, mailbox_capacity }`
- `ActorWoken { actor_id, actor_name, woken_by }`
- `MessageHandled { actor_id, actor_name, duration, mailbox_depth, priority }`
- `ActorIdle { actor_id, actor_name }`
- `ActorStopped { actor_id, actor_name }`
- `PriorityChanged { actor_id, actor_name, from, to }`
- `ThreadParked { thread_index }`
- `ThreadUnparked { thread_index }`

Usage :
```bash
LUCIVY_SCHEDULER_DEBUG=/tmp/sched.log cargo test "test_aggregation_flushing_variants" -- --test-threads=1
# Attendre que ça bloque, puis :
tail -100 /tmp/sched.log
```

La trace montrera exactement quel acteur est le dernier à passer en Idle,
s'il y a des messages non-traités, et quels threads se parkent sans se réveiller.

---

## Fichiers modifiés (depuis doc 08)

| Fichier | Changements |
|---------|-------------|
| `src/actor/mod.rs` | `on_start(&mut self, self_ref: ActorRef<Self::Msg>)` |
| `src/actor/scheduler.rs` | break après poll_idle Ready, has_pending check, HasMore post-boucle, debug logger env var |
| `src/actor/events.rs` | Inchangé (déjà en place) |
| `src/indexer/segment_updater_actor.rs` | Self-messages MergeStep, self_ref, on_start, poll_idle supprimé, AddSegment sans reply |
| `src/indexer/segment_updater.rs` | schedule_add_segment fire-and-forget, wait_blocking partout |
| `src/indexer/index_writer.rs` | wait_blocking partout |

---

## État actuel et prochaine étape

Le code compile. Les self-messages sont en place. Le debug logger est prêt.

**Prochaine étape** : lancer le test bloquant avec `LUCIVY_SCHEDULER_DEBUG`
et analyser la trace pour comprendre pourquoi la ready queue se vide alors
qu'il y a des messages en attente (ou pas de messages envoyés du tout).

Hypothèse à vérifier : le `wait_blocking` dans le test thread ne réveille
peut-être personne parce que les acteurs ne sont jamais spawnés (problème
d'initialisation ?), ou les messages Flush ne sont jamais envoyés.

**Commande à lancer** :
```bash
LUCIVY_SCHEDULER_DEBUG=/tmp/sched.log cargo test "test_aggregation_flushing_variants" -- --test-threads=1
# Après ~30s de blocage :
tail -200 /tmp/sched.log
```
