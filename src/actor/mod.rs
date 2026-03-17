//! Actor module — re-exports from the `luciole` crate.
//!
//! Luciole is the standalone actor runtime. This module re-exports
//! everything so existing code (`crate::actor::*`) continues to work.

pub use luciole::*;

// Re-export sub-modules for `crate::actor::envelope::*` style imports.
pub use luciole::actor_state;
pub use luciole::envelope;
pub use luciole::events;
pub use luciole::generic_actor;
pub use luciole::handler;
pub use luciole::mailbox;
pub use luciole::reply;
pub use luciole::scheduler;
