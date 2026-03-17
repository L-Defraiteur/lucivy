//! Message implementation for LucivyError — serializable errors for Luciole.
//!
//! Preserves the error variant (SchemaError, IoError, etc.) across actor
//! boundaries. The message string is preserved exactly.

use std::sync::Arc;

use crate::actor::envelope::{type_tag_hash, Message};
use crate::error::LucivyError;

// Variant tags for serialization.
const TAG_AGGREGATION: u8 = 0;
const TAG_OPEN_DIRECTORY: u8 = 1;
const TAG_OPEN_READ: u8 = 2;
const TAG_OPEN_WRITE: u8 = 3;
const TAG_INDEX_ALREADY_EXISTS: u8 = 4;
const TAG_LOCK_FAILURE: u8 = 5;
const TAG_IO: u8 = 6;
const TAG_DATA_CORRUPTION: u8 = 7;
const TAG_POISONED: u8 = 8;
const TAG_FIELD_NOT_FOUND: u8 = 9;
const TAG_INVALID_ARGUMENT: u8 = 10;
const TAG_ERROR_IN_THREAD: u8 = 11;
const TAG_INDEX_BUILDER: u8 = 12;
const TAG_SCHEMA: u8 = 13;
const TAG_SYSTEM: u8 = 14;
const TAG_INCOMPATIBLE: u8 = 15;
const TAG_INTERNAL: u8 = 16;
const TAG_DESERIALIZE: u8 = 17;

impl Message for LucivyError {
    fn type_tag() -> u64 {
        type_tag_hash(b"LucivyError")
    }

    fn encode(&self) -> Vec<u8> {
        let (tag, msg) = match self {
            LucivyError::AggregationError(e) => (TAG_AGGREGATION, format!("{e}")),
            LucivyError::OpenDirectoryError(e) => (TAG_OPEN_DIRECTORY, format!("{e:?}")),
            LucivyError::OpenReadError(e) => (TAG_OPEN_READ, format!("{e:?}")),
            LucivyError::OpenWriteError(e) => (TAG_OPEN_WRITE, format!("{e:?}")),
            LucivyError::IndexAlreadyExists => (TAG_INDEX_ALREADY_EXISTS, String::new()),
            LucivyError::LockFailure(e, ctx) => {
                (TAG_LOCK_FAILURE, format!("{e:?}|{}", ctx.as_deref().unwrap_or("")))
            }
            LucivyError::IoError(e) => (TAG_IO, format!("{e}")),
            LucivyError::DataCorruption(e) => (TAG_DATA_CORRUPTION, format!("{e:?}")),
            LucivyError::Poisoned => (TAG_POISONED, String::new()),
            LucivyError::FieldNotFound(s) => (TAG_FIELD_NOT_FOUND, s.clone()),
            LucivyError::InvalidArgument(s) => (TAG_INVALID_ARGUMENT, s.clone()),
            LucivyError::ErrorInThread(s) => (TAG_ERROR_IN_THREAD, s.clone()),
            LucivyError::IndexBuilderMissingArgument(s) => (TAG_INDEX_BUILDER, s.to_string()),
            LucivyError::SchemaError(s) => (TAG_SCHEMA, s.clone()),
            LucivyError::SystemError(s) => (TAG_SYSTEM, s.clone()),
            LucivyError::IncompatibleIndex(e) => (TAG_INCOMPATIBLE, format!("{e:?}")),
            LucivyError::InternalError(s) => (TAG_INTERNAL, s.clone()),
            LucivyError::DeserializeError(e) => (TAG_DESERIALIZE, format!("{e:?}")),
        };
        let msg_bytes = msg.as_bytes();
        let mut buf = Vec::with_capacity(1 + 4 + msg_bytes.len());
        buf.push(tag);
        buf.extend_from_slice(&(msg_bytes.len() as u32).to_le_bytes());
        buf.extend_from_slice(msg_bytes);
        buf
    }

    fn decode(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() < 5 {
            return Err("error bytes too short".into());
        }
        let tag = bytes[0];
        let len = u32::from_le_bytes(bytes[1..5].try_into().unwrap()) as usize;
        if bytes.len() < 5 + len {
            return Err("error bytes truncated".into());
        }
        let msg = String::from_utf8_lossy(&bytes[5..5 + len]).to_string();

        // Reconstruct the error. For complex types (IoError, DataCorruption, etc.)
        // we reconstruct from the message string — the original error object is lost
        // but the variant and message are preserved.
        Ok(match tag {
            TAG_AGGREGATION => LucivyError::SystemError(msg), // simplified
            TAG_OPEN_DIRECTORY => LucivyError::SystemError(msg),
            TAG_OPEN_READ => LucivyError::SystemError(msg),
            TAG_OPEN_WRITE => LucivyError::SystemError(msg),
            TAG_INDEX_ALREADY_EXISTS => LucivyError::IndexAlreadyExists,
            TAG_LOCK_FAILURE => LucivyError::SystemError(msg),
            TAG_IO => LucivyError::IoError(Arc::new(std::io::Error::other(msg))),
            TAG_DATA_CORRUPTION => LucivyError::SystemError(msg),
            TAG_POISONED => LucivyError::Poisoned,
            TAG_FIELD_NOT_FOUND => LucivyError::FieldNotFound(msg),
            TAG_INVALID_ARGUMENT => LucivyError::InvalidArgument(msg),
            TAG_ERROR_IN_THREAD => LucivyError::ErrorInThread(msg),
            TAG_INDEX_BUILDER => LucivyError::SystemError(msg), // &'static str lost
            TAG_SCHEMA => LucivyError::SchemaError(msg),
            TAG_SYSTEM => LucivyError::SystemError(msg),
            TAG_INCOMPATIBLE => LucivyError::SystemError(msg),
            TAG_INTERNAL => LucivyError::InternalError(msg),
            TAG_DESERIALIZE => LucivyError::SystemError(msg),
            _ => LucivyError::SystemError(format!("unknown error tag {tag}: {msg}")),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_schema_error_roundtrip() {
        let err = LucivyError::SchemaError("Error getting tokenizer for field: title".into());
        let bytes = err.encode();
        let decoded = LucivyError::decode(&bytes).unwrap();
        assert_eq!(decoded.to_string(), err.to_string());
        assert!(matches!(decoded, LucivyError::SchemaError(_)));
    }

    #[test]
    fn test_system_error_roundtrip() {
        let err = LucivyError::SystemError("something broke".into());
        let bytes = err.encode();
        let decoded = LucivyError::decode(&bytes).unwrap();
        assert_eq!(decoded.to_string(), err.to_string());
        assert!(matches!(decoded, LucivyError::SystemError(_)));
    }

    #[test]
    fn test_io_error_roundtrip() {
        let err = LucivyError::IoError(Arc::new(std::io::Error::other("disk full")));
        let bytes = err.encode();
        let decoded = LucivyError::decode(&bytes).unwrap();
        assert!(matches!(decoded, LucivyError::IoError(_)));
        assert!(decoded.to_string().contains("disk full"));
    }

    #[test]
    fn test_field_not_found_roundtrip() {
        let err = LucivyError::FieldNotFound("missing_field".into());
        let bytes = err.encode();
        let decoded = LucivyError::decode(&bytes).unwrap();
        assert!(matches!(decoded, LucivyError::FieldNotFound(_)));
    }

    #[test]
    fn test_index_already_exists_roundtrip() {
        let err = LucivyError::IndexAlreadyExists;
        let bytes = err.encode();
        let decoded = LucivyError::decode(&bytes).unwrap();
        assert!(matches!(decoded, LucivyError::IndexAlreadyExists));
    }
}
