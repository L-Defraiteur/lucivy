//! sparse-vector: an inverted index for sparse vectors with WAND pruning.
//!
//! Token ids are remapped to dense dimensions, each dimension holds a
//! posting list whose elements carry a suffix-maximum ceiling, and a query
//! is answered by the [`wand`] search loop: windows of record ids are
//! scored at once and the ranges that cannot reach the top-k are skipped.
//! Indexes persist as a flat mmap file plus bincode side files, through the
//! filesystem or a lucistore `BlobStore`, and shard behind luciole actors.
//!
//! The design follows the sparse index of Qdrant (dimension remapping,
//! ceilings on posting lists, batch scoring); the code is original.
//!
//! Lives in the lucivy workspace as a friend crate: it persists through
//! `lucistore` (`BlobStore`) like the FTS index, and is meant to share its
//! storage, sharding and sync machinery. rag3weaver drives it from Rust.

pub mod blob_store;
pub mod handle;
pub mod index;
pub mod mmap_index;
pub mod segments;
pub mod sharded;
pub mod wand;
