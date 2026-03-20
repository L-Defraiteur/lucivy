//! Diagnostic event bus — subscribe to internal events for debugging.
//!
//! Zero overhead when no subscribers (atomic bool check).
//!
//! ```ignore
//! let rx = diag_bus().subscribe(DiagFilter::All);
//! // ... run operations ...
//! while let Some(event) = rx.try_recv() {
//!     eprintln!("{:?}", event);
//! }
//! ```

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// Diagnostic event emitted by lucivy internals.
#[derive(Debug, Clone)]
pub enum DiagEvent {
    /// Token captured by SfxTokenInterceptor during indexing.
    TokenCaptured {
        doc_id: u32,
        field_id: u32,
        token: String,
        offset_from: usize,
        offset_to: usize,
    },
    /// Suffix added to the FST during build.
    SuffixAdded {
        token: String,
        ordinal: u64,
        suffix: String,
        si: u16,
    },
    /// SFX prefix_walk result for a search query.
    SfxWalk {
        query: String,
        segment_id: String,
        si0_entries: usize,
        si_rest_entries: usize,
        total_parents: usize,
    },
    /// SFX ordinal resolved to doc_ids via sfxpost.
    SfxResolve {
        query: String,
        segment_id: String,
        ordinal: u32,
        token: String,
        si: u16,
        doc_count: usize,
    },
    /// Contains search matched a doc in a segment.
    SearchMatch {
        query: String,
        segment_id: String,
        doc_id: u32,
        byte_from: usize,
        byte_to: usize,
        cross_token: bool,
    },
    /// Contains search completed for a segment.
    SearchComplete {
        query: String,
        segment_id: String,
        total_docs: u32,
    },
    /// Merge sfxpost: doc remapped.
    MergeDocRemapped {
        field_id: u32,
        token: String,
        old_doc_id: u32,
        new_doc_id: u32,
    },
}

// ---------------------------------------------------------------------------
// Filter
// ---------------------------------------------------------------------------

/// Filter for diagnostic subscriptions.
#[derive(Debug, Clone)]
pub enum DiagFilter {
    /// All events.
    All,
    /// Only tokenization events.
    Tokenization,
    /// Only SFX events (build + search).
    Sfx,
    /// Only SFX events for a specific term.
    SfxTerm(String),
    /// Only merge events.
    Merge,
}

impl DiagFilter {
    fn matches(&self, event: &DiagEvent) -> bool {
        match self {
            DiagFilter::All => true,
            DiagFilter::Tokenization => matches!(event, DiagEvent::TokenCaptured { .. }),
            DiagFilter::Sfx => matches!(event,
                DiagEvent::SuffixAdded { .. } |
                DiagEvent::SfxWalk { .. } |
                DiagEvent::SfxResolve { .. }
            ),
            DiagFilter::SfxTerm(term) => match event {
                DiagEvent::SfxWalk { query, .. } => query == term,
                DiagEvent::SfxResolve { query, .. } => query == term,
                DiagEvent::SuffixAdded { token, .. } => token.contains(term.as_str()),
                DiagEvent::TokenCaptured { token, .. } => token.contains(term.as_str()),
                DiagEvent::SearchMatch { query, .. } => query == term,
                DiagEvent::SearchComplete { query, .. } => query == term,
                _ => false,
            },
            DiagFilter::Merge => matches!(event, DiagEvent::MergeDocRemapped { .. }),
        }
    }
}

// ---------------------------------------------------------------------------
// DiagBus
// ---------------------------------------------------------------------------

/// Global diagnostic event bus.
pub struct DiagBus {
    subscribers: Mutex<Vec<(DiagFilter, std::sync::mpsc::Sender<DiagEvent>)>>,
    active: AtomicBool,
}

impl DiagBus {
    fn new() -> Self {
        Self {
            subscribers: Mutex::new(Vec::new()),
            active: AtomicBool::new(false),
        }
    }

    /// Check if any subscriber is listening (fast path).
    #[inline]
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Relaxed)
    }

    /// Emit a diagnostic event. No-op if no subscribers.
    #[inline]
    pub fn emit(&self, event: DiagEvent) {
        if !self.is_active() { return; }
        let subs = self.subscribers.lock().unwrap();
        for (filter, tx) in subs.iter() {
            if filter.matches(&event) {
                let _ = tx.send(event.clone());
            }
        }
    }

    /// Subscribe to diagnostic events matching the filter.
    pub fn subscribe(&self, filter: DiagFilter) -> std::sync::mpsc::Receiver<DiagEvent> {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut subs = self.subscribers.lock().unwrap();
        subs.push((filter, tx));
        self.active.store(true, Ordering::Relaxed);
        rx
    }

    /// Unsubscribe all and deactivate.
    pub fn clear(&self) {
        let mut subs = self.subscribers.lock().unwrap();
        subs.clear();
        self.active.store(false, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------
// Global instance
// ---------------------------------------------------------------------------

static DIAG_BUS: OnceLock<DiagBus> = OnceLock::new();

/// Global verbose flag — controls eprintln DAG summaries.
static VERBOSE: AtomicBool = AtomicBool::new(true);

/// Get the global diagnostic bus.
pub fn diag_bus() -> &'static DiagBus {
    DIAG_BUS.get_or_init(DiagBus::new)
}

/// Enable/disable verbose output (DAG summaries on stderr).
/// Default: true (for backward compat).
pub fn set_verbose(enabled: bool) {
    VERBOSE.store(enabled, Ordering::Relaxed);
}

/// Check if verbose output is enabled.
#[inline]
pub fn is_verbose() -> bool {
    VERBOSE.load(Ordering::Relaxed)
}

/// Convenience macro: emit a diagnostic event only if the bus is active.
#[macro_export]
macro_rules! diag_emit {
    ($event:expr) => {
        {
            let bus = $crate::diag::diag_bus();
            if bus.is_active() {
                bus.emit($event);
            }
        }
    };
}
