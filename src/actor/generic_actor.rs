//! GenericActor — an actor that can take any role at runtime.
//!
//! A GenericActor holds a dynamic state (ActorState) and a set of handlers.
//! Handlers are registered/unregistered at runtime. Incoming Envelopes are
//! dispatched to the matching handler by type_tag.
//!
//! GenericActor implements `Actor<Msg = Envelope>`, so it plugs into the
//! existing scheduler without any changes.

use std::collections::HashMap;

use super::actor_state::ActorState;
use super::envelope::Envelope;
use super::handler::Handler;
use super::mailbox::ActorRef;
use super::{Actor, ActorStatus, Priority};

/// An actor that dispatches Envelopes to dynamically registered handlers.
pub struct GenericActor {
    state: ActorState,
    handlers: HashMap<u64, Box<dyn Handler>>,
    name: &'static str,
    /// Optional dynamic priority function. If set, overrides handler priorities.
    priority_fn: Option<Box<dyn Fn(&ActorState) -> Priority + Send>>,
}

impl GenericActor {
    /// Create a new generic actor with the given name.
    ///
    /// The name must be a `&'static str` (use `Box::leak` for dynamic names).
    pub fn new(name: &'static str) -> Self {
        Self {
            state: ActorState::new(),
            handlers: HashMap::new(),
            name,
            priority_fn: None,
        }
    }

    /// Set a dynamic priority function based on actor state.
    ///
    /// Example: an indexer actor returns High priority when a segment is open
    /// (memory allocated), Low when idle.
    pub fn with_priority_fn(
        mut self,
        f: impl Fn(&ActorState) -> Priority + Send + 'static,
    ) -> Self {
        self.priority_fn = Some(Box::new(f));
        self
    }

    /// Register a handler (= add a role). Replaces any existing handler
    /// for the same type_tag.
    pub fn register(&mut self, handler: impl Handler) {
        self.handlers.insert(handler.type_tag(), Box::new(handler));
    }

    /// Unregister a handler by type_tag (= remove a role).
    pub fn unregister(&mut self, type_tag: u64) {
        self.handlers.remove(&type_tag);
    }

    /// Access the actor's state.
    pub fn state(&self) -> &ActorState {
        &self.state
    }

    /// Mutable access to the actor's state.
    pub fn state_mut(&mut self) -> &mut ActorState {
        &mut self.state
    }

    /// Number of registered handlers.
    pub fn num_handlers(&self) -> usize {
        self.handlers.len()
    }

    /// Check if a handler is registered for this type_tag.
    pub fn has_handler(&self, type_tag: u64) -> bool {
        self.handlers.contains_key(&type_tag)
    }

    /// Dispatch an envelope to the matching handler.
    fn dispatch(&mut self, envelope: Envelope) -> ActorStatus {
        match self.handlers.get(&envelope.type_tag) {
            Some(handler) => handler.handle(
                &mut self.state,
                &envelope.payload,
                envelope.reply,
                envelope.local,
            ),
            None => {
                // No handler — send raw error bytes (no dependency on app error type).
                if let Some(reply) = envelope.reply {
                    let msg = format!(
                        "actor '{}' has no handler for type_tag {:#018x}",
                        self.name, envelope.type_tag
                    );
                    reply.send_err_bytes(msg.into_bytes());
                }
                ActorStatus::Continue
            }
        }
    }
}

/// GenericActor plugs into the existing scheduler as `Actor<Msg = Envelope>`.
impl Actor for GenericActor {
    type Msg = Envelope;

    fn name(&self) -> &'static str {
        self.name
    }

    fn handle(&mut self, msg: Envelope) -> ActorStatus {
        self.dispatch(msg)
    }

    fn priority(&self) -> Priority {
        if let Some(ref f) = self.priority_fn {
            f(&self.state)
        } else {
            self.handlers
                .values()
                .map(|h| h.priority())
                .max()
                .unwrap_or(Priority::Medium)
        }
    }

    fn on_start(&mut self, self_ref: ActorRef<Envelope>) {
        self.state.insert::<ActorRef<Envelope>>(self_ref);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actor::actor_state::ActorState;
    use crate::actor::envelope::{type_tag_hash, Message, ReplyPort};
    use crate::actor::handler::TypedHandler;
    use crate::actor::mailbox::mailbox;
    use crate::actor::scheduler::global_scheduler;
    use std::sync::Arc;

    // ─── Test error type ─────────────────────────────────────────────

    #[derive(Debug)]
    struct TestError(String);
    impl Message for TestError {
        fn type_tag() -> u64 { type_tag_hash(b"TestError") }
        fn encode(&self) -> Vec<u8> { self.0.as_bytes().to_vec() }
        fn decode(bytes: &[u8]) -> Result<Self, String> {
            Ok(Self(String::from_utf8_lossy(bytes).to_string()))
        }
    }

    // ─── Test messages ──────────────────────────────────────────────

    struct AddMsg {
        value: i64,
    }

    impl Message for AddMsg {
        fn type_tag() -> u64 { type_tag_hash(b"AddMsg") }
        fn encode(&self) -> Vec<u8> { self.value.to_le_bytes().to_vec() }
        fn decode(bytes: &[u8]) -> Result<Self, String> {
            if bytes.len() < 8 { return Err("too short".into()); }
            Ok(Self { value: i64::from_le_bytes(bytes[..8].try_into().unwrap()) })
        }
    }

    struct GetMsg;

    impl Message for GetMsg {
        fn type_tag() -> u64 { type_tag_hash(b"GetMsg") }
        fn encode(&self) -> Vec<u8> { vec![] }
        fn decode(_bytes: &[u8]) -> Result<Self, String> { Ok(Self) }
    }

    struct ValueReply {
        value: i64,
    }

    impl Message for ValueReply {
        fn type_tag() -> u64 { type_tag_hash(b"ValueReply") }
        fn encode(&self) -> Vec<u8> { self.value.to_le_bytes().to_vec() }
        fn decode(bytes: &[u8]) -> Result<Self, String> {
            if bytes.len() < 8 { return Err("too short".into()); }
            Ok(Self { value: i64::from_le_bytes(bytes[..8].try_into().unwrap()) })
        }
    }

    // ─── Tests ──────────────────────────────────────────────────────

    #[test]
    fn test_generic_actor_dispatch() {
        let mut actor = GenericActor::new("counter");
        actor.state_mut().insert::<i64>(0);

        actor.register(TypedHandler::<AddMsg, _>::new(|state, msg, _reply, _local| {
            *state.get_mut::<i64>().unwrap() += msg.value;
            ActorStatus::Continue
        }));

        assert_eq!(actor.num_handlers(), 1);
        assert!(actor.has_handler(AddMsg::type_tag()));

        // Dispatch
        let env = AddMsg { value: 10 }.into_envelope();
        actor.handle(env);
        let env = AddMsg { value: 32 }.into_envelope();
        actor.handle(env);

        assert_eq!(*actor.state().get::<i64>().unwrap(), 42);
    }

    #[test]
    fn test_generic_actor_multiple_handlers() {
        let mut actor = GenericActor::new("multi");
        actor.state_mut().insert::<i64>(100);

        // Add handler
        actor.register(TypedHandler::<AddMsg, _>::new(|state, msg, _reply, _local| {
            *state.get_mut::<i64>().unwrap() += msg.value;
            ActorStatus::Continue
        }));

        // Get handler (with reply)
        actor.register(TypedHandler::<GetMsg, _>::new(|state, _msg, reply, _local| {
            let value = *state.get::<i64>().unwrap();
            if let Some(reply) = reply {
                reply.send(ValueReply { value });
            }
            ActorStatus::Continue
        }));

        assert_eq!(actor.num_handlers(), 2);

        // Add 50
        actor.handle(AddMsg { value: 50 }.into_envelope());

        // Get value via request/response
        let (env, rx) = GetMsg.into_request();
        actor.handle(env);
        let reply_bytes = rx.wait_blocking().unwrap();
        let reply = ValueReply::decode(&reply_bytes).unwrap();
        assert_eq!(reply.value, 150);
    }

    #[test]
    fn test_generic_actor_unknown_message() {
        let mut actor = GenericActor::new("empty");
        // No handlers registered

        // Send a message with reply — should get error
        let (env, rx) = AddMsg { value: 1 }.into_request();
        actor.handle(env);
        let result = rx.wait_blocking();
        assert!(result.is_err());
    }

    #[test]
    fn test_generic_actor_unregister() {
        let mut actor = GenericActor::new("unreg");
        actor.state_mut().insert::<i64>(0);

        actor.register(TypedHandler::<AddMsg, _>::new(|state, msg, _reply, _local| {
            *state.get_mut::<i64>().unwrap() += msg.value;
            ActorStatus::Continue
        }));

        actor.handle(AddMsg { value: 5 }.into_envelope());
        assert_eq!(*actor.state().get::<i64>().unwrap(), 5);

        // Unregister
        actor.unregister(AddMsg::type_tag());
        assert_eq!(actor.num_handlers(), 0);

        // Now dispatching AddMsg should be a no-op (no reply expected)
        actor.handle(AddMsg { value: 100 }.into_envelope());
        assert_eq!(*actor.state().get::<i64>().unwrap(), 5); // unchanged
    }

    #[test]
    fn test_generic_actor_in_scheduler() {
        let scheduler = global_scheduler();

        let mut actor = GenericActor::new("sched-test");
        actor.state_mut().insert::<i64>(0);
        actor.register(TypedHandler::<AddMsg, _>::new(|state, msg, _reply, _local| {
            *state.get_mut::<i64>().unwrap() += msg.value;
            ActorStatus::Continue
        }));
        actor.register(TypedHandler::<GetMsg, _>::with_priority(
            |state, _msg, reply, _local| {
                let value = *state.get::<i64>().unwrap();
                if let Some(reply) = reply {
                    reply.send(ValueReply { value });
                }
                ActorStatus::Continue
            },
            Priority::Critical,
        ));

        let (mb, mut actor_ref) = mailbox::<Envelope>(64);
        scheduler.spawn(actor, mb, &mut actor_ref, 64);

        // Send Add messages
        actor_ref.send(AddMsg { value: 10 }.into_envelope()).unwrap();
        actor_ref.send(AddMsg { value: 20 }.into_envelope()).unwrap();
        actor_ref.send(AddMsg { value: 12 }.into_envelope()).unwrap();

        // Send Get request
        let (env, rx) = GetMsg.into_request();
        actor_ref.send(env).unwrap();

        let reply_bytes = rx.wait_cooperative(|| scheduler.run_one_step());
        let reply = ValueReply::decode(&reply_bytes.unwrap()).unwrap();
        assert_eq!(reply.value, 42);
    }

    #[test]
    fn test_typed_actor_ref() {
        use crate::actor::envelope::TypedActorRef;

        let scheduler = global_scheduler();

        let mut actor = GenericActor::new("typed-ref-test");
        actor.state_mut().insert::<i64>(0);
        actor.register(TypedHandler::<AddMsg, _>::new(|state, msg, _reply, _local| {
            *state.get_mut::<i64>().unwrap() += msg.value;
            ActorStatus::Continue
        }));
        actor.register(TypedHandler::<GetMsg, _>::with_priority(
            |state, _msg, reply, _local| {
                let value = *state.get::<i64>().unwrap();
                if let Some(reply) = reply {
                    reply.send(ValueReply { value });
                }
                ActorStatus::Continue
            },
            Priority::Critical,
        ));

        let (mb, mut actor_ref) = mailbox::<Envelope>(64);
        scheduler.spawn(actor, mb, &mut actor_ref, 64);

        let typed = TypedActorRef::new(actor_ref);

        // Fire-and-forget
        typed.send(AddMsg { value: 10 }).unwrap();
        typed.send(AddMsg { value: 32 }).unwrap();

        // Request/response
        let reply: ValueReply = typed.request::<GetMsg, ValueReply, TestError>(GetMsg).unwrap();
        assert_eq!(reply.value, 42);
    }

    #[test]
    fn test_priority_fn() {
        let actor = GenericActor::new("prio-test")
            .with_priority_fn(|state| {
                if state.get::<bool>().copied().unwrap_or(false) {
                    Priority::High
                } else {
                    Priority::Low
                }
            });

        // No bool in state → false → Low
        assert_eq!(actor.priority(), Priority::Low);

        // Manually test with state change
        let mut actor = actor;
        actor.state_mut().insert::<bool>(true);
        assert_eq!(actor.priority(), Priority::High);

        actor.state_mut().insert::<bool>(false);
        assert_eq!(actor.priority(), Priority::Low);
    }

    #[test]
    fn test_on_start_stores_self_ref() {
        let scheduler = global_scheduler();

        // SelfPingMsg: the actor sends itself an AddMsg(100) via self_ref,
        // then replies OK so the caller knows the self-message was sent.
        struct SelfPingMsg;
        impl Message for SelfPingMsg {
            fn type_tag() -> u64 { type_tag_hash(b"SelfPingMsg") }
            fn encode(&self) -> Vec<u8> { vec![] }
            fn decode(_: &[u8]) -> Result<Self, String> { Ok(Self) }
        }

        struct OkReply;
        impl Message for OkReply {
            fn type_tag() -> u64 { type_tag_hash(b"OkReply") }
            fn encode(&self) -> Vec<u8> { vec![] }
            fn decode(_: &[u8]) -> Result<Self, String> { Ok(Self) }
        }

        let mut actor = GenericActor::new("self-msg-test");
        actor.state_mut().insert::<i64>(0);

        // Add handler
        actor.register(TypedHandler::<AddMsg, _>::new(|state, msg, _reply, _local| {
            *state.get_mut::<i64>().unwrap() += msg.value;
            ActorStatus::Continue
        }));

        // SelfPing handler: send AddMsg(100) to self, then reply OK
        actor.register(TypedHandler::<SelfPingMsg, _>::new(|state, _msg, reply, _local| {
            let self_ref = state.get::<ActorRef<Envelope>>().unwrap();
            let _ = self_ref.send(AddMsg { value: 100 }.into_envelope());
            if let Some(reply) = reply {
                reply.send(OkReply);
            }
            ActorStatus::Continue
        }));

        // Get handler
        actor.register(TypedHandler::<GetMsg, _>::with_priority(
            |state, _msg, reply, _local| {
                let value = *state.get::<i64>().unwrap();
                if let Some(reply) = reply {
                    reply.send(ValueReply { value });
                }
                ActorStatus::Continue
            },
            Priority::Critical,
        ));

        let (mb, mut actor_ref) = mailbox::<Envelope>(64);
        scheduler.spawn(actor, mb, &mut actor_ref, 64);

        // Send SelfPing and wait for reply (ensures self-message was sent)
        let (env, rx) = SelfPingMsg.into_request();
        actor_ref.send(env).unwrap();
        let _ = rx.wait_cooperative(|| scheduler.run_one_step());

        // Now Get — the AddMsg(100) from self-message is in the mailbox
        // and will be processed before this Get (FIFO)
        let (env, rx) = GetMsg.into_request();
        actor_ref.send(env).unwrap();
        let reply_bytes = rx.wait_cooperative(|| scheduler.run_one_step());
        let reply = ValueReply::decode(&reply_bytes.unwrap()).unwrap();
        assert_eq!(reply.value, 100);
    }
}
