//! lucivy-fts: typed Rust ↔ C++ bridge for Lucivy full-text search.
//!
//! This crate provides a cxx bridge for creating, managing, and querying
//! Lucivy indexes. It is designed to be compiled as a static library
//! and linked into the rag3db C++ extension.
//!
//! Core functionality (handle, query, tokenizer, directory) lives in `lucivy-core`.

#[cfg(feature = "cxx-bridge")]
mod bridge;

/// Local wrapper around `lucivy_core::handle::LucivyHandle` for CXX bridge
/// compatibility (orphan rule: CXX-generated impls require a local type).
pub struct LucivyHandle(pub lucivy_core::handle::LucivyHandle);

impl std::ops::Deref for LucivyHandle {
    type Target = lucivy_core::handle::LucivyHandle;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
