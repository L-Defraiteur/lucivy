# Migration de tous les acteurs vers Luciole — Stratégie

Date : 17 mars 2026

## Inventaire

| Acteur | Fichier | Messages | État | Complexité |
|--------|---------|----------|------|------------|
| `IndexerActor<D>` | indexer_actor.rs | Docs, Flush, Shutdown | segment en cours, delete_cursor, bomb | Moyenne |
| `SegmentUpdaterActor` | segment_updater_actor.rs | AddSegment, Commit, GC, StartMerge, MergeStep, DrainMerges, Kill | active_merge, explicit_merge, pending_merges, segments_in_merge | Haute |
| `ShardActor` (déjà migré) | sharded_handle.rs | Search, Insert, Commit, Delete | LucivyHandle | Fait |
| Acteurs de test | scheduler.rs | Divers | Compteurs | Triviale (laisser tels quels) |

## Problèmes à résoudre

### 1. Reply typés → ReplyPort (bytes)

**Aujourd'hui :**
```rust
// IndexWriter envoie un commit
let (reply, rx) = reply::<crate::Result<Opstamp>>();
self.segment_updater_ref.send(SegmentUpdaterMsg::Commit { opstamp, payload, reply });
let result: crate::Result<Opstamp> = rx.wait_blocking();
```

**Avec Luciole :**
```rust
// Le reply est en bytes, pas typé
let (env, rx) = CommitMsg { opstamp, payload }.into_request();
self.actor_ref.send(env);
let bytes: Result<Vec<u8>, String> = rx.wait_cooperative(|| scheduler.run_one_step());
let result: crate::Result<Opstamp> = decode_result(&bytes?)?;
```

**Solution : `TypedActorRef` — façade typée sur `ActorRef<Envelope>`**

```rust
/// Wraps ActorRef<Envelope> avec des méthodes typées.
/// Les callers ne voient jamais les bytes.
pub struct TypedActorRef {
    inner: ActorRef<Envelope>,
}

impl TypedActorRef {
    /// Envoie un message fire-and-forget.
    pub fn send<M: Message>(&self, msg: M) -> Result<(), String> {
        self.inner.send(msg.into_envelope())
            .map_err(|_| "channel closed".into())
    }

    /// Envoie un message et attend une réponse typée.
    pub fn request<M: Message, R: Message>(&self, msg: M) -> Result<R, String> {
        let (env, rx) = msg.into_request();
        self.inner.send(env).map_err(|_| "channel closed")?;
        let scheduler = global_scheduler();
        let bytes = rx.wait_cooperative(|| scheduler.run_one_step())
            .map_err(|e| e)?;
        R::decode(&bytes)
    }

    /// Envoie avec local data (non-sérialisable).
    pub fn send_with_local<M: Message>(&self, msg: M, local: impl Any + Send) -> Result<(), String> {
        self.inner.send(msg.into_envelope_with_local(local))
            .map_err(|_| "channel closed".into())
    }

    /// Request avec local data.
    pub fn request_with_local<M: Message, R: Message>(
        &self, msg: M, local: impl Any + Send
    ) -> Result<R, String> {
        let (env, rx) = msg.into_request_with_local(local);
        self.inner.send(env).map_err(|_| "channel closed")?;
        let scheduler = global_scheduler();
        let bytes = rx.wait_cooperative(|| scheduler.run_one_step())
            .map_err(|e| e)?;
        R::decode(&bytes)
    }
}
```

Les callers (IndexWriter, SegmentUpdater) passent de `ActorRef<SegmentUpdaterMsg>` à `TypedActorRef` avec la même ergonomie.

### 2. Données non-sérialisables dans les messages

**Problème :** `IndexerMsg::Docs(AddBatch<D>)` contient des documents Rust, pas sérialisables. `SegmentUpdaterMsg::AddSegment { entry: SegmentEntry }` contient une SegmentEntry (handles de fichiers).

**Solution : `Envelope.local` (déjà implémenté)**

Les données non-sérialisables passent via `Envelope.local: Option<Box<dyn Any + Send>>`. Le payload contient un marqueur vide ou des métadonnées sérialisables. Le handler downcast le `local`.

```rust
// Envoi
let msg = DocsMsg; // payload vide, juste le type_tag
let env = msg.into_envelope_with_local(batch); // AddBatch<D> dans local

// Handler
|state, _msg, _reply, local| {
    let batch = local.unwrap().downcast::<AddBatch<D>>().unwrap();
    // ... traiter le batch
}
```

**Pour le réseau (Phase 4)** : les documents devront être sérialisés. On ajoutera une implémentation `Message` pour `AddBatch<D>` qui sérialise les docs en JSON/binaire. Mais pour Phase 1 (local), `Envelope.local` suffit.

### 3. Self-messages (MergeStep)

**Problème :** `SegmentUpdaterActor` s'envoie `MergeStep` via `self_ref: ActorRef<SegmentUpdaterMsg>`.

**Solution :** Le `GenericActor` reçoit son `self_ref: ActorRef<Envelope>` via `on_start()`. Le handler MergeStep utilise ce self_ref pour se re-programmer :

```rust
// Dans on_start, stocker le self_ref dans l'ActorState
fn on_start(&mut self, self_ref: ActorRef<Envelope>) {
    self.state_mut().insert::<ActorRef<Envelope>>(self_ref);
}

// Dans le handler MergeStep :
|state, _msg, _reply, _local| {
    // ... avancer le merge d'un step ...

    // Re-programmer le prochain step
    if has_more_work {
        let self_ref = state.get::<ActorRef<Envelope>>().unwrap();
        let _ = self_ref.send(MergeStepMsg.into_envelope());
    }
    ActorStatus::Continue
}
```

**Subtilité :** `GenericActor` implémente `on_start` via le trait Actor, qui reçoit `ActorRef<Self::Msg>` = `ActorRef<Envelope>`. Il faut exposer cette méthode dans GenericActor :

```rust
impl Actor for GenericActor {
    fn on_start(&mut self, self_ref: ActorRef<Envelope>) {
        self.state.insert::<ActorRef<Envelope>>(self_ref);
    }
}
```

### 4. Types génériques (IndexerActor\<D\>)

**Problème :** `IndexerActor<D: Document>` est générique. Le `GenericActor` ne connaît pas `D`.

**Solution :** Le type `D` est connu au moment du spawn. Le handler est construit avec le bon type via closure :

```rust
fn create_indexer_actor<D: Document>(
    segment_updater: SegmentUpdater,
    index: Index,
    mem_budget: usize,
    delete_cursor: DeleteCursor,
    bomb: IndexWriterBomb<D>,
) -> GenericActor {
    let mut actor = GenericActor::new("indexer");

    // État
    actor.state_mut().insert::<SegmentUpdater>(segment_updater);
    actor.state_mut().insert::<Index>(index);
    actor.state_mut().insert::<usize>(mem_budget);
    actor.state_mut().insert::<DeleteCursor>(delete_cursor);
    actor.state_mut().insert::<Option<IndexWriterBomb<D>>>(Some(bomb));
    actor.state_mut().insert::<Option<SegmentInProgress>>(None);
    actor.state_mut().insert::<Option<crate::LucivyError>>(None);

    // Le handler Docs est typé D au moment de la construction
    actor.register(TypedHandler::<DocsMsg, _>::new(move |state, _msg, _reply, local| {
        let batch: AddBatch<D> = *local.unwrap().downcast().unwrap();
        handle_docs::<D>(state, batch)
    }));

    actor.register(TypedHandler::<FlushMsg, _>::new(|state, _msg, reply, _local| {
        handle_flush(state, reply)
    }));

    actor.register(TypedHandler::<ShutdownMsg, _>::new(|state, _msg, _reply, _local| {
        handle_shutdown::<D>(state);
        ActorStatus::Stop
    }));

    actor
}
```

Le type `D` est capturé par la closure du handler Docs. Le GenericActor lui-même n'est pas générique — c'est la closure qui l'est. Type erasure via Any + closure.

### 5. Priority dynamique

**Problème :** `IndexerActor` change de priorité selon son état (High si segment ouvert, Low si idle).

**Solution :** Le `GenericActor` peut lire son état pour déterminer la priorité. Deux approches :

**A — Priority callback dans l'ActorState :**
```rust
actor.state_mut().insert::<Box<dyn Fn(&ActorState) -> Priority + Send>>(
    Box::new(|state| {
        if state.get::<Option<SegmentInProgress>>().unwrap().is_some() {
            Priority::High
        } else {
            Priority::Low
        }
    })
);
```

**B — Priority handler explicite :**
Ajouter un champ `priority_fn: Option<Box<dyn Fn(&ActorState) -> Priority + Send>>` dans GenericActor. Si présent, il override la priority statique des handlers.

```rust
impl GenericActor {
    pub fn with_priority_fn(mut self, f: impl Fn(&ActorState) -> Priority + Send + 'static) -> Self {
        self.priority_fn = Some(Box::new(f));
        self
    }
}

impl Actor for GenericActor {
    fn priority(&self) -> Priority {
        if let Some(ref f) = self.priority_fn {
            f(&self.state)
        } else {
            self.handlers.values().map(|h| h.priority()).max().unwrap_or(Priority::Medium)
        }
    }
}
```

Option B est plus propre.

## Plan de migration

### Étape 1 : Infra (avant de toucher les acteurs)
1. Implémenter `TypedActorRef` — façade typée sur `ActorRef<Envelope>` (~50 lignes)
2. Ajouter `on_start` auto dans GenericActor (stocke self_ref dans state)
3. Ajouter `priority_fn` dans GenericActor (~15 lignes)

### Étape 2 : IndexerActor (le plus simple)
1. Définir messages : `DocsMsg`, `FlushMsg`, `ShutdownMsg`
2. Créer `create_indexer_actor<D>()` qui construit un GenericActor avec les handlers
3. Migrer `IndexWriter` : `ActorRef<IndexerMsg<D>>` → `TypedActorRef`
4. Tests : re-run tous les tests d'indexation (1203 tests ld-lucivy)

### Étape 3 : SegmentUpdaterActor (le plus complexe)
1. Définir 7 messages : `AddSegmentMsg`, `CommitMsg`, `GarbageCollectMsg`, `StartMergeMsg`, `MergeStepMsg`, `DrainMergesMsg`, `KillMsg`
2. Extraire la logique dans des fonctions stateless `handle_commit(state, ...)`, `handle_merge_step(state, ...)` etc.
3. Créer `create_segment_updater_actor()` avec GenericActor + tous les handlers
4. Self-message : MergeStep via `state.get::<ActorRef<Envelope>>().send(MergeStepMsg.into_envelope())`
5. Migrer `SegmentUpdater` : `ActorRef<SegmentUpdaterMsg>` → `TypedActorRef`
6. Tests : re-run 1203 tests

### Étape 4 : Cleanup
1. Supprimer l'ancien trait `Actor` (ou le laisser pour backward compat)
2. Supprimer les enums de messages typés (`SegmentUpdaterMsg`, `IndexerMsg`)
3. Acteurs de test dans scheduler.rs : migrer ou laisser (pas critique)

## Estimation

| Étape | Lignes modifiées | Risque |
|-------|-----------------|--------|
| Étape 1 (infra) | ~80 nouvelles | Faible |
| Étape 2 (IndexerActor) | ~200 modifiées | Moyen (generics D) |
| Étape 3 (SegmentUpdater) | ~400 modifiées | Élevé (self-messages, merge state) |
| Étape 4 (cleanup) | ~100 supprimées | Faible |

## Ce qu'on gagne

1. **Un seul type d'acteur** dans tout le système — plus de `Actor<Msg>` vs `GenericActor`
2. **IndexerActor idle peut recevoir d'autres rôles** — pas immédiat mais possible
3. **SegmentUpdaterActor peut être supervisé** par un acteur qui comprend ses messages
4. **Tous les messages sont sérialisables** — prêt pour le distribué (Phase 4 Luciole)
5. **L'ancien trait `Actor` peut être supprimé** — simplification du scheduler

## Ce qu'on ne perd PAS

- **Compile-time safety** : `TypedActorRef` vérifie les types des messages à la compilation
- **Performance** : zero overhead en local (Envelope.local = pointeur, pas de sérialisation)
- **WASM compat** : rien ne change côté scheduler/coopératif
- **Tests existants** : les acteurs de test dans scheduler.rs restent compatibles (le trait Actor typé coexiste)

## Piste suivante : pipeline d'ingestion avec ReaderActors

Une fois la migration terminée, le pipeline d'ingestion pourrait être découpé en acteurs spécialisés :

```
Caller → ReaderActor[0] → tokenize+hash → RouterActor → ShardActor[best]
       → ReaderActor[1] → tokenize+hash ↗
       → ReaderActor[2] → tokenize+hash ↗
```

- **ReaderActors** (pool de N) : reçoivent les docs bruts, font le tokenize+hash (CPU-bound, parallèle)
- **RouterActor** (unique) : reçoit les `(doc, hashes)`, route via ShardRouter (séquentiel, pas de contention), dispatch au bon ShardActor
- **ShardActors** : écrivent dans les index (déjà en place)

Le tokenize+hash est le plus coûteux CPU. Le paralléliser sur N ReaderActors pendant que le routage reste séquentiel donne le meilleur ratio. Chaque pièce est un GenericActor — les ReaderActors idle pourraient même prendre le rôle de ShardActor pour du search si le pipeline d'ingestion est au repos.

Pertinent surtout pour les gros volumes (>10K docs). Pour les petits corpus, l'overhead des messages entre acteurs annule le gain.
