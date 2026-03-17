//! ActorState — dynamic resource bag for generic actors.
//!
//! Each resource is stored by its TypeId. An actor can hold any number
//! of typed resources (Arc<LucivyHandle>, Vec<LucivyDocument>, usize, etc.).

use std::any::{Any, TypeId};
use std::collections::HashMap;

/// Dynamic state for a generic actor. Resources are typed and keyed by TypeId.
pub struct ActorState {
    resources: HashMap<TypeId, Box<dyn Any + Send>>,
}

impl ActorState {
    /// Create an empty state.
    pub fn new() -> Self {
        Self {
            resources: HashMap::new(),
        }
    }

    /// Insert a resource. Replaces any existing resource of the same type.
    pub fn insert<T: Send + 'static>(&mut self, value: T) {
        self.resources.insert(TypeId::of::<T>(), Box::new(value));
    }

    /// Get a reference to a resource by type.
    pub fn get<T: Send + 'static>(&self) -> Option<&T> {
        self.resources
            .get(&TypeId::of::<T>())
            .and_then(|b| b.downcast_ref())
    }

    /// Get a mutable reference to a resource by type.
    pub fn get_mut<T: Send + 'static>(&mut self) -> Option<&mut T> {
        self.resources
            .get_mut(&TypeId::of::<T>())
            .and_then(|b| b.downcast_mut())
    }

    /// Remove a resource by type, returning it if it existed.
    pub fn remove<T: Send + 'static>(&mut self) -> Option<T> {
        self.resources
            .remove(&TypeId::of::<T>())
            .and_then(|b| b.downcast().ok())
            .map(|b| *b)
    }

    /// Check if a resource of the given type exists.
    pub fn has<T: Send + 'static>(&self) -> bool {
        self.resources.contains_key(&TypeId::of::<T>())
    }

    /// Number of resources stored.
    pub fn len(&self) -> usize {
        self.resources.len()
    }

    /// Whether the state is empty.
    pub fn is_empty(&self) -> bool {
        self.resources.is_empty()
    }
}

impl Default for ActorState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn test_insert_and_get() {
        let mut state = ActorState::new();
        state.insert::<u64>(42);
        assert_eq!(state.get::<u64>(), Some(&42));
    }

    #[test]
    fn test_get_missing() {
        let state = ActorState::new();
        assert_eq!(state.get::<u64>(), None);
    }

    #[test]
    fn test_get_mut() {
        let mut state = ActorState::new();
        state.insert::<String>("hello".into());
        *state.get_mut::<String>().unwrap() = "world".into();
        assert_eq!(state.get::<String>().unwrap(), "world");
    }

    #[test]
    fn test_remove() {
        let mut state = ActorState::new();
        state.insert::<u32>(7);
        assert_eq!(state.remove::<u32>(), Some(7));
        assert!(!state.has::<u32>());
    }

    #[test]
    fn test_multiple_types() {
        let mut state = ActorState::new();
        state.insert::<u64>(1);
        state.insert::<String>("two".into());
        state.insert::<Vec<u8>>(vec![3]);

        assert_eq!(state.get::<u64>(), Some(&1));
        assert_eq!(state.get::<String>().unwrap(), "two");
        assert_eq!(state.get::<Vec<u8>>().unwrap(), &vec![3]);
        assert_eq!(state.len(), 3);
    }

    #[test]
    fn test_replace() {
        let mut state = ActorState::new();
        state.insert::<u64>(1);
        state.insert::<u64>(2);
        assert_eq!(state.get::<u64>(), Some(&2));
        assert_eq!(state.len(), 1);
    }

    #[test]
    fn test_arc_resource() {
        let mut state = ActorState::new();
        let shared = Arc::new(vec![1, 2, 3]);
        state.insert::<Arc<Vec<i32>>>(shared.clone());
        assert_eq!(state.get::<Arc<Vec<i32>>>().unwrap().len(), 3);
        assert_eq!(Arc::strong_count(&shared), 2);
    }

    #[test]
    fn test_has() {
        let mut state = ActorState::new();
        assert!(!state.has::<u64>());
        state.insert::<u64>(0);
        assert!(state.has::<u64>());
    }

    #[test]
    fn test_empty() {
        let state = ActorState::new();
        assert!(state.is_empty());
        assert_eq!(state.len(), 0);
    }
}
