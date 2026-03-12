use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crossbeam_channel as channel;

use super::events::{EventBus, SchedulerEvent};
use super::scheduler::SchedulerNotifier;

/// Côté réception d'un acteur. FIFO strict.
pub(crate) struct Mailbox<M> {
    receiver: channel::Receiver<M>,
}

impl<M> Mailbox<M> {
    pub fn try_recv(&self) -> Option<M> {
        self.receiver.try_recv().ok()
    }

    pub fn has_pending(&self) -> bool {
        !self.receiver.is_empty()
    }

    pub fn len(&self) -> usize {
        self.receiver.len()
    }
}

/// Handle pour envoyer des messages à un acteur. Clonable.
pub(crate) struct ActorRef<M> {
    sender: channel::Sender<M>,
    notifier: Option<Arc<WakeHandle>>,
}

/// Partagé entre l'ActorRef et le scheduler.
/// Le scheduler remet `is_idle = true` quand l'acteur passe idle.
/// L'ActorRef le passe à `false` (swap) pour savoir s'il doit wake.
pub(crate) struct WakeHandle {
    pub(super) notifier: SchedulerNotifier,
    pub(super) is_idle: AtomicBool,
    pub(super) events: Arc<EventBus<SchedulerEvent>>,
}

// Manual Clone impl — don't require M: Clone (crossbeam Sender is Clone for any M).
impl<M> Clone for ActorRef<M> {
    fn clone(&self) -> Self {
        ActorRef {
            sender: self.sender.clone(),
            notifier: self.notifier.clone(),
        }
    }
}

impl<M> ActorRef<M> {
    pub fn send(&self, msg: M) -> Result<(), channel::SendError<M>> {
        self.sender.send(msg)?;
        if let Some(wh) = &self.notifier {
            if wh.is_idle.swap(false, Ordering::AcqRel) {
                wh.events.emit(SchedulerEvent::MessageSentWithWake {
                    actor_id: wh.notifier.actor_id(),
                    actor_name: wh.notifier.actor_name(),
                });
                wh.notifier.wake();
            } else {
                wh.events.emit(SchedulerEvent::MessageSentNoWake {
                    actor_id: wh.notifier.actor_id(),
                    actor_name: wh.notifier.actor_name(),
                    mailbox_depth: self.sender.len(),
                });
            }
        } else {
            // Pas encore de notifier — ActorRef utilisé avant spawn.
            // Normalement ne devrait pas arriver en production.
        }
        Ok(())
    }

    pub fn try_send(&self, msg: M) -> Result<(), channel::TrySendError<M>> {
        self.sender.try_send(msg)?;
        if let Some(wh) = &self.notifier {
            if wh.is_idle.swap(false, Ordering::AcqRel) {
                wh.notifier.wake();
            }
        }
        Ok(())
    }
}

/// Crée une paire (Mailbox, ActorRef) avec un channel bounded.
/// Le WakeHandle sera attaché par le scheduler au spawn.
pub(crate) fn mailbox<M>(capacity: usize) -> (Mailbox<M>, ActorRef<M>) {
    let (sender, receiver) = channel::bounded(capacity);
    (
        Mailbox { receiver },
        ActorRef {
            sender,
            notifier: None,
        },
    )
}

pub(super) fn attach_wake_handle<M>(actor_ref: &mut ActorRef<M>, handle: Arc<WakeHandle>) {
    actor_ref.notifier = Some(handle);
}
