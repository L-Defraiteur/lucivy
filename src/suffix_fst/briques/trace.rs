//! QueryTrace — structured debug trace for query-time briques.
//!
//! Thread-local store, zero overhead when inactive (trace_id = None).
//! Usage:
//! ```ignore
//! qtrace!(ctx, "find_literal_v3", "query" => query, "strict" => strict);
//! qtrace!(ctx, "splits", "count" => splits.len());
//! ```

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;

/// A single trace event.
#[derive(Debug, Clone)]
pub struct TraceEvent {
    pub label: String,
    pub data: Vec<(String, String)>,
    pub depth: usize,
}

/// Trace for one query execution.
#[derive(Debug, Default)]
pub struct QueryTrace {
    pub events: Vec<TraceEvent>,
    pub depth: usize,
}

impl QueryTrace {
    pub fn push(&mut self, label: &str, data: &[(&str, &dyn fmt::Display)]) {
        self.events.push(TraceEvent {
            label: label.to_string(),
            data: data.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            depth: self.depth,
        });
    }

    pub fn enter(&mut self, label: &str) {
        self.events.push(TraceEvent {
            label: format!("→ {label}"),
            data: Vec::new(),
            depth: self.depth,
        });
        self.depth += 1;
    }

    pub fn exit(&mut self) {
        if self.depth > 0 { self.depth -= 1; }
    }

    /// Dump as indented text.
    pub fn dump(&self) -> String {
        let mut out = String::new();
        for ev in &self.events {
            let indent = "  ".repeat(ev.depth);
            out.push_str(&indent);
            out.push_str(&ev.label);
            for (k, v) in &ev.data {
                out.push_str(&format!(" {k}={v}"));
            }
            out.push('\n');
        }
        out
    }
}

thread_local! {
    static TRACES: RefCell<HashMap<u64, QueryTrace>> = RefCell::new(HashMap::new());
    static NEXT_ID: RefCell<u64> = RefCell::new(1);
}

/// Allocate a new trace_id and create an empty trace.
pub fn trace_begin() -> u64 {
    NEXT_ID.with(|id| {
        let mut id = id.borrow_mut();
        let tid = *id;
        *id += 1;
        TRACES.with(|t| t.borrow_mut().insert(tid, QueryTrace::default()));
        tid
    })
}

/// Get the finished trace and remove it from the store.
pub fn trace_finish(trace_id: u64) -> Option<QueryTrace> {
    TRACES.with(|t| t.borrow_mut().remove(&trace_id))
}

/// Push an event to a trace.
pub fn trace_event(trace_id: u64, label: &str, data: &[(&str, &dyn fmt::Display)]) {
    TRACES.with(|t| {
        if let Some(trace) = t.borrow_mut().get_mut(&trace_id) {
            trace.push(label, data);
        }
    });
}

/// Enter a sub-section (increases depth).
pub fn trace_enter(trace_id: u64, label: &str) {
    TRACES.with(|t| {
        if let Some(trace) = t.borrow_mut().get_mut(&trace_id) {
            trace.enter(label);
        }
    });
}

/// Exit a sub-section (decreases depth).
pub fn trace_exit(trace_id: u64) {
    TRACES.with(|t| {
        if let Some(trace) = t.borrow_mut().get_mut(&trace_id) {
            trace.exit();
        }
    });
}

/// Convenience macro — no-op when ctx.trace_id is None.
#[macro_export]
macro_rules! qtrace {
    ($ctx:expr, $label:expr $(, $key:expr => $val:expr)*) => {
        if let Some(tid) = $ctx.trace_id {
            $crate::suffix_fst::briques::trace::trace_event(
                tid, $label, &[$(($key, &$val as &dyn std::fmt::Display)),*]
            );
        }
    };
}

/// Convenience macro for entering a sub-section.
#[macro_export]
macro_rules! qtrace_enter {
    ($ctx:expr, $label:expr) => {
        if let Some(tid) = $ctx.trace_id {
            $crate::suffix_fst::briques::trace::trace_enter(tid, $label);
        }
    };
}

/// Convenience macro for exiting a sub-section.
#[macro_export]
macro_rules! qtrace_exit {
    ($ctx:expr) => {
        if let Some(tid) = $ctx.trace_id {
            $crate::suffix_fst::briques::trace::trace_exit(tid);
        }
    };
}
