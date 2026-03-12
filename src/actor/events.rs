use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use flume as channel;

use super::{ActorId, Priority};

/// Events émis par le scheduler.
#[derive(Debug, Clone)]
pub(crate) enum SchedulerEvent {
    MessageHandled {
        actor_id: ActorId,
        actor_name: &'static str,
        duration: Duration,
        mailbox_depth: usize,
        priority: Priority,
    },
    PriorityChanged {
        actor_id: ActorId,
        actor_name: &'static str,
        from: Priority,
        to: Priority,
    },
    ActorIdle {
        actor_id: ActorId,
        actor_name: &'static str,
    },
    ActorWoken {
        actor_id: ActorId,
        actor_name: &'static str,
        woken_by: WakeReason,
    },
    ThreadParked {
        thread_index: usize,
    },
    ThreadUnparked {
        thread_index: usize,
    },
    ActorStopped {
        actor_id: ActorId,
        actor_name: &'static str,
    },
    ActorSpawned {
        actor_id: ActorId,
        actor_name: &'static str,
        mailbox_capacity: usize,
    },
    /// Un message a été envoyé à un acteur qui n'était PAS idle.
    /// Le scheduler ne sera pas réveillé — le message attend dans la mailbox.
    MessageSentNoWake {
        actor_id: ActorId,
        actor_name: &'static str,
        mailbox_depth: usize,
    },
    /// Un message a été envoyé et l'acteur a été réveillé.
    MessageSentWithWake {
        actor_id: ActorId,
        actor_name: &'static str,
    },
    /// Le notifier n'est pas encore attaché (ActorRef avant spawn).
    MessageSentNoNotifier,
    /// handle_batch commence le traitement d'un acteur.
    BatchStarted {
        actor_id: ActorId,
        actor_name: &'static str,
        mailbox_depth: usize,
    },
}

#[derive(Debug, Clone)]
pub(crate) enum WakeReason {
    MessageReceived,
    IdleWork,
}

/// Bus d'events générique. Zero-cost quand personne n'écoute.
///
/// Utilise un vrai broadcast : chaque subscriber a son propre channel.
/// `emit()` envoie une copie à chaque subscriber.
pub(crate) struct EventBus<E> {
    subscriber_count: AtomicUsize,
    subscribers: Mutex<Vec<channel::Sender<E>>>,
}

impl<E: Clone + Send + 'static> EventBus<E> {
    pub fn new() -> Self {
        EventBus {
            subscriber_count: AtomicUsize::new(0),
            subscribers: Mutex::new(Vec::new()),
        }
    }

    #[inline]
    pub fn has_subscribers(&self) -> bool {
        self.subscriber_count.load(Ordering::Relaxed) > 0
    }

    #[inline]
    pub fn emit(&self, event: E) {
        if !self.has_subscribers() {
            return;
        }
        let subs = self.subscribers.lock().unwrap();
        for sender in subs.iter() {
            let _ = sender.send(event.clone());
        }
    }

    pub fn subscribe(self: &Arc<Self>) -> EventReceiver<E> {
        let (sender, receiver) = channel::unbounded();
        self.subscribers.lock().unwrap().push(sender);
        self.subscriber_count.fetch_add(1, Ordering::Relaxed);
        EventReceiver {
            receiver,
            bus: Arc::clone(self),
        }
    }
}

pub(crate) struct EventReceiver<E> {
    receiver: channel::Receiver<E>,
    bus: Arc<EventBus<E>>,
}

impl<E> EventReceiver<E> {
    pub fn try_recv(&self) -> Option<E> {
        self.receiver.try_recv().ok()
    }

    pub fn recv(&self) -> Option<E> {
        self.receiver.recv().ok()
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Option<E> {
        self.receiver.recv_timeout(timeout).ok()
    }
}

impl<E> Drop for EventReceiver<E> {
    fn drop(&mut self) {
        self.bus.subscriber_count.fetch_sub(1, Ordering::Relaxed);
        // Les senders morts sont des no-op silencieux dans crossbeam.
        // Pas de dummy event — le compteur atomique suffit pour has_subscribers().
    }
}

impl<E> Iterator for EventReceiver<E> {
    type Item = E;
    fn next(&mut self) -> Option<E> {
        self.receiver.recv().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_subscriber_no_alloc() {
        let bus: Arc<EventBus<SchedulerEvent>> = Arc::new(EventBus::new());
        assert!(!bus.has_subscribers());
        bus.emit(SchedulerEvent::ThreadParked { thread_index: 0 });
    }

    #[test]
    fn test_subscribe_receive() {
        let bus: Arc<EventBus<SchedulerEvent>> = Arc::new(EventBus::new());
        let rx = bus.subscribe();
        assert!(bus.has_subscribers());

        bus.emit(SchedulerEvent::ThreadParked { thread_index: 7 });
        let event = rx.try_recv().unwrap();
        assert!(matches!(
            event,
            SchedulerEvent::ThreadParked { thread_index: 7 }
        ));
    }

    #[test]
    fn test_unsubscribe_on_drop() {
        let bus: Arc<EventBus<SchedulerEvent>> = Arc::new(EventBus::new());
        {
            let _rx = bus.subscribe();
            assert!(bus.has_subscribers());
        }
        assert!(!bus.has_subscribers());
    }

    #[test]
    fn test_multiple_subscribers() {
        let bus: Arc<EventBus<SchedulerEvent>> = Arc::new(EventBus::new());
        let rx1 = bus.subscribe();
        let rx2 = bus.subscribe();
        assert_eq!(bus.subscriber_count.load(Ordering::Relaxed), 2);

        bus.emit(SchedulerEvent::ThreadParked { thread_index: 0 });
        // Both should receive the event (broadcast)
        assert!(rx1.try_recv().is_some());
        assert!(rx2.try_recv().is_some());
    }
}
