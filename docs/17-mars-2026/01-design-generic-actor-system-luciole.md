# Design : Système d'acteurs générique — "Luciole"

Date : 17 mars 2026

## Vision

Un système d'acteurs où **n'importe quel acteur peut prendre n'importe quel rôle** à runtime. Les messages sont sérialisés (bytes), les handlers sont enregistrés dynamiquement, l'état est un bag de resources. Prêt pour le distribué.

À terme, extractible en lib standalone `luciole` (actor runtime pour Rust, WASM-compatible).

## Problème avec le système actuel

```rust
// Aujourd'hui : acteurs typés statiquement
trait Actor: Send + 'static {
    type Msg: Send + 'static;   // ← UN seul type de message, fixé à la compilation
    fn handle(&mut self, msg: Self::Msg) -> ActorStatus;
}
```

Limites :
- Un acteur ne peut recevoir qu'un type de message
- Pour ajouter un rôle (insert en plus de search), il faut modifier l'enum + recompiler
- Pas de migration d'acteur entre threads/machines
- Pas de load balancing (un acteur idle ne peut pas prendre le travail d'un autre)
- Pas de transport réseau (les messages sont des structs Rust, pas sérialisables)

## Architecture Luciole

### Trois concepts fondamentaux

```
┌─────────────────────────────────────────────────┐
│  Envelope                                       │
│  ┌──────────┬──────────────┬──────────────────┐ │
│  │ type_tag │ payload_bytes│ reply_port (opt)  │ │
│  │ u64      │ Vec<u8>      │ ReplyPort         │ │
│  └──────────┴──────────────┴──────────────────┘ │
│  Sérialisable. Transportable réseau.            │
└─────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────┐
│  Handler                                        │
│  ┌──────────────────────────────────────────┐   │
│  │ type_tag() → u64                         │   │
│  │ handle(state, payload, reply) → Status   │   │
│  └──────────────────────────────────────────┘   │
│  Sait désérialiser UN type de message.          │
│  Accède à l'état partagé de l'acteur.           │
└─────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────┐
│  GenericActor                                   │
│  ┌────────────────┐  ┌───────────────────────┐  │
│  │ ActorState     │  │ handlers              │  │
│  │ HashMap<       │  │ HashMap<              │  │
│  │   TypeId,      │  │   u64,                │  │
│  │   Box<dyn Any> │  │   Box<dyn Handler>    │  │
│  │ >              │  │ >                     │  │
│  └────────────────┘  └───────────────────────┘  │
│  N'importe quel handler ajouté à runtime.       │
│  N'importe quelle resource dans le state.       │
└─────────────────────────────────────────────────┘
```

### Message — Envelope

```rust
/// Un message sérialisé prêt pour transport local ou réseau.
pub struct Envelope {
    /// Hash stable du nom du type de message (fnv1a ou similaire).
    /// Permet le dispatch sans dépendance sur TypeId (qui n'est pas stable cross-build).
    pub type_tag: u64,
    /// Payload sérialisé (postcard pour le compact, bincode pour la vitesse).
    pub payload: Vec<u8>,
    /// Canal de réponse optionnel (pour request/response pattern).
    pub reply: Option<ReplyPort>,
}

/// Trait pour les messages sérialisables.
pub trait Message: Send + Sized + 'static {
    /// Tag stable pour le dispatch. Dérivé automatiquement du nom du type.
    fn type_tag() -> u64;
    /// Sérialise en bytes.
    fn encode(&self) -> Vec<u8>;
    /// Désérialise depuis bytes.
    fn decode(bytes: &[u8]) -> Result<Self, DecodeError>;
}
```

Un `#[derive(Message)]` macro génère `type_tag` depuis le nom complet du type (hash FNV-1a stable). `encode`/`decode` via `serde` + `postcard` (compact, no-std compatible, WASM friendly).

### Handler — un rôle

```rust
/// Un handler sait traiter un type de message.
pub trait Handler: Send + 'static {
    /// Quel type de message ce handler traite.
    fn type_tag(&self) -> u64;

    /// Traite le message. Accès mutable à l'état de l'acteur.
    fn handle(
        &self,
        state: &mut ActorState,
        payload: &[u8],
        reply: Option<ReplyPort>,
    ) -> ActorStatus;

    /// Priorité de ce handler (pour le scheduling).
    fn priority(&self) -> Priority {
        Priority::Medium
    }
}

/// Helper macro : crée un Handler typé depuis une closure.
/// Gère la désérialisation automatiquement.
macro_rules! handler {
    ($msg_type:ty, |$state:ident, $msg:ident, $reply:ident| $body:expr) => {
        // Désérialise $msg_type depuis payload, appelle la closure
    };
}
```

Exemple d'usage :

```rust
// Définir un message
#[derive(Message, Serialize, Deserialize)]
struct SearchMsg {
    weight_bytes: Vec<u8>,  // Weight sérialisé
    top_k: usize,
}

#[derive(Message, Serialize, Deserialize)]
struct SearchReply {
    hits: Vec<(f32, u64)>,
}

// Créer un handler
let search_handler = handler!(SearchMsg, |state, msg, reply| {
    let handle: &Arc<LucivyHandle> = state.get::<Arc<LucivyHandle>>()?;
    let weight = deserialize_weight(&msg.weight_bytes)?;
    let results = execute_weight_on_shard(handle, &*weight, msg.top_k)?;
    reply.send(SearchReply { hits: results })?;
    ActorStatus::Continue
});
```

### ActorState — bag de resources

```rust
/// État dynamique d'un acteur. Chaque resource est typée et nommée.
pub struct ActorState {
    resources: HashMap<TypeId, Box<dyn Any + Send>>,
}

impl ActorState {
    /// Ajouter une resource.
    pub fn insert<T: Send + 'static>(&mut self, value: T) {
        self.resources.insert(TypeId::of::<T>(), Box::new(value));
    }

    /// Lire une resource.
    pub fn get<T: Send + 'static>(&self) -> Option<&T> {
        self.resources.get(&TypeId::of::<T>())?.downcast_ref()
    }

    /// Lire une resource mutable.
    pub fn get_mut<T: Send + 'static>(&mut self) -> Option<&mut T> {
        self.resources.get_mut(&TypeId::of::<T>())?.downcast_mut()
    }

    /// Sérialiser l'état entier (pour migration).
    pub fn serialize(&self) -> Vec<u8> { /* ... */ }

    /// Désérialiser (pour restauration).
    pub fn deserialize(bytes: &[u8]) -> Self { /* ... */ }
}
```

### GenericActor — l'acteur universel

```rust
pub struct GenericActor {
    id: ActorId,
    state: ActorState,
    handlers: HashMap<u64, Box<dyn Handler>>,  // type_tag → handler
    mailbox: Receiver<Envelope>,
}

impl GenericActor {
    /// Enregistrer un handler (= ajouter un rôle).
    pub fn register(&mut self, handler: impl Handler) {
        self.handlers.insert(handler.type_tag(), Box::new(handler));
    }

    /// Retirer un handler (= enlever un rôle).
    pub fn unregister(&mut self, type_tag: u64) {
        self.handlers.remove(&type_tag);
    }

    /// Dispatch un envelope au bon handler.
    fn dispatch(&mut self, envelope: Envelope) -> ActorStatus {
        match self.handlers.get(&envelope.type_tag) {
            Some(handler) => handler.handle(
                &mut self.state,
                &envelope.payload,
                envelope.reply,
            ),
            None => {
                // Message inconnu : log + drop (ou forward à un superviseur)
                eprintln!("actor {} has no handler for type_tag {}", self.id, envelope.type_tag);
                ActorStatus::Continue
            }
        }
    }
}
```

### Scheduler — inchangé dans le principe

Le scheduler existant reste le même. La seule différence : au lieu de `ActorWrapper<A: Actor>`, on a directement `GenericActor` qui implémente `AnyActor`.

```rust
impl AnyActor for GenericActor {
    fn try_handle_one(&mut self) -> Option<ActorStatus> {
        let envelope = self.mailbox.try_recv()?;
        Some(self.dispatch(envelope))
    }

    fn priority(&self) -> Priority {
        // Priorité max parmi les handlers enregistrés,
        // ou basée sur le type du dernier message reçu
        Priority::Medium
    }

    fn name(&self) -> &'static str {
        "generic"  // ou configurable
    }
}
```

## Rôles pour le sharding lucivy

```rust
// Un shard actor = GenericActor avec ces rôles :

fn create_shard_actor(shard_id: usize, handle: Arc<LucivyHandle>) -> GenericActor {
    let mut actor = GenericActor::new();

    // Injecter l'état
    actor.state.insert::<Arc<LucivyHandle>>(handle);
    actor.state.insert::<usize>(shard_id);
    actor.state.insert::<Vec<LucivyDocument>>(Vec::new());  // buffer d'insertion

    // Enregistrer les rôles
    actor.register(SearchHandler);
    actor.register(InsertHandler);
    actor.register(InsertBatchHandler);
    actor.register(CommitHandler);
    actor.register(DeleteHandler);
    actor.register(FlushHandler);

    actor
}
```

### Load balancing

Un acteur surchargé peut transférer un rôle à un acteur idle :

```rust
// L'acteur shard_0 est surchargé en search, shard_3 est idle
// Le superviseur migre le rôle search du shard_0 vers shard_3

// 1. Sérialiser l'état nécessaire (IndexReader est Arc, clonable)
let reader = shard_0.state.get::<Arc<LucivyHandle>>().clone();

// 2. Donner l'état + handler à shard_3
shard_3.state.insert::<Arc<LucivyHandle>>(reader);
shard_3.register(SearchHandler);

// 3. Maintenant shard_3 peut aussi servir les search du shard 0
```

## Transport réseau (futur)

L'Envelope est déjà des bytes. Pour le distribué :

```rust
/// Transport = comment envoyer des Envelopes entre machines.
pub trait Transport: Send + Sync {
    fn send(&self, target: ActorAddress, envelope: Envelope) -> Result<()>;
    fn receive(&self) -> Result<Envelope>;
}

/// Adresse réseau d'un acteur.
pub struct ActorAddress {
    pub node: String,     // "192.168.1.10:9090"
    pub actor_id: ActorId,
}
```

L'`ActorRef` actuel devient transparent : si l'acteur est local → flume channel. Si distant → Transport. L'appelant ne sait pas.

```rust
pub enum ActorRef {
    Local(flume::Sender<Envelope>),
    Remote(ActorAddress, Arc<dyn Transport>),
}

impl ActorRef {
    pub fn send(&self, envelope: Envelope) -> Result<()> {
        match self {
            Self::Local(sender) => sender.send(envelope)?,
            Self::Remote(addr, transport) => transport.send(addr.clone(), envelope)?,
        }
        Ok(())
    }
}
```

## Migration progressive

On ne jette pas le système actuel. On migre par étapes :

### Phase 1 : Envelope + Handler (local)
- Implémenter `Envelope`, `Message` trait, `Handler` trait, `ActorState`, `GenericActor`
- Le scheduler supporte `GenericActor` en plus des `Actor<Msg>` typés existants
- Migrer `ShardSearchActor` → `GenericActor` avec SearchHandler + InsertHandler
- Les acteurs typés existants (IndexerActor, MergerActor, etc.) restent inchangés

### Phase 2 : Derive macro + serde
- `#[derive(Message)]` pour auto-générer type_tag + encode/decode
- postcard comme format de sérialisation (compact, no-std, WASM)
- Tests de roundtrip sérialisation pour tous les messages

### Phase 3 : ActorState sérialisable
- Sérialiser/désérialiser l'état complet d'un acteur
- Migration d'acteur entre threads (pour load balancing local)
- Snapshot d'acteur (pour debugging, replay)

### Phase 4 : Transport réseau
- Trait `Transport` (TCP, QUIC, WebSocket)
- `ActorRef::Remote` transparent
- Discovery service (quel noeud a quel acteur)
- Extraction en lib standalone `luciole`

## Compatibilité WASM

- postcard est no-std → fonctionne en WASM
- Le scheduler coopératif existant reste identique
- Pas de threads réseau en WASM → Transport via WebSocket (SharedArrayBuffer + Atomics)
- Les Envelopes transitent par le même SAB ring buffer que les logs actuels

## Estimation

- Phase 1 : ~300 lignes (Envelope, Handler, ActorState, GenericActor, migration ShardActor)
- Phase 2 : ~100 lignes (derive macro) + dépendance postcard
- Phase 3 : ~200 lignes (sérialisation état)
- Phase 4 : ~500 lignes (transport, discovery) — lib séparée

## Nommage : Luciole

- Luci(vy) + (fire)fly = Luciole
- Actors that glow in the dark, carry messages, and can fly anywhere
- `luciole::Actor`, `luciole::Envelope`, `luciole::Handler`, `luciole::Scheduler`
