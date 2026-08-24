//! sparse-vector: sparse vector inverted index with WAND pruning, batch
//! scoring and dimension remapping. Inspired by Qdrant's sparse index
//! (Apache 2.0).
//!
//! Lives in the lucivy workspace as a friend crate: it persists through
//! `lucistore` (`BlobStore`) like the FTS index, and is meant to share its
//! storage, sharding and sync machinery. The former C++ bridge (rag3db
//! extension) is gone — rag3weaver drives it from Rust.

pub mod blob_store;
pub mod handle;
pub mod index;
pub mod mmap_index;
pub mod posting_list;
pub mod posting_list_common;
pub mod scores_memory_pool;
pub mod search_context;
pub mod top_k;
