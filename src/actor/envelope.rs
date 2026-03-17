//! Envelope — universal serialized message for generic actors.
//!
//! An Envelope carries a type tag (stable hash of the message type name),
//! a serialized payload (bytes), and an optional reply port for request/response.

use std::any::Any;

use super::reply::Reply;

/// Stable hash (FNV-1a 64-bit) of a string. Cross-build, cross-platform.
pub const fn type_tag_hash(name: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    let mut i = 0;
    while i < name.len() {
        hash ^= name[i] as u64;
        hash = hash.wrapping_mul(0x100000001b3);
        i += 1;
    }
    hash
}

/// A serialized message ready for local or network transport.
pub struct Envelope {
    /// Stable hash of the message type name. Used for handler dispatch.
    pub type_tag: u64,
    /// Serialized payload (serde_json for now, postcard later).
    pub payload: Vec<u8>,
    /// Optional reply channel for request/response pattern.
    pub reply: Option<ReplyPort>,
    /// Local-only opaque data (not serialized for network transport).
    /// Used to carry non-serializable resources like `Arc<dyn Weight>`.
    /// Will be None when the message comes from the network (Phase 4).
    pub local: Option<Box<dyn Any + Send>>,
}

/// Reply channel that sends bytes back to the caller.
pub struct ReplyPort {
    inner: Reply<Result<Vec<u8>, String>>,
}

impl ReplyPort {
    /// Create a new ReplyPort from a Reply.
    pub fn new(inner: Reply<Result<Vec<u8>, String>>) -> Self {
        Self { inner }
    }

    /// Send a typed reply (serializes via Message::encode).
    pub fn send<M: Message>(self, msg: M) {
        self.inner.send(Ok(msg.encode()));
    }

    /// Send raw bytes.
    pub fn send_bytes(self, bytes: Vec<u8>) {
        self.inner.send(Ok(bytes));
    }

    /// Send an error.
    pub fn send_err(self, err: String) {
        self.inner.send(Err(err));
    }
}

/// Create a (ReplyPort, ReplyReceiver) pair for request/response.
pub fn reply_port() -> (ReplyPort, super::reply::ReplyReceiver<Result<Vec<u8>, String>>) {
    let (tx, rx) = super::reply::reply();
    (ReplyPort::new(tx), rx)
}

/// Trait for messages that can be sent via Envelope.
///
/// Provides type tag (stable hash), encode (to bytes), decode (from bytes).
/// Implement manually for now; `#[derive(Message)]` macro in Phase 2.
pub trait Message: Send + Sized + 'static {
    /// Stable type tag for dispatch. Must be unique per message type.
    fn type_tag() -> u64;

    /// Serialize to bytes.
    fn encode(&self) -> Vec<u8>;

    /// Deserialize from bytes.
    fn decode(bytes: &[u8]) -> Result<Self, String>;

    /// Wrap into an Envelope (no reply, no local data).
    fn into_envelope(self) -> Envelope {
        Envelope {
            type_tag: Self::type_tag(),
            payload: self.encode(),
            reply: None,
            local: None,
        }
    }

    /// Wrap into an Envelope with local opaque data (no reply).
    fn into_envelope_with_local(self, local: impl Any + Send) -> Envelope {
        Envelope {
            type_tag: Self::type_tag(),
            payload: self.encode(),
            reply: None,
            local: Some(Box::new(local)),
        }
    }

    /// Wrap into an Envelope with a reply port (no local data).
    fn into_request(self) -> (Envelope, super::reply::ReplyReceiver<Result<Vec<u8>, String>>) {
        let (port, rx) = reply_port();
        let envelope = Envelope {
            type_tag: Self::type_tag(),
            payload: self.encode(),
            reply: Some(port),
            local: None,
        };
        (envelope, rx)
    }

    /// Wrap into an Envelope with a reply port and local opaque data.
    fn into_request_with_local(
        self,
        local: impl Any + Send,
    ) -> (Envelope, super::reply::ReplyReceiver<Result<Vec<u8>, String>>) {
        let (port, rx) = reply_port();
        let envelope = Envelope {
            type_tag: Self::type_tag(),
            payload: self.encode(),
            reply: Some(port),
            local: Some(Box::new(local)),
        };
        (envelope, rx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_type_tag_hash_stable() {
        // Same input always gives same hash.
        let h1 = type_tag_hash(b"MyMessage");
        let h2 = type_tag_hash(b"MyMessage");
        assert_eq!(h1, h2);
    }

    #[test]
    fn test_type_tag_hash_different() {
        let h1 = type_tag_hash(b"SearchMsg");
        let h2 = type_tag_hash(b"InsertMsg");
        assert_ne!(h1, h2);
    }

    #[test]
    fn test_type_tag_hash_not_zero() {
        assert_ne!(type_tag_hash(b"anything"), 0);
    }

    // A test message type.
    struct PingMsg {
        value: u32,
    }

    impl Message for PingMsg {
        fn type_tag() -> u64 {
            type_tag_hash(b"PingMsg")
        }

        fn encode(&self) -> Vec<u8> {
            self.value.to_le_bytes().to_vec()
        }

        fn decode(bytes: &[u8]) -> Result<Self, String> {
            if bytes.len() < 4 {
                return Err("too short".into());
            }
            let value = u32::from_le_bytes(bytes[..4].try_into().unwrap());
            Ok(Self { value })
        }
    }

    struct PongMsg {
        doubled: u32,
    }

    impl Message for PongMsg {
        fn type_tag() -> u64 {
            type_tag_hash(b"PongMsg")
        }

        fn encode(&self) -> Vec<u8> {
            self.doubled.to_le_bytes().to_vec()
        }

        fn decode(bytes: &[u8]) -> Result<Self, String> {
            if bytes.len() < 4 {
                return Err("too short".into());
            }
            let doubled = u32::from_le_bytes(bytes[..4].try_into().unwrap());
            Ok(Self { doubled })
        }
    }

    #[test]
    fn test_message_roundtrip() {
        let msg = PingMsg { value: 42 };
        let bytes = msg.encode();
        let decoded = PingMsg::decode(&bytes).unwrap();
        assert_eq!(decoded.value, 42);
    }

    #[test]
    fn test_into_envelope() {
        let msg = PingMsg { value: 7 };
        let env = msg.into_envelope();
        assert_eq!(env.type_tag, PingMsg::type_tag());
        assert!(env.reply.is_none());
        let decoded = PingMsg::decode(&env.payload).unwrap();
        assert_eq!(decoded.value, 7);
    }

    #[test]
    fn test_into_request_with_reply() {
        let msg = PingMsg { value: 99 };
        let (env, rx) = msg.into_request();
        assert_eq!(env.type_tag, PingMsg::type_tag());
        assert!(env.reply.is_some());

        // Simulate handler replying
        let reply = env.reply.unwrap();
        reply.send(PongMsg { doubled: 198 });

        // Caller receives the reply
        let reply_bytes = rx.wait_blocking().unwrap();
        let pong = PongMsg::decode(&reply_bytes).unwrap();
        assert_eq!(pong.doubled, 198);
    }

    #[test]
    fn test_reply_port_error() {
        let msg = PingMsg { value: 1 };
        let (env, rx) = msg.into_request();
        let reply = env.reply.unwrap();
        reply.send_err("something broke".into());

        let result = rx.wait_blocking();
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "something broke");
    }
}
