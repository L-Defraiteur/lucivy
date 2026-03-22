pub mod actor_state;
pub mod checkpoint;
pub mod dag;
pub mod envelope;
pub mod events;
pub mod generic_actor;
pub mod graph_node;
pub mod handler;
pub mod mailbox;
pub mod node;
pub mod observe;
pub mod pool;
pub mod port;
pub mod reply;
pub mod runtime;
pub mod scheduler;
pub mod scope;
pub mod branch;
pub mod scatter;
pub use branch::BranchNode;
pub use scatter::ScatterResults;
pub mod stream_dag;

pub use actor_state::ActorState;
pub use dag::{Dag, DagEdge};
pub use envelope::{ActorError, Envelope, Message, ReplyPort, TypedActorRef, reply_port, type_tag_hash};
pub use generic_actor::GenericActor;
pub use graph_node::GraphNode;
pub use handler::{Handler, TypedHandler};
pub use mailbox::{mailbox, ActorRef, Mailbox};
pub use node::{LogLevel, Node, NodeContext, NodePoll, PollNode, PollNodeAdapter, PortDef, ServiceRegistry};
pub use observe::{TapEvent, TapRegistry};
pub use pool::{DrainMsg, DrainableRef, Pool, ShutdownMsg};
pub use port::{PortType, PortValue};
pub use reply::{reply, Reply, ReplyReceiver};
pub use checkpoint::{CheckpointStatus, CheckpointStore, DagCheckpoint, FileCheckpointStore, MemoryCheckpointStore};
pub use runtime::{display_progress, execute_dag, execute_dag_with_checkpoint, subscribe_dag_events, DagEvent, DagResult, NodeResult};
pub use scheduler::{ActorId, Scheduler, SchedulerHandle};
pub use scope::{Drainable, Scope};
pub use stream_dag::StreamDag;

use std::task::Poll;

/// Trait principal pour un acteur dans le système.
///
/// Un acteur reçoit des messages typés via sa mailbox, les traite un par un,
/// et déclare sa priorité courante pour le scheduling.
pub trait Actor: Send + 'static {
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
pub enum ActorStatus {
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
pub enum Priority {
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
