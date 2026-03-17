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
use super::{Actor, ActorStatus, Priority};

/// An actor that dispatches Envelopes to dynamically registered handlers.
pub struct GenericActor {
    state: ActorState,
    handlers: HashMap<u64, Box<dyn Handler>>,
    name: &'static str,
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
        }
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
            ),
            None => {
                // No handler — send error reply if expected, otherwise drop silently.
                if let Some(reply) = envelope.reply {
                    reply.send_err(format!(
                        "actor '{}' has no handler for type_tag {:#018x}",
                        self.name, envelope.type_tag
                    ));
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
        self.handlers
            .values()
            .map(|h| h.priority())
            .max()
            .unwrap_or(Priority::Medium)
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

        actor.register(TypedHandler::<AddMsg, _>::new(|state, msg, _reply| {
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
        actor.register(TypedHandler::<AddMsg, _>::new(|state, msg, _reply| {
            *state.get_mut::<i64>().unwrap() += msg.value;
            ActorStatus::Continue
        }));

        // Get handler (with reply)
        actor.register(TypedHandler::<GetMsg, _>::new(|state, _msg, reply| {
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

        actor.register(TypedHandler::<AddMsg, _>::new(|state, msg, _reply| {
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
        actor.register(TypedHandler::<AddMsg, _>::new(|state, msg, _reply| {
            *state.get_mut::<i64>().unwrap() += msg.value;
            ActorStatus::Continue
        }));
        actor.register(TypedHandler::<GetMsg, _>::with_priority(
            |state, _msg, reply| {
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
}
