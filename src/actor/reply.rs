use crossbeam_channel as channel;

/// Côté acteur : envoie la réponse (oneshot).
pub(crate) struct Reply<T> {
    sender: channel::Sender<T>,
}

/// Côté appelant : attend la réponse.
pub(crate) struct ReplyReceiver<T> {
    receiver: channel::Receiver<T>,
}

impl<T> Reply<T> {
    /// Envoie la réponse. Consomme le Reply.
    pub fn send(self, value: T) {
        let _ = self.sender.send(value);
    }
}

impl<T> ReplyReceiver<T> {
    /// Attente bloquante (mode multi-thread).
    pub fn wait_blocking(self) -> T {
        self.receiver
            .recv()
            .expect("actor died without replying")
    }

    /// Attente non-bloquante. Retourne None si pas encore de réponse.
    pub fn try_recv(&self) -> Option<T> {
        self.receiver.try_recv().ok()
    }

    /// Attente coopérative (mode single-thread).
    /// Fait tourner le scheduler entre chaque tentative via `run_step`.
    pub fn wait_cooperative<F>(self, mut run_step: F) -> T
    where
        F: FnMut(),
    {
        loop {
            match self.receiver.try_recv() {
                Ok(value) => return value,
                Err(_) => run_step(),
            }
        }
    }
}

/// Crée une paire (Reply, ReplyReceiver).
/// Utilise un channel bounded(1) — une seule réponse.
pub(crate) fn reply<T>() -> (Reply<T>, ReplyReceiver<T>) {
    let (sender, receiver) = channel::bounded(1);
    (Reply { sender }, ReplyReceiver { receiver })
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
        let mut steps = 0;
        // Simulate: the reply arrives after 3 run_steps
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(10));
            tx.send(99);
        });
        let val = rx.wait_cooperative(|| {
            steps += 1;
            std::thread::sleep(std::time::Duration::from_millis(2));
        });
        assert_eq!(val, 99);
        assert!(steps > 0);
    }

    #[test]
    #[should_panic(expected = "actor died without replying")]
    fn test_reply_dropped_sender_panics() {
        let (tx, rx) = reply::<u32>();
        drop(tx);
        rx.wait_blocking();
    }
}
