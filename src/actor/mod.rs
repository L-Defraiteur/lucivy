pub(crate) mod events;
pub(crate) mod mailbox;
pub(crate) mod reply;
pub(crate) mod scheduler;

pub(crate) use mailbox::{mailbox, ActorRef, Mailbox};
pub(crate) use reply::{reply, Reply, ReplyReceiver};
pub(crate) use scheduler::{ActorId, Scheduler, SchedulerHandle};

use std::task::Poll;

/// Trait principal pour un acteur dans le système.
///
/// Un acteur reçoit des messages typés via sa mailbox, les traite un par un,
/// et déclare sa priorité courante pour le scheduling.
pub(crate) trait Actor: Send + 'static {
    type Msg: Send + 'static;

    /// Nom lisible pour les events et le debug.
    fn name(&self) -> &'static str;

    /// Traite un message. Retourne le status souhaité.
    fn handle(&mut self, msg: Self::Msg) -> ActorStatus;

    /// Priorité courante (recalculée par le scheduler après chaque handle).
    fn priority(&self) -> Priority;

    /// Travail interne quand la mailbox est vide.
    /// Ex: MergerActor avance son merge incrémental.
    /// Par défaut : rien à faire.
    fn poll_idle(&mut self) -> Poll<()> {
        Poll::Pending
    }

    /// Appelé une seule fois après le spawn, quand le wake_handle est attaché.
    /// Le self_ref permet à l'acteur de s'envoyer des messages (self-messages).
    fn on_start(&mut self, _self_ref: ActorRef<Self::Msg>) {}
}

/// Status retourné par `Actor::handle()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ActorStatus {
    /// Continuer à traiter les messages.
    Continue,
    /// Yield au scheduler — l'acteur a du travail mais cède pour l'équité.
    Yield,
    /// L'acteur a terminé, le retirer du scheduler.
    Stop,
}

/// Priorité de scheduling d'un acteur.
/// Plus la valeur est haute, plus l'acteur est traité en premier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Priority {
    /// WatcherActor en attente de tick.
    Idle = 0,
    /// Merges, compression — différable.
    Low = 1,
    /// Segment updater idle — pas urgent.
    Medium = 2,
    /// Indexer workers avec segment ouvert — mémoire allouée.
    High = 3,
    /// Un Reply est en attente — l'appelant bloque dessus.
    Critical = 4,
}
