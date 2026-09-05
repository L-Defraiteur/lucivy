//! The state of a shard dictionary's background fold, shared by the
//! commit that starts it, the task that runs it, and the searches that wait
//! for it (`indexer::dictionary_commit`).
//!
//! One fold at a time per index. A commit names its new segments' pairs as
//! pending parts and, if no fold runs, starts one; the task merges every
//! pending pair into the next generation, swaps the live dictionary (in
//! RAM — `meta.json` follows at the next commit), and loops while pairs
//! remain. Searches call `wait` so that they never walk the pairs on top of
//! the generations: what a query costs never depends on when it runs.

use std::sync::{Condvar, Mutex};

#[derive(Default)]
struct Inner {
    /// A fold task is running.
    in_flight: bool,
    /// The live dictionary was swapped since `wait` last returned true.
    changed: bool,
    /// The task folded something and asked the segment updater to write
    /// `meta.json`; cleared once written.
    persist_pending: bool,
}

#[derive(Default)]
pub struct DictionaryFold {
    inner: Mutex<Inner>,
    done: Condvar,
    /// Held by whoever edits the live dictionary's meta (a commit adding
    /// pending pairs, the task swapping folded ones in), so the two never
    /// interleave a read-modify-write.
    meta_lock: Mutex<()>,
}

impl DictionaryFold {
    /// Take the meta lock (see the field).
    pub fn lock_meta(&self) -> std::sync::MutexGuard<'_, ()> {
        self.meta_lock.lock().unwrap()
    }

    /// Claim the single fold slot; false when a fold already runs.
    pub fn begin(&self) -> bool {
        let mut inner = self.inner.lock().unwrap();
        if inner.in_flight {
            return false;
        }
        inner.in_flight = true;
        true
    }

    /// The task swapped the live dictionary.
    pub fn mark_changed(&self) {
        self.inner.lock().unwrap().changed = true;
    }

    /// Release the fold slot and wake the waiters.
    pub fn finish(&self) {
        self.inner.lock().unwrap().in_flight = false;
        self.done.notify_all();
    }

    /// A fold is running.
    pub fn in_flight(&self) -> bool {
        self.inner.lock().unwrap().in_flight
    }

    /// The task folded something: `meta.json` is to be rewritten.
    pub fn set_persist_pending(&self) {
        self.inner.lock().unwrap().persist_pending = true;
    }

    /// The segment updater wrote `meta.json` after a fold.
    pub fn persisted(&self) {
        self.inner.lock().unwrap().persist_pending = false;
        self.done.notify_all();
    }

    /// Block while a fold runs or its `meta.json` is not yet written — what
    /// closing a writer waits for, so that a reopen sees the settled parts.
    /// The write is bounded (the updater may be gone): 10 s at most.
    pub fn wait_settled(&self) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut inner = self.inner.lock().unwrap();
        while inner.in_flight {
            inner = self.done.wait(inner).unwrap();
        }
        while inner.persist_pending && std::time::Instant::now() < deadline {
            inner = self.done.wait_timeout(inner, std::time::Duration::from_millis(50)).unwrap().0;
        }
    }

    /// Block while a fold runs; returns whether the live dictionary changed
    /// since the last time this returned true (the flag is consumed).
    pub fn wait(&self) -> bool {
        let mut inner = self.inner.lock().unwrap();
        while inner.in_flight {
            inner = self.done.wait(inner).unwrap();
        }
        std::mem::take(&mut inner.changed)
    }
}
