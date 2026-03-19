use std::sync::{Arc, Condvar, Mutex};

/// État interne partagé du oneshot.
struct Inner<T> {
    state: Mutex<State<T>>,
    ready: Condvar,
}

struct State<T> {
    value: Option<T>,
    closed: bool,
}

/// Côté acteur : envoie la réponse (oneshot).
pub struct Reply<T> {
    inner: Arc<Inner<T>>,
}

/// Côté appelant : attend la réponse.
pub struct ReplyReceiver<T> {
    inner: Arc<Inner<T>>,
}

impl<T> Reply<T> {
    /// Envoie la réponse. Consomme le Reply.
    pub fn send(self, value: T) {
        let mut state = self.inner.state.lock().unwrap();
        state.value = Some(value);
        state.closed = true;
        self.inner.ready.notify_one();
    }
}

impl<T> Drop for Reply<T> {
    fn drop(&mut self) {
        let mut state = self.inner.state.lock().unwrap();
        state.closed = true;
        self.inner.ready.notify_one();
    }
}

impl<T> ReplyReceiver<T> {
    /// Attente bloquante (mode multi-thread).
    /// Utilise Mutex + Condvar — compatible ASYNCIFY en WASM.
    pub fn wait_blocking(self) -> T {
        let mut state = self.inner.state.lock().unwrap();
        loop {
            if let Some(value) = state.value.take() {
                return value;
            }
            if state.closed {
                panic!("actor died without replying");
            }
            state = self.inner.ready.wait(state).unwrap();
        }
    }

    /// Attente non-bloquante. Retourne None si pas encore de réponse.
    pub fn try_recv(&self) -> Option<T> {
        let mut state = self.inner.state.lock().unwrap();
        state.value.take()
    }

    /// Attente coopérative sans label (backward compat).
    pub fn wait_cooperative<F>(self, run_step: F) -> T
    where
        F: FnMut() -> bool,
    {
        self.wait_cooperative_named("(unnamed)", run_step)
    }

    /// Attente coopérative avec label pour diagnostics.
    ///
    /// Pompe le scheduler entre chaque tentative. Émet un warning si le wait
    /// dépasse le seuil (LUCIVY_WAIT_WARN_SECS, défaut 10s).
    ///
    /// `run_step` retourne `true` si du travail a été effectué.
    pub fn wait_cooperative_named<F>(self, label: &str, mut run_step: F) -> T
    where
        F: FnMut() -> bool,
    {
        use std::time::{Duration, Instant};
        use std::sync::atomic::{AtomicU64, Ordering};

        static WARN_SECS: AtomicU64 = AtomicU64::new(u64::MAX);

        let threshold_secs = {
            let v = WARN_SECS.load(Ordering::Relaxed);
            if v == u64::MAX {
                let secs = std::env::var("LUCIVY_WAIT_WARN_SECS")
                    .ok().and_then(|v| v.parse().ok())
                    .unwrap_or(10u64);
                WARN_SECS.store(secs, Ordering::Relaxed);
                secs
            } else {
                v
            }
        };
        let warn_threshold = Duration::from_secs(threshold_secs);

        let start = Instant::now();
        let mut warn_count = 0u32;

        loop {
            {
                let mut state = self.inner.state.lock().unwrap();
                if let Some(value) = state.value.take() {
                    if warn_count > 0 {
                        eprintln!("[luciole] {:?} resolved after {:.1}s",
                            label, start.elapsed().as_secs_f64());
                    }
                    return value;
                }
                if state.closed {
                    panic!("[luciole] actor died without replying (wait {:?}, {:.1}s)",
                        label, start.elapsed().as_secs_f64());
                }
            }

            // Periodic warning with scheduler state dump
            let elapsed = start.elapsed();
            if elapsed >= warn_threshold * (warn_count + 1) {
                warn_count += 1;
                let dump = crate::scheduler::global_scheduler().dump_state();
                eprintln!("[luciole] WARNING: {:?} waiting {:.1}s (warn #{})\n{}",
                    label, elapsed.as_secs_f64(), warn_count, dump);
            }

            if !run_step() {
                let mut state = self.inner.state.lock().unwrap();
                if let Some(value) = state.value.take() {
                    if warn_count > 0 {
                        eprintln!("[luciole] {:?} resolved after {:.1}s",
                            label, start.elapsed().as_secs_f64());
                    }
                    return value;
                }
                if state.closed {
                    panic!("[luciole] actor died without replying (wait {:?}, {:.1}s)",
                        label, start.elapsed().as_secs_f64());
                }
                let (mut state, _) = self
                    .inner
                    .ready
                    .wait_timeout(state, Duration::from_millis(1))
                    .unwrap();
                if let Some(value) = state.value.take() {
                    if warn_count > 0 {
                        eprintln!("[luciole] {:?} resolved after {:.1}s",
                            label, start.elapsed().as_secs_f64());
                    }
                    return value;
                }
            }
        }
    }
}

/// Crée une paire (Reply, ReplyReceiver).
pub fn reply<T>() -> (Reply<T>, ReplyReceiver<T>) {
    let inner = Arc::new(Inner {
        state: Mutex::new(State {
            value: None,
            closed: false,
        }),
        ready: Condvar::new(),
    });
    (
        Reply {
            inner: inner.clone(),
        },
        ReplyReceiver { inner },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reply_send_recv() {
        let (tx, rx) = reply();
        tx.send(42u32);
        assert_eq!(rx.wait_blocking(), 42);
    }

    #[test]
    fn test_reply_try_recv_empty() {
        let (_tx, rx) = reply::<u32>();
        assert!(rx.try_recv().is_none());
    }

    #[test]
    fn test_reply_try_recv_after_send() {
        let (tx, rx) = reply();
        tx.send("hello");
        assert_eq!(rx.try_recv(), Some("hello"));
    }

    #[test]
    fn test_reply_cooperative() {
        let (tx, rx) = reply();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(10));
            tx.send(99);
        });
        let val = rx.wait_cooperative(|| false);
        assert_eq!(val, 99);
    }

    #[test]
    #[should_panic(expected = "actor died without replying")]
    fn test_reply_dropped_sender_panics() {
        let (tx, rx) = reply::<u32>();
        drop(tx);
        rx.wait_blocking();
    }
}
