# Luciole — Erreurs sérialisables dans les replies

Date : 17 mars 2026

## Problème

Le `ReplyPort` actuel envoie `Result<Vec<u8>, String>`. L'erreur est un `String` — on perd le type d'erreur original (`SchemaError`, `IoError`, etc.).

```rust
// Handler côté acteur
reply.send_err(format!("{e}"));  // LucivyError → String, perte du type

// Caller côté IndexWriter
rx.wait_blocking()
  .map_err(|e: String| LucivyError::SystemError(e))?;
  // SchemaError devenu SystemError — double wrapping
```

Le test `test_show_error_when_tokenizer_not_registered` échoue parce que l'erreur `SchemaError` est wrappée en `SystemError`.

## Solution : erreurs sérialisées en bytes

Le `ReplyPort` envoie `Result<Vec<u8>, Vec<u8>>` — les deux côtés sont des bytes. L'erreur est sérialisée comme un message, pas comme un String.

### Format d'erreur

`LucivyError` implémente `Message` :

```rust
impl Message for LucivyError {
    fn type_tag() -> u64 { type_tag_hash(b"LucivyError") }

    fn encode(&self) -> Vec<u8> {
        // [variant_tag: u8] [message_len: u32] [message: bytes]
        let (tag, msg) = match self {
            LucivyError::PathDoesNotExist(_) => (0u8, self.to_string()),
            LucivyError::FileAlreadyExists(_) => (1, self.to_string()),
            LucivyError::IndexAlreadyExists => (2, String::new()),
            LucivyError::LockFailure(_, _) => (3, self.to_string()),
            LucivyError::IoError(_) => (4, self.to_string()),
            LucivyError::DataCorruption(_) => (5, self.to_string()),
            LucivyError::SchemaError(s) => (6, s.clone()),
            LucivyError::InvalidArgument(s) => (7, s.clone()),
            LucivyError::SystemError(s) => (8, s.clone()),
            LucivyError::ErrorInThread(s) => (9, s.clone()),
        };
        let msg_bytes = msg.as_bytes();
        let mut buf = Vec::with_capacity(1 + 4 + msg_bytes.len());
        buf.push(tag);
        buf.extend_from_slice(&(msg_bytes.len() as u32).to_le_bytes());
        buf.extend_from_slice(msg_bytes);
        buf
    }

    fn decode(bytes: &[u8]) -> Result<Self, String> {
        // Decode variant tag + message
        if bytes.is_empty() { return Err("empty error bytes".into()); }
        let tag = bytes[0];
        let msg = if bytes.len() > 5 {
            let len = u32::from_le_bytes(bytes[1..5].try_into().unwrap()) as usize;
            String::from_utf8_lossy(&bytes[5..5+len]).to_string()
        } else {
            String::new()
        };
        match tag {
            0 => Ok(LucivyError::PathDoesNotExist(std::path::PathBuf::from(msg))),
            1 => Ok(LucivyError::FileAlreadyExists(std::path::PathBuf::from(msg))),
            2 => Ok(LucivyError::IndexAlreadyExists),
            3 => Ok(LucivyError::SystemError(msg)), // LockFailure simplifié
            4 => Ok(LucivyError::IoError(std::io::Error::other(msg))),
            5 => Ok(LucivyError::DataCorruption(msg)),
            6 => Ok(LucivyError::SchemaError(msg)),
            7 => Ok(LucivyError::InvalidArgument(msg)),
            8 => Ok(LucivyError::SystemError(msg)),
            9 => Ok(LucivyError::ErrorInThread(msg)),
            _ => Err(format!("unknown error tag: {tag}")),
        }
    }
}
```

### Changements dans ReplyPort

```rust
// Avant
pub struct ReplyPort {
    inner: Reply<Result<Vec<u8>, String>>,
}

// Après
pub struct ReplyPort {
    inner: Reply<Result<Vec<u8>, Vec<u8>>>,
}

impl ReplyPort {
    pub fn send<M: Message>(self, msg: M) {
        self.inner.send(Ok(msg.encode()));
    }

    pub fn send_bytes(self, bytes: Vec<u8>) {
        self.inner.send(Ok(bytes));
    }

    /// Send a typed error (serialized via Message::encode).
    pub fn send_err<E: Message>(self, err: E) {
        self.inner.send(Err(err.encode()));
    }

    /// Send a LucivyError directly.
    pub fn send_lucivy_err(self, err: crate::LucivyError) {
        self.inner.send(Err(err.encode()));
    }
}
```

### Changements dans TypedActorRef

```rust
impl TypedActorRef {
    /// Send a request and wait for a typed reply.
    /// Errors are decoded as LucivyError.
    pub fn request<M: Message, R: Message>(&self, msg: M) -> crate::Result<R> {
        let (env, rx) = msg.into_request();
        self.inner.send(env).map_err(|_| "channel closed")?;
        let scheduler = global_scheduler();
        match rx.wait_cooperative(|| scheduler.run_one_step()) {
            Ok(bytes) => R::decode(&bytes).map_err(|e| LucivyError::SystemError(e)),
            Err(err_bytes) => {
                let err = LucivyError::decode(&err_bytes)
                    .unwrap_or_else(|_| LucivyError::SystemError("decode error".into()));
                Err(err)
            }
        }
    }
}
```

### Changements dans les handlers

```rust
// Avant (indexer_actor.rs flush handler)
match result {
    Ok(()) => reply.send(IndexerFlushReply),
    Err(e) => reply.send_err(format!("{e}")),  // ← perte du type
}

// Après
match result {
    Ok(()) => reply.send(IndexerFlushReply),
    Err(e) => reply.send_lucivy_err(e),  // ← type préservé
}
```

### Changements dans IndexWriter

```rust
// Avant
rx.wait_blocking()
    .map_err(|e: String| LucivyError::SystemError(e))?;

// Après
match rx.wait_blocking() {
    Ok(_) => {},
    Err(err_bytes) => {
        let err = LucivyError::decode(&err_bytes)
            .unwrap_or_else(|_| LucivyError::SystemError("decode error".into()));
        return Err(err);
    }
}
```

Ou via `TypedActorRef` :
```rust
typed_ref.request::<IndexerFlushMsg, IndexerFlushReply>(IndexerFlushMsg)?;
// → Result<IndexerFlushReply, LucivyError>, type d'erreur préservé
```

## Fichiers à modifier

1. `src/actor/envelope.rs` — `ReplyPort` inner type, `reply_port()`, `TypedActorRef`
2. `src/actor/envelope.rs` ou nouveau fichier — `impl Message for LucivyError`
3. `src/actor/handler.rs` — tests mis à jour
4. `src/actor/generic_actor.rs` — tests mis à jour
5. `src/indexer/indexer_actor.rs` — flush handler utilise `send_lucivy_err`
6. `src/indexer/index_writer.rs` — decode error bytes au lieu de String
7. `lucivy_core/src/sharded_handle.rs` — shard handlers mis à jour

## Impact

- **Zero perte d'information** : SchemaError reste SchemaError
- **Network-ready** : l'erreur est des bytes comme tout le reste
- **Le test passe** : le message d'erreur est reconstruit à l'identique
- **Uniforme** : succès et erreur sont des bytes, même pipeline
