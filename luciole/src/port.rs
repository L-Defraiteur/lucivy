use std::any::{Any, TypeId};
use std::fmt;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// PortType — static type tag checked at connect time
// ---------------------------------------------------------------------------

/// Describes the type of data a port expects or produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PortType {
    /// Typed data identified by `TypeId`.
    Typed(TypeId),
    /// Trigger signal (no payload).
    Trigger,
    /// Wildcard — compatible with anything.
    Any,
}

impl PortType {
    /// Convenience: build a `Typed` variant from a concrete type.
    pub fn of<T: 'static>() -> Self {
        PortType::Typed(TypeId::of::<T>())
    }

    /// Two port types are compatible when they can be connected.
    pub fn compatible_with(&self, other: &PortType) -> bool {
        match (self, other) {
            (PortType::Any, _) | (_, PortType::Any) => true,
            (PortType::Trigger, PortType::Trigger) => true,
            (PortType::Typed(a), PortType::Typed(b)) => a == b,
            _ => false,
        }
    }
}

impl fmt::Display for PortType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PortType::Typed(id) => write!(f, "Typed({:?})", id),
            PortType::Trigger => write!(f, "Trigger"),
            PortType::Any => write!(f, "Any"),
        }
    }
}

// ---------------------------------------------------------------------------
// PortValue — runtime data flowing between nodes
// ---------------------------------------------------------------------------

/// Runtime value flowing through a DAG edge.
///
/// Uses `Arc` internally so that fan-out (one output connected to multiple
/// inputs) works via cheap clones. For exclusive access, use `take()` which
/// attempts `Arc::try_unwrap` (succeeds when there is a single consumer).
#[derive(Clone)]
pub enum PortValue {
    /// Type-erased data wrapped in Arc for cheap fan-out cloning.
    Data(Arc<dyn Any + Send + Sync>),
    /// Trigger signal (no payload).
    Trigger,
}

impl PortValue {
    /// Wrap a concrete value.
    pub fn new<T: Send + Sync + 'static>(data: T) -> Self {
        PortValue::Data(Arc::new(data))
    }

    /// Borrow as concrete type.
    pub fn downcast<T: 'static>(&self) -> Option<&T> {
        match self {
            PortValue::Data(b) => b.downcast_ref(),
            _ => None,
        }
    }

    /// Consume and extract the concrete type.
    ///
    /// Panics if the type matches but there are multiple references (fan-out).
    /// This catches the bug at runtime with a clear message instead of silent None.
    /// Use `downcast()` for read-only fan-out access.
    pub fn take<T: Send + Sync + 'static>(self) -> Option<T> {
        match self {
            PortValue::Data(arc) => {
                let typed = Arc::downcast::<T>(arc).ok()?;
                match Arc::try_unwrap(typed) {
                    Ok(val) => Some(val),
                    Err(arc) => panic!(
                        "PortValue::take() failed: {} outstanding references to {}. \
                         This means the same output port is connected to multiple inputs \
                         (fan-out). Use separate output ports for data that will be taken, \
                         or use downcast() for read-only access.",
                        Arc::strong_count(&arc),
                        std::any::type_name::<T>(),
                    ),
                }
            }
            _ => None,
        }
    }

    /// True if this is a trigger signal.
    pub fn is_trigger(&self) -> bool {
        matches!(self, PortValue::Trigger)
    }

    /// True if the inner data matches type `T`.
    pub fn is<T: 'static>(&self) -> bool {
        match self {
            PortValue::Data(b) => b.is::<T>(),
            PortValue::Trigger => false,
        }
    }
}

impl fmt::Debug for PortValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PortValue::Data(_) => write!(f, "PortValue::Data(...)"),
            PortValue::Trigger => write!(f, "PortValue::Trigger"),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_type_compatibility() {
        let int = PortType::of::<i32>();
        let string = PortType::of::<String>();

        assert!(int.compatible_with(&int));
        assert!(!int.compatible_with(&string));
        assert!(int.compatible_with(&PortType::Any));
        assert!(PortType::Any.compatible_with(&string));
        assert!(PortType::Trigger.compatible_with(&PortType::Trigger));
        assert!(!PortType::Trigger.compatible_with(&int));
    }

    #[test]
    fn port_value_roundtrip() {
        let v = PortValue::new(42u64);
        assert!(v.is::<u64>());
        assert!(!v.is::<i32>());
        assert_eq!(v.downcast::<u64>(), Some(&42u64));
        assert_eq!(v.take::<u64>(), Some(42u64));
    }

    #[test]
    fn port_value_clone_fanout() {
        let v = PortValue::new(42u64);
        let v2 = v.clone();
        assert_eq!(v.downcast::<u64>(), Some(&42u64));
        assert_eq!(v2.downcast::<u64>(), Some(&42u64));
        // downcast (borrow) works with fan-out
        // take panics with fan-out — tested separately via should_panic
        drop(v);
        // succeeds on the last reference
        assert_eq!(v2.take::<u64>(), Some(42));
    }

    #[test]
    #[should_panic(expected = "outstanding references")]
    fn port_value_take_panics_on_fanout() {
        let v = PortValue::new(42u64);
        let _v2 = v.clone();
        // This should panic — fan-out detected
        let _ = v.take::<u64>();
    }

    #[test]
    fn port_value_trigger() {
        let v = PortValue::Trigger;
        assert!(v.is_trigger());
        assert!(!v.is::<u64>());
        let v2 = v.clone();
        assert!(v2.is_trigger());
    }

    #[test]
    fn port_value_wrong_type() {
        let v = PortValue::new("hello".to_string());
        assert_eq!(v.downcast::<i32>(), None);
        assert_eq!(v.take::<i32>(), None);
    }
}
