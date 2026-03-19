use std::sync::Arc;

use crate::dag::DagEdge;
use crate::events::{EventBus, EventReceiver};
use crate::port::PortValue;

// ---------------------------------------------------------------------------
// TapEvent — data flowing through a tapped edge
// ---------------------------------------------------------------------------

/// Captured data from a tapped edge during DAG execution.
#[derive(Debug, Clone)]
pub struct TapEvent {
    pub from_node: String,
    pub from_port: String,
    pub to_node: String,
    pub to_port: String,
    pub value: PortValue,
}

// ---------------------------------------------------------------------------
// TapRegistry — zero-cost edge tapping
// ---------------------------------------------------------------------------

/// Registry of edge taps. Zero-cost when no taps are active.
///
/// Taps capture the data flowing through specific edges during DAG execution.
/// When no taps are registered, `check_and_emit` is a simple boolean check.
pub struct TapRegistry {
    specs: Vec<TapSpec>,
    all: bool,
    bus: Arc<EventBus<TapEvent>>,
}

/// Identifies a specific edge to tap.
#[derive(Debug, Clone)]
pub struct TapSpec {
    pub from_node: String,
    pub from_port: String,
    pub to_node: String,
    pub to_port: String,
}

impl TapRegistry {
    pub fn new() -> Self {
        Self {
            specs: Vec::new(),
            all: false,
            bus: Arc::new(EventBus::new()),
        }
    }

    /// Tap a specific edge between two ports.
    pub fn tap(
        &mut self,
        from_node: &str,
        from_port: &str,
        to_node: &str,
        to_port: &str,
    ) -> EventReceiver<TapEvent> {
        self.specs.push(TapSpec {
            from_node: from_node.to_string(),
            from_port: from_port.to_string(),
            to_node: to_node.to_string(),
            to_port: to_port.to_string(),
        });
        self.bus.subscribe()
    }

    /// Tap all edges.
    pub fn tap_all(&mut self) -> EventReceiver<TapEvent> {
        self.all = true;
        self.bus.subscribe()
    }

    /// Subscribe to tap events without adding a new spec.
    pub fn subscribe(&self) -> EventReceiver<TapEvent> {
        self.bus.subscribe()
    }

    /// Returns true if any taps are active.
    #[inline]
    pub fn is_active(&self) -> bool {
        self.all || !self.specs.is_empty()
    }

    /// Check if an edge matches any tap spec, and emit if so.
    /// Zero-cost when no taps are active.
    #[inline]
    pub fn check_and_emit(&self, edge: &DagEdge, value: &PortValue) {
        if !self.is_active() {
            return;
        }
        if !self.bus.has_subscribers() {
            return;
        }

        let matches = self.all || self.specs.iter().any(|spec| {
            spec.from_node == edge.from_node
                && spec.from_port == edge.from_port
                && spec.to_node == edge.to_node
                && spec.to_port == edge.to_port
        });

        if matches {
            self.bus.emit(TapEvent {
                from_node: edge.from_node.clone(),
                from_port: edge.from_port.clone(),
                to_node: edge.to_node.clone(),
                to_port: edge.to_port.clone(),
                value: value.clone(), // Arc clone, cheap
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::port::PortValue;

    fn test_edge(from: &str, from_port: &str, to: &str, to_port: &str) -> DagEdge {
        DagEdge {
            from_node: from.to_string(),
            from_port: from_port.to_string(),
            to_node: to.to_string(),
            to_port: to_port.to_string(),
        }
    }

    #[test]
    fn zero_cost_when_inactive() {
        let reg = TapRegistry::new();
        assert!(!reg.is_active());
        // check_and_emit should be no-op
        let edge = test_edge("a", "out", "b", "in");
        reg.check_and_emit(&edge, &PortValue::new(42i32));
    }

    #[test]
    fn tap_specific_edge() {
        let mut reg = TapRegistry::new();
        let rx = reg.tap("a", "out", "b", "in");
        assert!(reg.is_active());

        let edge_match = test_edge("a", "out", "b", "in");
        let edge_miss = test_edge("a", "out", "c", "in");

        reg.check_and_emit(&edge_match, &PortValue::new(42i32));
        reg.check_and_emit(&edge_miss, &PortValue::new(99i32));

        // Only the matching edge should produce an event
        let evt = rx.try_recv().unwrap();
        assert_eq!(evt.from_node, "a");
        assert_eq!(evt.to_node, "b");
        assert_eq!(*evt.value.downcast::<i32>().unwrap(), 42);

        assert!(rx.try_recv().is_none());
    }

    #[test]
    fn tap_all_edges() {
        let mut reg = TapRegistry::new();
        let rx = reg.tap_all();

        let e1 = test_edge("a", "out", "b", "in");
        let e2 = test_edge("x", "data", "y", "input");

        reg.check_and_emit(&e1, &PortValue::new(1i32));
        reg.check_and_emit(&e2, &PortValue::new(2i32));

        assert!(rx.try_recv().is_some());
        assert!(rx.try_recv().is_some());
        assert!(rx.try_recv().is_none());
    }

    #[test]
    fn no_subscriber_no_emit() {
        let mut reg = TapRegistry::new();
        reg.specs.push(TapSpec {
            from_node: "a".into(),
            from_port: "out".into(),
            to_node: "b".into(),
            to_port: "in".into(),
        });
        // Active but no subscriber — should not panic
        let edge = test_edge("a", "out", "b", "in");
        reg.check_and_emit(&edge, &PortValue::new(42i32));
    }
}
