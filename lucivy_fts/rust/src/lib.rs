//! lucivy-fts: typed Rust ↔ C++ bridge for Lucivy full-text search.
//!
//! This crate provides a cxx bridge for creating, managing, and querying
//! Lucivy indexes. It is designed to be compiled as a static library
//! and linked into the rag3db C++ extension.
//!
//! Core functionality (handle, query, tokenizer, directory) lives in `lucivy-core`.
//! Uses ShardedHandle (unified handle, even for single-shard).

#[cfg(feature = "cxx-bridge")]
mod bridge;

/// Local wrapper around `ShardedHandle` for CXX bridge compatibility
/// (orphan rule: CXX-generated impls require a local type).
pub struct LucivyHandle(pub lucivy_core::sharded_handle::ShardedHandle);

impl std::ops::Deref for LucivyHandle {
    type Target = lucivy_core::sharded_handle::ShardedHandle;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
