# Luciole

Generic actor runtime for Rust with serialized messages, dynamic roles, and WASM support.

## Features

- **Envelope-based messaging** — messages are serialized bytes with a stable type tag (FNV-1a hash). Transport-agnostic: local or network.
- **Dynamic roles** — actors register/unregister handlers at runtime. Any actor can take any role without recompilation.
- **GenericActor** — universal actor type with dynamic state (`ActorState`) and handler registry. Implements the `Actor` trait for seamless scheduler integration.
- **TypedActorRef** — ergonomic typed facade over `ActorRef<Envelope>`. Compile-time safe send/request with automatic encode/decode.
- **Generic errors** — `ActorError<E>` is parameterized over the application's error type. No coupling to any specific error enum.
- **Cooperative scheduling** — works with persistent thread pools (native) or single-thread cooperative mode (WASM). Same code, same API.
- **Envelope.local** — carry non-serializable data (e.g. `Arc<dyn Trait>`) alongside serialized payloads for zero-cost local transport.

## Quick start

```rust
use luciole::*;

// Define a message
struct AddMsg { value: i64 }

impl Message for AddMsg {
    fn type_tag() -> u64 { type_tag_hash(b"AddMsg") }
    fn encode(&self) -> Vec<u8> { self.value.to_le_bytes().to_vec() }
    fn decode(bytes: &[u8]) -> Result<Self, String> {
        Ok(Self { value: i64::from_le_bytes(bytes[..8].try_into().unwrap()) })
    }
}

// Create an actor with a handler
let mut actor = GenericActor::new("counter");
actor.state_mut().insert::<i64>(0);
actor.register(TypedHandler::<AddMsg, _>::new(|state, msg, _reply, _local| {
    *state.get_mut::<i64>().unwrap() += msg.value;
    ActorStatus::Continue
}));

// Spawn in the scheduler
let scheduler = scheduler::global_scheduler();
let (mb, mut actor_ref) = mailbox::<Envelope>(64);
scheduler.spawn(actor, mb, &mut actor_ref, 64);

// Send messages
actor_ref.send(AddMsg { value: 42 }.into_envelope()).unwrap();
```

## Architecture

```
Caller → TypedActorRef::send(msg) → Envelope { type_tag, payload, local }
                                          ↓
                                    ActorRef<Envelope>::send()
                                          ↓
                                    Scheduler (thread pool or cooperative)
                                          ↓
                                    GenericActor::dispatch()
                                          ↓
                                    handlers[type_tag].handle(state, payload, reply, local)
```

## WASM compatibility

Luciole supports two execution modes:

- **Multi-threaded** (native): scheduler runs N worker threads. `ReplyReceiver::wait_blocking()` blocks the caller thread.
- **Single-threaded** (WASM): no worker threads. `ReplyReceiver::wait_cooperative(|| scheduler.run_one_step())` pumps the scheduler between reply checks.

Same actor code works in both modes.

## Dependencies

- `flume` — bounded MPSC channel (lightweight, no-std friendly)
- `std` — threading, synchronization, collections

## License

MIT
