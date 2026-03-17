//! Handler — typed message handler for generic actors.
//!
//! A Handler knows how to deserialize one message type and process it
//! with access to the actor's state. Handlers are registered dynamically
//! on a GenericActor to give it roles.

use std::any::Any;
use std::marker::PhantomData;

use super::actor_state::ActorState;
use super::envelope::{Message, ReplyPort};
use super::{ActorStatus, Priority};

/// A handler that processes one type of serialized message.
pub trait Handler: Send + 'static {
    /// Which message type this handler processes (stable hash).
    fn type_tag(&self) -> u64;

    /// Process a serialized message with access to actor state.
    ///
    /// `local` carries non-serializable data (e.g. Arc<dyn Weight>) in local mode.
    fn handle(
        &self,
        state: &mut ActorState,
        payload: &[u8],
        reply: Option<ReplyPort>,
        local: Option<Box<dyn Any + Send>>,
    ) -> ActorStatus;

    /// Scheduling priority for this handler.
    fn priority(&self) -> Priority {
        Priority::Medium
    }
}

/// A typed handler: wraps a closure that processes a specific Message type.
///
/// Handles deserialization automatically. The closure receives the decoded
/// message and mutable access to the actor state.
pub struct TypedHandler<M, F> {
    handler_fn: F,
    priority: Priority,
    _phantom: PhantomData<fn() -> M>,
}

impl<M, F> TypedHandler<M, F>
where
    M: Message,
    F: Fn(&mut ActorState, M, Option<ReplyPort>, Option<Box<dyn Any + Send>>) -> ActorStatus
        + Send
        + 'static,
{
    /// Create a new typed handler with default priority (Medium).
    pub fn new(f: F) -> Self {
        Self {
            handler_fn: f,
            priority: Priority::Medium,
            _phantom: PhantomData,
        }
    }

    /// Create a new typed handler with a specific priority.
    pub fn with_priority(f: F, priority: Priority) -> Self {
        Self {
            handler_fn: f,
            priority,
            _phantom: PhantomData,
        }
    }
}

impl<M, F> Handler for TypedHandler<M, F>
where
    M: Message,
    F: Fn(&mut ActorState, M, Option<ReplyPort>, Option<Box<dyn Any + Send>>) -> ActorStatus
        + Send
        + 'static,
{
    fn type_tag(&self) -> u64 {
        M::type_tag()
    }

    fn handle(
        &self,
        state: &mut ActorState,
        payload: &[u8],
        reply: Option<ReplyPort>,
        local: Option<Box<dyn Any + Send>>,
    ) -> ActorStatus {
        match M::decode(payload) {
            Ok(msg) => (self.handler_fn)(state, msg, reply, local),
            Err(e) => {
                if let Some(reply) = reply {
                    reply.send_err(crate::LucivyError::SystemError(
                        format!("decode error: {e}")
                    ));
                }
                ActorStatus::Continue
            }
        }
    }

    fn priority(&self) -> Priority {
        self.priority
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actor::envelope::{self, type_tag_hash};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    // Test message
    struct IncrMsg {
        amount: u32,
    }

    impl Message for IncrMsg {
        fn type_tag() -> u64 {
            type_tag_hash(b"IncrMsg")
        }
        fn encode(&self) -> Vec<u8> {
            self.amount.to_le_bytes().to_vec()
        }
        fn decode(bytes: &[u8]) -> Result<Self, String> {
            if bytes.len() < 4 {
                return Err("too short".into());
            }
            Ok(Self {
                amount: u32::from_le_bytes(bytes[..4].try_into().unwrap()),
            })
        }
    }

    struct CounterReply {
        value: u32,
    }

    impl Message for CounterReply {
        fn type_tag() -> u64 {
            type_tag_hash(b"CounterReply")
        }
        fn encode(&self) -> Vec<u8> {
            self.value.to_le_bytes().to_vec()
        }
        fn decode(bytes: &[u8]) -> Result<Self, String> {
            if bytes.len() < 4 {
                return Err("too short".into());
            }
            Ok(Self {
                value: u32::from_le_bytes(bytes[..4].try_into().unwrap()),
            })
        }
    }

    #[test]
    fn test_typed_handler_dispatch() {
        let call_count = Arc::new(AtomicU32::new(0));
        let cc = call_count.clone();

        let handler = TypedHandler::<IncrMsg, _>::new(move |state, msg, _reply, _local| {
            let counter = state.get_mut::<u32>().unwrap();
            *counter += msg.amount;
            cc.fetch_add(1, Ordering::Relaxed);
            ActorStatus::Continue
        });

        assert_eq!(handler.type_tag(), IncrMsg::type_tag());

        let mut state = ActorState::new();
        state.insert::<u32>(0);

        // Dispatch
        let payload = IncrMsg { amount: 5 }.encode();
        handler.handle(&mut state, &payload, None, None);
        handler.handle(&mut state, &payload, None, None);

        assert_eq!(*state.get::<u32>().unwrap(), 10);
        assert_eq!(call_count.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn test_typed_handler_with_reply() {
        let handler = TypedHandler::<IncrMsg, _>::new(|state, msg, reply, _local| {
            let counter = state.get_mut::<u32>().unwrap();
            *counter += msg.amount;
            if let Some(reply) = reply {
                reply.send(CounterReply { value: *counter });
            }
            ActorStatus::Continue
        });

        let mut state = ActorState::new();
        state.insert::<u32>(100);

        let msg = IncrMsg { amount: 7 };
        let (env, rx) = msg.into_request();
        handler.handle(&mut state, &env.payload, env.reply, env.local);

        let reply_bytes = rx.wait_blocking().unwrap();
        let reply = CounterReply::decode(&reply_bytes).unwrap();
        assert_eq!(reply.value, 107);
    }

    #[test]
    fn test_typed_handler_decode_error() {
        let handler = TypedHandler::<IncrMsg, _>::new(|_state, _msg, _reply, _local| {
            panic!("should not be called on bad payload");
        });

        let mut state = ActorState::new();
        // Bad payload — too short
        let status = handler.handle(&mut state, &[0u8, 1], None, None);
        assert_eq!(status, ActorStatus::Continue);
    }

    #[test]
    fn test_typed_handler_priority() {
        let handler = TypedHandler::<IncrMsg, _>::with_priority(
            |_s, _m, _r, _l| ActorStatus::Continue,
            Priority::Critical,
        );
        assert_eq!(handler.priority(), Priority::Critical);
    }
}
