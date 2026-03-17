# Implémentation Luciole Phase 1 — Envelope + GenericActor local

Date : 17 mars 2026

## Objectif

Implémenter le système d'envelopes et le GenericActor dans le module `src/actor/` existant, en coexistence avec les acteurs typés. Puis migrer `ShardSearchActor` vers un `GenericActor` avec les rôles Search + Insert + Commit + Delete.

## Fichiers à créer/modifier

### Nouveaux fichiers dans `src/actor/`

```
src/actor/
  mod.rs              ← modifier : ajouter les nouveaux modules
  envelope.rs         ← NEW : Envelope, Message trait, type_tag, encode/decode
  handler.rs          ← NEW : Handler trait, HandlerFn helper
  actor_state.rs      ← NEW : ActorState (resource bag)
  generic_actor.rs    ← NEW : GenericActor, dispatch, register/unregister
  // existants inchangés :
  mailbox.rs          ← inchangé (fonctionne déjà avec Envelope au lieu de M)
  reply.rs            ← adapter ReplyPort pour Envelope
  scheduler.rs        ← adapter pour supporter GenericActor en plus de Actor<Msg>
  events.rs           ← inchangé
```

### Fichier modifié dans `lucivy_core/`

```
lucivy_core/src/
  sharded_handle.rs   ← migrer ShardSearchActor → GenericActor
```

## Étapes détaillées

### Étape 1 : `envelope.rs` — le message universel

```rust
// src/actor/envelope.rs

/// Hash FNV-1a stable d'un nom de type. Cross-build, cross-platform.
pub fn type_tag_hash(name: &str) -> u64 { /* fnv1a */ }

/// Un message sérialisé.
pub struct Envelope {
    pub type_tag: u64,
    pub payload: Vec<u8>,
    pub reply: Option<ReplyPort>,
}

/// Trait pour les messages typés ↔ Envelope.
pub trait Message: Send + Sized + 'static {
    fn type_tag() -> u64;
    fn encode(&self) -> Vec<u8>;
    fn decode(bytes: &[u8]) -> Result<Self, String>;
}
```

**Sérialisation** : pour l'instant, `serde_json` (on a déjà la dépendance). Migration vers `postcard` en Phase 2 quand on ajoutera la derive macro.

**ReplyPort** : wrapper autour du `Reply<Vec<u8>>` existant — la réponse est aussi des bytes.

```rust
pub struct ReplyPort {
    inner: Reply<Vec<u8>>,
}

impl ReplyPort {
    pub fn send<M: Message>(self, msg: M) {
        self.inner.send(msg.encode());
    }

    pub fn send_bytes(self, bytes: Vec<u8>) {
        self.inner.send(bytes);
    }
}
```

**Tests** : roundtrip encode/decode, type_tag collision check.

### Étape 2 : `actor_state.rs` — le bag de resources

```rust
// src/actor/actor_state.rs

pub struct ActorState {
    resources: HashMap<TypeId, Box<dyn Any + Send>>,
}

impl ActorState {
    pub fn new() -> Self { ... }
    pub fn insert<T: Send + 'static>(&mut self, value: T) { ... }
    pub fn get<T: Send + 'static>(&self) -> Option<&T> { ... }
    pub fn get_mut<T: Send + 'static>(&mut self) -> Option<&mut T> { ... }
    pub fn remove<T: Send + 'static>(&mut self) -> Option<T> { ... }
    pub fn has<T: Send + 'static>(&self) -> bool { ... }
}
```

**Tests** : insert/get/remove, type mismatch, multiple types.

### Étape 3 : `handler.rs` — les rôles

```rust
// src/actor/handler.rs

pub trait Handler: Send + 'static {
    fn type_tag(&self) -> u64;
    fn handle(
        &self,
        state: &mut ActorState,
        payload: &[u8],
        reply: Option<ReplyPort>,
    ) -> ActorStatus;
    fn priority(&self) -> Priority { Priority::Medium }
}

/// Helper : créer un Handler depuis un type Message + une closure.
pub struct TypedHandler<M: Message, F> {
    handler_fn: F,
    _phantom: PhantomData<M>,
}

impl<M, F> TypedHandler<M, F>
where
    M: Message,
    F: Fn(&mut ActorState, M, Option<ReplyPort>) -> ActorStatus + Send + 'static,
{
    pub fn new(f: F) -> Self { ... }
}

impl<M, F> Handler for TypedHandler<M, F>
where
    M: Message,
    F: Fn(&mut ActorState, M, Option<ReplyPort>) -> ActorStatus + Send + 'static,
{
    fn type_tag(&self) -> u64 { M::type_tag() }

    fn handle(&self, state: &mut ActorState, payload: &[u8], reply: Option<ReplyPort>) -> ActorStatus {
        match M::decode(payload) {
            Ok(msg) => (self.handler_fn)(state, msg, reply),
            Err(e) => {
                eprintln!("handler decode error: {e}");
                ActorStatus::Continue
            }
        }
    }
}
```

**Tests** : créer un handler typé, dispatch un envelope, vérifier le résultat via reply.

### Étape 4 : `generic_actor.rs` — l'acteur universel

```rust
// src/actor/generic_actor.rs

pub struct GenericActor {
    state: ActorState,
    handlers: HashMap<u64, Box<dyn Handler>>,
    name: String,
}

impl GenericActor {
    pub fn new(name: impl Into<String>) -> Self { ... }

    pub fn register(&mut self, handler: impl Handler) {
        self.handlers.insert(handler.type_tag(), Box::new(handler));
    }

    pub fn unregister(&mut self, type_tag: u64) {
        self.handlers.remove(&type_tag);
    }

    pub fn state(&self) -> &ActorState { &self.state }
    pub fn state_mut(&mut self) -> &mut ActorState { &mut self.state }

    pub fn dispatch(&mut self, envelope: Envelope) -> ActorStatus {
        match self.handlers.get(&envelope.type_tag) {
            Some(handler) => handler.handle(
                &mut self.state,
                &envelope.payload,
                envelope.reply,
            ),
            None => {
                eprintln!("[{}] no handler for type_tag {:#x}", self.name, envelope.type_tag);
                ActorStatus::Continue
            }
        }
    }

    /// Nombre de handlers enregistrés.
    pub fn num_handlers(&self) -> usize { self.handlers.len() }

    /// Vérifie si un handler est enregistré pour ce type.
    pub fn has_handler(&self, type_tag: u64) -> bool {
        self.handlers.contains_key(&type_tag)
    }
}
```

**Intégration scheduler** : `GenericActor` implémente le trait `Actor` existant avec `Msg = Envelope` :

```rust
impl Actor for GenericActor {
    type Msg = Envelope;

    fn name(&self) -> &'static str {
        // Leak le nom pour avoir un &'static str (une seule fois par acteur)
        Box::leak(self.name.clone().into_boxed_str())
    }

    fn handle(&mut self, msg: Envelope) -> ActorStatus {
        self.dispatch(msg)
    }

    fn priority(&self) -> Priority {
        // Max priority parmi les handlers
        self.handlers.values()
            .map(|h| h.priority())
            .max()
            .unwrap_or(Priority::Medium)
    }
}
```

**Avantage** : le scheduler n'a PAS besoin de changer. Un `GenericActor` est juste un `Actor<Msg = Envelope>`. Il se spawn comme n'importe quel acteur typé.

**Tests** : spawn un GenericActor, envoyer des Envelopes de différents types, vérifier dispatch.

### Étape 5 : Helper `ActorRef<Envelope>` pour envoi typé

```rust
// Extension sur ActorRef<Envelope> pour envoyer des messages typés

pub trait TypedSend {
    fn send_msg<M: Message>(&self, msg: M) -> Result<(), String>;
    fn send_request<M: Message, R: Message>(&self, msg: M) -> Result<ReplyReceiver<Vec<u8>>, String>;
}

impl TypedSend for ActorRef<Envelope> {
    fn send_msg<M: Message>(&self, msg: M) -> Result<(), String> {
        self.send(Envelope {
            type_tag: M::type_tag(),
            payload: msg.encode(),
            reply: None,
        }).map_err(|e| format!("send: {e}"))
    }

    fn send_request<M: Message, R: Message>(&self, msg: M) -> Result<ReplyReceiver<Vec<u8>>, String> {
        let (reply_port, receiver) = reply_port();
        self.send(Envelope {
            type_tag: M::type_tag(),
            payload: msg.encode(),
            reply: Some(reply_port),
        }).map_err(|e| format!("send: {e}"))?;
        Ok(receiver)
    }
}
```

### Étape 6 : Migrer ShardSearchActor → GenericActor

Remplacer dans `lucivy_core/src/sharded_handle.rs` :

**Avant** :
```rust
struct ShardSearchActor { shard_id, handle }
enum ShardSearchMsg { Search { weight, top_k, reply } }
impl Actor for ShardSearchActor { type Msg = ShardSearchMsg; ... }
```

**Après** :
```rust
// Messages sérialisables
#[derive(Serialize, Deserialize)]
struct ShardSearchMsg { weight_bytes: Vec<u8>, top_k: usize }

#[derive(Serialize, Deserialize)]
struct ShardInsertMsg { doc_bytes: Vec<u8> }

#[derive(Serialize, Deserialize)]
struct ShardCommitMsg {}

#[derive(Serialize, Deserialize)]
struct ShardDeleteMsg { node_id: u64 }

// Handlers
fn search_handler() -> impl Handler { TypedHandler::<ShardSearchMsg, _>::new(|state, msg, reply| { ... }) }
fn insert_handler() -> impl Handler { TypedHandler::<ShardInsertMsg, _>::new(|state, msg, reply| { ... }) }
fn commit_handler() -> impl Handler { TypedHandler::<ShardCommitMsg, _>::new(|state, msg, reply| { ... }) }
fn delete_handler() -> impl Handler { TypedHandler::<ShardDeleteMsg, _>::new(|state, msg, reply| { ... }) }

// Création
fn create_shard_actor(shard_id: usize, handle: Arc<LucivyHandle>) -> GenericActor {
    let mut actor = GenericActor::new(format!("shard-{shard_id}"));
    actor.state_mut().insert(handle);
    actor.state_mut().insert(shard_id);
    actor.state_mut().insert::<Vec<LucivyDocument>>(Vec::new()); // insert buffer
    actor.register(search_handler());
    actor.register(insert_handler());
    actor.register(commit_handler());
    actor.register(delete_handler());
    actor
}
```

## Problème ouvert : sérialisation du Weight

Le `Arc<dyn Weight>` n'est pas sérialisable. Pour le scatter-gather BM25, deux options :

**A** — Ne pas sérialiser le Weight, le passer via le state de l'acteur. L'Envelope contient juste `top_k`, le Weight est dans un `Arc` dans l'ActorState. Le caller le met dans le state avant d'envoyer le message.

**B** — Sérialiser la QueryConfig + les stats globales. L'acteur reconstruit le Weight localement. Plus propre pour le réseau, mais on perd le scatter-gather (N reconstructions au lieu de 1).

**Pour Phase 1 : option A** (performance locale). Le Weight passe par un side-channel (pas dans l'Envelope). On ajoutera la sérialisation Weight en Phase 4 (distribué).

```rust
// Side-channel : mettre le Weight dans le state avant l'envoi
actor_state.insert::<Arc<dyn Weight>>(weight.clone());
actor_ref.send_msg(ShardSearchMsg { top_k: 20 })?;
```

Non. Mieux : **le Weight est dans l'Envelope.payload mais comme raw bytes wrappant un Arc**. On caste via un newtype. En local c'est un pointeur, en réseau on sérialiserait vraiment.

```rust
// LocalWeight : wrapper qui "sérialise" un Arc<dyn Weight> en local
// (juste un pointeur derrière, zero-copy)
struct LocalWeight(Arc<dyn Weight>);

impl Message for ShardSearchMsg {
    // Le payload contient le top_k + un index dans un registre global de Weights
    // Pas de vraie sérialisation du Weight en local
}
```

**Décision : on garde le `Arc<dyn Weight>` dans le ShardSearchMsg tel quel pour Phase 1. On n'essaie pas de sérialiser le Weight. Le Message trait a une implémentation "locale" qui encode les champs sérialisables + passe les Arc via un side-channel dans l'ActorState. La vraie sérialisation viendra en Phase 4.**

## Ordre d'implémentation

1. `envelope.rs` — Envelope, Message trait, type_tag_hash (~40 lignes)
2. `actor_state.rs` — ActorState (~50 lignes)
3. `handler.rs` — Handler trait, TypedHandler (~60 lignes)
4. `generic_actor.rs` — GenericActor, dispatch, impl Actor (~80 lignes)
5. Tests unitaires pour chaque (~100 lignes)
6. `mod.rs` — exporter les nouveaux types
7. `sharded_handle.rs` — migrer ShardSearchActor → GenericActor (~100 lignes modifiées)
8. Re-run bench pour vérifier pas de régression perf

## Ce qu'on ne fait PAS en Phase 1

- Derive macro `#[derive(Message)]` — on implémente Message à la main
- postcard — on utilise serde_json
- Sérialisation de l'ActorState — juste un HashMap en mémoire
- Transport réseau — tout est local
- Migration d'acteur — les acteurs restent sur leur thread
- Remplacement des acteurs typés existants (IndexerActor, MergerActor) — ils restent tels quels
