# luciole

Actor runtime + DAG execution engine for Rust. WASM-safe, deadlock-aware, built for real workloads.

## Why luciole

Most actor frameworks stop at message passing. luciole adds structured concurrency on top: DAGs with checkpoints, streaming pipelines, non-blocking request-reply, and built-in deadlock diagnostics. Everything runs on a fixed thread pool that works identically on native and WASM.

## Features

### Actors

- **Actor trait** — typed messages, priority scheduling (Idle → Critical)
- **GenericActor** — dynamic handler registration by type tag, no enum boilerplate
- **Pool** — N identical actors with scatter/gather and round-robin routing
- **Envelope** — serialized messages with stable type tags. Carry non-serializable data via `local` field for zero-cost local transport

### DAG execution

- **Dag** — build directed acyclic graphs of computation nodes
- **execute_dag** — topological execution with parallel fan-out per level
- **execute_dag_async** — non-blocking DAG execution via DagExecutor actor
- **BranchNode** — conditional 2-way branching (`BranchNode(|| condition)`)
- **GateNode** — pass/block gate
- **MergeNode** — fan-out with merge (`fan_out_merge`)
- **ScatterDAG** — distributed fan-out across workers
- **Undo** — per-node rollback support
- **Checkpoint** — save/restore DAG progress (FileCheckpointStore, MemoryCheckpointStore)
- **Services** — named service injection via `Dag::with_services(Arc<ServiceRegistry>)`

### Streaming

- **StreamDag** — pipeline topology with topological drain. Feed items through a chain of actors, drain in dependency order.

### Non-blocking request-reply

- **pipe_to** — "send message, get result as a message back". Callback registered BEFORE send — no race condition.
- **collect_replies_to** — N:1 gather. Send N requests, get 1 message when all complete.
- **task_pipe_to** — submit CPU work to thread pool, pipe result to actor as message.

### Scheduling

- **Persistent thread pool** — fixed N threads, no spawn/destroy overhead
- **WASM-safe** — same code runs native (multi-threaded) and WASM (cooperative)
- **Priority scheduling** — actors with higher priority are processed first
- **Cooperative wait** — `scheduler.wait(rx, label)` pumps the scheduler while waiting
- **Activity labels** — `ctx.set_activity("processing doc 42/500")` visible in dumps

### Diagnostics

- **WaitGraph** — tracks all inter-thread/inter-actor dependencies. Dumps as mermaid or text.
- **ActorActivity** — dynamic labels on what each actor is doing, visible in scheduler dumps
- **Event subscription** — `subscribe_dag_events()` for DAG progress monitoring
- **TapRegistry** — side-channel introspection of DAG nodes

## Quick start

```rust
use luciole::*;

// Define a message
struct Ping;
impl Message for Ping {
    fn type_tag() -> u64 { type_tag_hash(b"Ping") }
    fn encode(&self) -> Vec<u8> { vec![] }
    fn decode(_: &[u8]) -> Result<Self, String> { Ok(Ping) }
}

// Create an actor
let mut actor = GenericActor::new("pinger");
actor.register(TypedHandler::<Ping, _>::new(
    |_state, _msg, _reply, _local, _ctx| {
        println!("pong!");
        ActorStatus::Continue
    },
));

// Spawn and send
let scheduler = scheduler::global_scheduler();
let actor_ref = scheduler.spawn_generic(actor);
actor_ref.send(Ping.into_envelope());
```

### pipe_to example

```rust
// Non-blocking request → reply as message
let target: ActorRef<MyMsg> = /* ... */;
let worker: ActorRef<WorkMsg> = /* ... */;

// "Send Work to worker, when done send ResultMsg to target"
worker.pipe_to(
    |reply| WorkMsg::Process { reply },
    &target,
    "work_result",
    |result| MyMsg::WorkDone(result),
);
```

### DAG example

```rust
let mut dag = Dag::new();
let a = dag.add_node(MyNode::new("fetch"));
let b = dag.add_node(MyNode::new("parse"));
let c = dag.add_node(MyNode::new("index"));
dag.add_edge(a, b);
dag.add_edge(b, c);

let result = execute_dag(dag)?;
```

## WASM compatibility

luciole runs identically on native and WASM:

- **Native**: scheduler runs N worker threads
- **WASM (emscripten)**: scheduler runs on emscripten pthreads (SharedArrayBuffer required)
- **No `thread::spawn` in actor handlers** — everything goes through the scheduler

## Tests

```bash
cargo test -p luciole --lib
# 154 tests, 0 failures
```

## License

MIT
