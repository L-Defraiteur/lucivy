# Design : pipe_to / collect_to — Inter-actor request-reply déclaratif

## Le problème

Aujourd'hui, un acteur qui veut déléguer du travail à un autre acteur doit
câbler manuellement : Reply, ReplyReceiver, set_resume, JoinResume, Suspend,
poll_idle, pending state. C'est 10-15 lignes de plomberie par interaction,
fragile, et la sémantique ("qui attend qui, pour quoi") est noyée dans le code.

```rust
// ShardActor::handle(Commit) aujourd'hui — 15 lignes de câblage :
let flush_rxs = writer.flush_workers()?;
let join = JoinResume::new(flush_rxs.len(), ctx.resume_handle());
for rx in &flush_rxs {
    rx.set_resume(join.one_shot());
}
self.pending_commit = Some(PendingShardCommitState {
    flush_rxs, fast, reply,
});
return ActorStatus::Suspend;
// + poll_idle (20 lignes) pour collecter les résultats et finaliser
```

Problèmes :
- **Suspend + poll_idle** = machine à états manuelle. Chaque dépendance
  ajoute un variant à pending_state + une branche dans poll_idle.
- **L'intention est invisible** : on ne voit pas "j'attends que les indexers
  flushent" sans lire 30 lignes.
- **Pas composable** : enchaîner 2 dépendances (flush → commit DAG → reply)
  nécessite encore plus de pending states.
- **Deadlock possible** : si poll_idle fait du travail bloquant (execute_dag),
  le scheduler thread est capturé.

## La vision

**UN pattern pour toute communication inter-acteur** : "j'envoie cette tâche
à tel acteur (ou N acteurs), je déclare ce que j'attends, et quand c'est fait
je reçois le résultat comme un message normal dans ma mailbox."

```rust
// ShardActor::handle(Commit) — avec collect_to :
self.shard_pool.collect_to(
    |reply| WorkerMsg::Flush(reply),
    &self.self_ref, "flush_workers",
    |results| ShardMsg::AllFlushesDone { results, fast, reply },
);
return ActorStatus::Continue;

// Le résultat arrive comme un message normal :
ShardMsg::AllFlushesDone { results, fast, reply } => {
    let prepared = writer.finalize_flush_and_prepare(results)?;
    if fast { prepared.commit_fast()?; } else { prepared.commit()?; }
    reply.send(Ok(()));
    ActorStatus::Continue
}
```

Avantages :
- **L'intention est explicite** : on voit QUI fait QUOI et QUI reçoit le résultat
- **Pas de Suspend** : l'acteur reste actif (peut traiter d'autres messages)
- **Pas de poll_idle** : le résultat arrive comme un message FIFO
- **Composable** : enchaîner = pipe_to dans le handler du résultat
- **WaitGraph auto** : chaque pipe_to/collect_to s'enregistre automatiquement
- **Deadlock impossible** : aucun thread n'est bloqué à aucun moment
- **Pas de race condition** : le callback est enregistré AVANT l'envoi du message

## Invariant fondamental : callback avant envoi

**L'envoi du message et l'enregistrement du callback sont atomiques.**

Ordre garanti dans chaque primitive :
1. Créer le Reply/ReplyReceiver
2. Enregistrer le callback `on_send` sur le ReplyReceiver
3. PUIS envoyer le message (qui contient le Reply)

Comme le callback est posé AVANT que le message parte, il est impossible
que la reply arrive avant que le callback soit en place. Pas de race condition.

C'est pourquoi les méthodes sont sur `ActorRef` et `Pool` (qui font le send)
et non sur `ReplyReceiver` séparément.

## Design technique

### Structure Inner modifiée

```rust
struct Inner<T> {
    state: Mutex<State<T>>,
    ready: Condvar,
    resume: Mutex<Option<ResumeHandle>>,
    on_send: Mutex<Option<Box<dyn FnOnce(T) + Send>>>,  // NOUVEAU
}
```

### Modification de Reply::send()

```rust
pub fn send(self, value: T) {
    // Si un pipe est enregistré, le résultat va directement au pipe.
    if let Some(pipe) = self.inner.on_send.lock().unwrap().take() {
        pipe(value);
        let mut state = self.inner.state.lock().unwrap();
        state.closed = true;
        self.inner.ready.notify_one();
        return;
    }
    // Chemin normal inchangé (store value, notify, fire resume)...
}
```

### Méthode interne sur ReplyReceiver (privée)

```rust
impl<T: Send + 'static> ReplyReceiver<T> {
    /// Pose le callback on_send. Appelé par pipe_to/collect_to AVANT
    /// que le message soit envoyé. Méthode interne — l'API publique
    /// passe par ActorRef::pipe_to / Pool::collect_to.
    fn set_pipe(&self, callback: impl FnOnce(T) + Send + 'static) {
        *self.inner.on_send.lock().unwrap() = Some(Box::new(callback));
    }
}
```

### Primitive 1 : `ActorRef::pipe_to` — 1 acteur, 1 résultat

```rust
impl<M: Send + 'static> ActorRef<M> {
    /// Envoie un message à cet acteur et pipe le résultat vers target.
    ///
    /// `msg_fn` reçoit un Reply<T> et construit le message à envoyer.
    /// Quand l'acteur répond, `map` transforme le résultat en un message
    /// pour target, qui le reçoit dans sa mailbox.
    ///
    /// Ordre garanti : callback posé AVANT envoi → pas de race condition.
    /// Aucun thread bloqué. WaitGraph auto-enregistré.
    pub fn pipe_to<T, R, F, G>(
        &self,
        msg_fn: F,
        target: &ActorRef<R>,
        label: &str,
        map: G,
    ) where
        T: Send + 'static,
        R: Send + 'static,
        F: FnOnce(Reply<T>) -> M,
        G: FnOnce(T) -> R + Send + 'static,
    {
        let (tx, rx) = reply::<T>();
        let target = target.clone();
        let edge_id = wait_graph::register(
            wait_graph::current_waiter(),
            label.to_string(),
        );
        // 1. Callback AVANT envoi
        rx.set_pipe(move |value: T| {
            wait_graph::unregister(edge_id);
            let _ = target.send(map(value));
        });
        // 2. Envoi APRÈS callback
        let _ = self.send(msg_fn(tx));
    }
}
```

### Primitive 2 : `Pool::collect_to` — N acteurs, N résultats → 1 message

```rust
impl<M: Send + 'static> Pool<M> {
    /// Scatter un message à tous les workers, collecte tous les résultats,
    /// envoie un seul message à target quand tous ont répondu.
    ///
    /// `msg_fn` est appelé N fois (une par worker) avec un Reply<T>.
    /// Quand le dernier worker répond, `map(results)` construit le message.
    /// results[i] correspond au worker i (ordre garanti).
    pub fn collect_to<T, R, F, G>(
        &self,
        msg_fn: F,
        target: &ActorRef<R>,
        label: &str,
        map: G,
    ) where
        T: Send + 'static,
        R: Send + 'static,
        F: Fn(Reply<T>) -> M,       // Fn, pas FnOnce — appelé N fois
        G: FnOnce(Vec<T>) -> R + Send + 'static,
    {
        let n = self.len();
        if n == 0 {
            let _ = target.send(map(vec![]));
            return;
        }

        // Shared state entre les N callbacks
        let results: Arc<Mutex<Vec<Option<T>>>> = Arc::new(Mutex::new(
            (0..n).map(|_| None).collect()
        ));
        let remaining = Arc::new(AtomicUsize::new(n));
        let target_arc = target.clone();
        let map = Arc::new(Mutex::new(Some(map)));
        let edge_id = wait_graph::register(
            wait_graph::current_waiter(),
            format!("{label} (0/{n})"),
        );

        for i in 0..n {
            let (tx, rx) = reply::<T>();
            let results = Arc::clone(&results);
            let remaining = Arc::clone(&remaining);
            let target = target_arc.clone();
            let map = Arc::clone(&map);

            // 1. Callback AVANT envoi
            rx.set_pipe(move |value: T| {
                results.lock().unwrap()[i] = Some(value);
                if remaining.fetch_sub(1, Ordering::AcqRel) == 1 {
                    // Dernier résultat — collecter et envoyer
                    wait_graph::unregister(edge_id);
                    let collected: Vec<T> = results.lock().unwrap()
                        .iter_mut()
                        .map(|opt| opt.take().unwrap())
                        .collect();
                    if let Some(f) = map.lock().unwrap().take() {
                        let _ = target.send(f(collected));
                    }
                }
            });
            // 2. Envoi APRÈS callback
            let _ = self.worker(i).send(msg_fn(tx));
        }
    }
}
```

### Primitive 3 : `ActorRef::pipe_to` pour tâches (submit_task)

Pour les tâches CPU (execute_dag, etc.) qui tournent sur le pool :

```rust
impl Scheduler {
    /// Soumet une tâche CPU et pipe le résultat vers target.
    ///
    /// Même pattern : callback posé avant soumission.
    pub fn task_pipe_to<T, R, F, G>(
        &self,
        priority: Priority,
        task: F,
        target: &ActorRef<R>,
        label: &str,
        map: G,
    ) where
        T: Send + 'static,
        R: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
        G: FnOnce(T) -> R + Send + 'static,
    {
        let (result_tx, result_rx) = reply::<T>();
        let target = target.clone();
        let edge_id = wait_graph::register(
            wait_graph::current_waiter(),
            label.to_string(),
        );
        // 1. Callback AVANT soumission
        result_rx.set_pipe(move |value: T| {
            wait_graph::unregister(edge_id);
            let _ = target.send(map(value));
        });
        // 2. Soumission APRÈS callback
        let task_wrapped = Box::new(move || {
            let result = task();
            result_tx.send(result);
        });
        // Push to ready queue...
        self.submit_task_raw(priority, task_wrapped);
    }
}
```

### Pas de circular Arc

Le callback dans `on_send` capture `target`, `map`, `results`, `remaining`
— mais **jamais** `inner`. Le callback est stocké dans `inner.on_send`, mais
ne référence pas `inner`. Donc pas de cycle d'Arc.

Quand `Reply::send()` appelle le callback, c'est synchrone dans le même
call stack. Puis `Reply` drop, `Inner` drop (si plus de refs), tout est
nettoyé.

## Usage patterns

### Pattern A : requête simple (1 acteur, 1 résultat)

```rust
fn handle(&mut self, msg: ShardMsg, ctx: &ActorContext) -> ActorStatus {
    match msg {
        ShardMsg::Commit { fast, reply } => {
            // "Je demande à l'indexer de flusher, rappelle-moi quand c'est fait"
            self.indexer.pipe_to(
                |reply| IndexerMsg::Flush(reply),
                &self.self_ref, "flush",
                |result| ShardMsg::FlushDone { result, fast, reply },
            );
            ActorStatus::Continue
        }
        ShardMsg::FlushDone { result, fast, reply } => {
            let prepared = self.writer.finalize(result)?;
            prepared.commit()?;
            reply.send(Ok(()));
            ActorStatus::Continue
        }
    }
}
```

### Pattern B : scatter-gather (N acteurs)

```rust
ShardMsg::Commit { fast, reply } => {
    // "Je demande à tous les workers de flusher, rappelle-moi quand
    //  ils ont TOUS fini avec tous les résultats"
    self.worker_pool.collect_to(
        |reply| WorkerMsg::Flush(reply),
        &self.self_ref, "flush_all",
        |results| ShardMsg::AllFlushesDone { results, fast, reply },
    );
    ActorStatus::Continue
}

ShardMsg::AllFlushesDone { results, fast, reply } => {
    let prepared = self.writer.finalize_flush_and_prepare(results)?;
    prepared.commit()?;
    reply.send(Ok(()));
    ActorStatus::Continue
}
```

### Pattern C : chaînage (A → B → C)

```rust
// Flush workers → commit DAG → reply
ShardMsg::Commit { reply } => {
    self.worker_pool.collect_to(
        |r| WorkerMsg::Flush(r),
        &self.self_ref, "flush",
        |results| ShardMsg::FlushDone { results, reply },
    );
    ActorStatus::Continue
}

ShardMsg::FlushDone { results, reply } => {
    let prepared = self.writer.finalize(results)?;
    // Lancer le commit DAG comme tâche, résultat revient comme message
    scheduler.task_pipe_to(
        Priority::High,
        move || execute_dag(&mut dag, None),
        &self.self_ref, "commit_dag",
        |dag_result| ShardMsg::CommitDagDone { dag_result, reply },
    );
    ActorStatus::Continue
}

ShardMsg::CommitDagDone { dag_result, reply } => {
    dag_result?;
    reply.send(Ok(()));
    ActorStatus::Continue
}
```

### Pattern D : réactivité pendant l'attente

L'acteur reste actif — il peut traiter d'autres messages pendant l'attente.
Si on veut bloquer certains messages (sémantique Suspend), un simple flag :

```rust
ShardMsg::Commit { reply } => {
    self.committing = true;  // Flag: rejeter les inserts pendant commit
    self.worker_pool.collect_to(
        |r| WorkerMsg::Flush(r),
        &self.self_ref, "flush",
        |results| ShardMsg::FlushDone { results, reply },
    );
    ActorStatus::Continue
}

ShardMsg::Insert { .. } if self.committing => {
    // Rejeter ou queuer — l'acteur décide
    ActorStatus::Continue
}

ShardMsg::FlushDone { results, reply } => {
    self.committing = false;
    // ...
}
```

## Quand utiliser quoi

| Pattern | Quand | Thread bloqué ? | Messages pendant attente ? |
|---------|-------|-----------------|---------------------------|
| `actor.pipe_to(msg, target, label, map)` | 1 dépendance | Non | Oui |
| `pool.collect_to(msg, target, label, map)` | N dépendances | Non | Oui |
| `scheduler.task_pipe_to(task, target, label, map)` | Tâche CPU | Non | Oui |
| `Suspend + set_resume` | Legacy / edge cases | Non | Non |
| `scheduler.wait()` | Thread externe seulement | Oui | N/A |

**Règle** : `scheduler.wait()` = uniquement depuis un thread externe (main,
ingestion). Jamais depuis un handler ou poll_idle.

## Intégration WaitGraph

Chaque primitive enregistre automatiquement un edge dans le WaitGraph global.
Visible dans `dump_wait_graph()` :

```
WaitGraph (3 edges):
  shard_0 --[flush_all (0/4)]--> waiting (2.3s)
  shard_1 --[flush_all (0/4)]--> waiting (2.1s)
  shard_2 --[commit_dag]--> waiting (0.5s)
```

L'edge est supprimé quand le dernier résultat arrive et le message est envoyé.

## Backward compat

- `Suspend` + `set_resume` + `JoinResume` restent disponibles pour les cas
  edge (actors sans self_ref, GenericActor legacy).
- `pipe_to` / `collect_to` / `task_pipe_to` sont additifs — aucun code
  existant ne casse.
- Migration incrémentale : on peut convertir les acteurs un par un.

## Acteurs vs tâches — quand utiliser quoi

**Un acteur** a de l'état mutable qui persiste entre les appels :
- IndexWriter : segments ouverts, buffers, mem_budget
- ShardRouter : compteurs de tokens, mapping node_id → shard
- ShardActor : le LucivyHandle, le dirty flag, le committing flag

On ne peut pas en faire une simple tâche parce que l'état doit survivre
entre les messages et être accédé séquentiellement (pas de race).

**Une tâche** est stateless — données en entrée, résultat en sortie, terminé :
- "Flush ce segment" — prend des docs buffered, produit des bytes
- "Execute ce DAG" — prend un DAG, retourne un résultat
- "Tokenize ce document" — texte in, tokens out

**Le flow d'orchestration EST une chaîne de tâches.** Le commit shardé
(flush workers → collect → finalize → DAG → reply) est une chaîne, pas
du "comportement d'acteur." Et c'est exactement ce que pipe_to modélise.

**L'insight clé** : l'appelant se fiche de savoir si le travail est fait par
un acteur ou par une tâche. Le pattern est identique :

```rust
// Vers un acteur :
indexer.pipe_to(|r| FlushMsg(r), &self_ref, "flush", |res| Done(res));

// Vers une tâche CPU :
scheduler.task_pipe_to(|| heavy_work(), &self_ref, "cpu", |res| Done(res));

// Vers N acteurs :
pool.collect_to(|r| FlushMsg(r), &self_ref, "all", |res| AllDone(res));
```

Même API, même pattern. L'utilisateur de luciole pense en termes de
**"chaîne de travail"** : j'envoie du travail quelque part, je récupère
le résultat. Pas en termes de "est-ce un acteur ou une tâche."

**Résumé** :
- Acteurs = état mutable persistant + sérialisation d'accès
- Tâches = calcul stateless one-shot
- pipe_to/collect_to/task_pipe_to = pattern unifié pour les enchaîner

## Impact sur les deadlocks

Avec pipe_to/collect_to, un handler ne bloque jamais. Il :
1. Enregistre un callback (non-bloquant)
2. Envoie un message (non-bloquant)
3. Retourne Continue (libère le thread immédiatement)

Le thread est libéré **immédiatement**. Le résultat arrive plus tard comme
un message dans la mailbox. Aucune capture de thread → aucun deadlock.

pipe_to est **structurellement deadlock-free** : un cycle de pipe_to
(A → B → A) ne cause pas de deadlock car les messages sont dans les
mailboxes et les scheduler threads les dispatchent normalement. Aucun
thread ne wait, donc aucun cycle de wait.

## Résumé des primitives

| Méthode | Sur | Rôle |
|---------|-----|------|
| `actor_ref.pipe_to(msg, target, label, map)` | `ActorRef<M>` | 1 requête → 1 message retour |
| `pool.collect_to(msg, target, label, map)` | `Pool<M>` | N requêtes → 1 message retour |
| `scheduler.task_pipe_to(task, target, label, map)` | `Scheduler` | 1 tâche CPU → 1 message retour |
| `rx.set_pipe(callback)` | `ReplyReceiver<T>` | Primitive interne (privée) |

Toutes respectent l'invariant : **callback posé avant envoi**.
