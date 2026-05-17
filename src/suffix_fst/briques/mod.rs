//! Unified query building blocks (briques) for SFX v3.
//!
//! - `fst_walk`: Tier 1 — FST primitives (candidates, falling walk, sep-skip, cross-token chain)
//! - `resolve`: Tier 2 — Posting resolution (single-token, chains, adjacency, doc filtering)
//! - `composite`: Tier 3 — High-level (find_literal, trigrams, DFA continuation)

pub mod fst_walk;
pub mod resolve;
pub mod composite;
pub mod orchestrator;
