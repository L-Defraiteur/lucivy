//! The blob storage abstraction is lucistore's: one trait for the FTS index,
//! the sparse index, and any store a host implements (rag3weaver's
//! `CypherBlobStore`).

pub use lucistore::blob_store::{BlobStore, MemBlobStore};
