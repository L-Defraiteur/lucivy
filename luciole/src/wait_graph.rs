//! WaitGraph — global dependency graph for deadlock diagnosis.
//!
//! Every inter-thread wait in luciole automatically registers an edge in the
//! global WaitGraph. This makes ALL waits visible — no manual instrumentation
//! needed by user code.
//!
//! Auto-instrumented points:
//! - `scheduler.wait()` (blocking + cooperative)
//! - `ReplyReceiver::wait_cooperative_named()`
//! - `ReplyReceiver::wait_blocking()` / `wait_blocking_with_diag()`
//! - `ActorStatus::Suspend` (tracked by scheduler dispatch loop)
//!
//! Use `dump_mermaid()` or `dump_text()` at any time to see who waits on what.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use super::scheduler::ActorId;

// ---------------------------------------------------------------------------
// Edge ID generator
// ---------------------------------------------------------------------------

static NEXT_EDGE_ID: AtomicU64 = AtomicU64::new(1);

fn next_id() -> u64 {
    NEXT_EDGE_ID.fetch_add(1, Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Who is waiting.
#[derive(Debug, Clone)]
pub enum WaiterKind {
    /// A named thread (scheduler thread or external thread).
    Thread(String),
    /// An actor (suspended or doing cooperative wait inside handler).
    Actor(ActorId, &'static str),
}

impl WaiterKind {
    /// Short label for mermaid/text output.
    fn label(&self) -> String {
        match self {
            WaiterKind::Thread(name) => name.clone(),
            WaiterKind::Actor(id, name) => format!("{name}_{}", id.0),
        }
    }
}

/// One edge in the wait graph: "waiter is blocked on <label>".
#[derive(Debug)]
struct WaitEdge {
    id: u64,
    waiter: WaiterKind,
    label: String,
    since: Instant,
}

// ---------------------------------------------------------------------------
// Global storage
// ---------------------------------------------------------------------------

static EDGES: Mutex<Vec<WaitEdge>> = Mutex::new(Vec::new());

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Determine the current waiter identity from thread-local state.
///
/// If on a scheduler thread running an actor → WaiterKind::Actor.
/// Otherwise → WaiterKind::Thread with thread name.
pub fn current_waiter() -> WaiterKind {
    if let Some(info) = super::scheduler::current_thread_info() {
        if let Some((actor_id, actor_name)) = info.current_actor() {
            return WaiterKind::Actor(actor_id, actor_name);
        }
        return WaiterKind::Thread(info.name.clone());
    }
    let name = std::thread::current()
        .name()
        .unwrap_or("unknown")
        .to_string();
    WaiterKind::Thread(name)
}

/// Register a wait edge. Returns an edge ID for later unregistration.
///
/// Called automatically by luciole's wait primitives — user code should
/// NOT call this directly.
pub fn register(waiter: WaiterKind, label: impl Into<String>) -> u64 {
    let id = next_id();
    let edge = WaitEdge {
        id,
        waiter,
        label: label.into(),
        since: Instant::now(),
    };
    EDGES.lock().unwrap().push(edge);
    id
}

/// Unregister a wait edge (wait completed).
///
/// Called automatically by luciole's wait primitives.
pub fn unregister(id: u64) {
    EDGES.lock().unwrap().retain(|e| e.id != id);
}

/// Number of active wait edges (for diagnostics).
pub fn len() -> usize {
    EDGES.lock().unwrap().len()
}

/// Dump the wait graph as a Mermaid diagram.
pub fn dump_mermaid() -> String {
    let edges = EDGES.lock().unwrap();
    if edges.is_empty() {
        return "graph LR\n    no_waits[\"no active waits\"]\n".to_string();
    }

    let mut out = String::from("graph LR\n");
    for edge in edges.iter() {
        let elapsed = edge.since.elapsed().as_secs();
        let waiter = edge.waiter.label();
        // Sanitize node IDs for mermaid (replace spaces/special chars).
        let waiter_id = sanitize_mermaid_id(&waiter);
        out.push_str(&format!(
            "    {waiter_id}[\"{waiter}\"] -->|\"{}  ({elapsed}s)\"| wait_{}\n",
            edge.label, edge.id,
        ));
    }
    out
}

/// Dump the wait graph as plain text.
pub fn dump_text() -> String {
    let edges = EDGES.lock().unwrap();
    if edges.is_empty() {
        return "WaitGraph: (empty)".to_string();
    }

    let mut lines = vec![format!("WaitGraph ({} edges):", edges.len())];
    for edge in edges.iter() {
        let elapsed = edge.since.elapsed().as_secs_f64();
        let waiter = edge.waiter.label();
        lines.push(format!("  {} --[{}]--> waiting ({:.1}s)", waiter, edge.label, elapsed));
    }
    lines.join("\n")
}

/// Sanitize a string for use as a Mermaid node ID.
fn sanitize_mermaid_id(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_alphanumeric() || c == '_' { c } else { '_' })
        .collect()
}

// ---------------------------------------------------------------------------
// RAII guard for automatic unregistration
// ---------------------------------------------------------------------------

/// RAII guard that unregisters a wait edge on drop.
///
/// Used internally by luciole's wait primitives to guarantee cleanup
/// even on panic/early return.
pub struct WaitGuard {
    edge_id: u64,
}

impl WaitGuard {
    /// Register a wait edge and return a guard that unregisters on drop.
    pub fn new(waiter: WaiterKind, label: impl Into<String>) -> Self {
        let edge_id = register(waiter, label);
        Self { edge_id }
    }

    /// Register using the auto-detected current waiter.
    pub fn current(label: impl Into<String>) -> Self {
        Self::new(current_waiter(), label)
    }
}

impl Drop for WaitGuard {
    fn drop(&mut self) {
        unregister(self.edge_id);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_register_unregister() {
        let id1 = register(WaiterKind::Thread("test-1".into()), "wait_a");
        let id2 = register(WaiterKind::Thread("test-2".into()), "wait_b");
        assert!(len() >= 2);

        unregister(id1);
        unregister(id2);
    }

    #[test]
    fn test_guard_auto_unregister() {
        let before = len();
        {
            let _g = WaitGuard::new(WaiterKind::Thread("guard-test".into()), "test_wait");
            assert_eq!(len(), before + 1);
        }
        assert_eq!(len(), before);
    }

    #[test]
    fn test_dump_mermaid_empty() {
        // Can't test empty since other tests may have edges, but format is stable.
        let mermaid = dump_mermaid();
        assert!(mermaid.starts_with("graph LR"));
    }

    #[test]
    fn test_dump_text() {
        let id = register(WaiterKind::Thread("dump-test".into()), "my_label");
        let text = dump_text();
        assert!(text.contains("dump-test"));
        assert!(text.contains("my_label"));
        unregister(id);
    }

    #[test]
    fn test_actor_waiter() {
        let id = register(
            WaiterKind::Actor(ActorId(42), "indexer"),
            "flush_wait",
        );
        let text = dump_text();
        assert!(text.contains("indexer_42"));
        assert!(text.contains("flush_wait"));
        unregister(id);
    }
}
