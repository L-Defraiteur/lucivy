use std::time::Duration;

use crate::index::SegmentId;
use crate::Opstamp;

/// Events métier émis par l'indexer.
///
/// S'abonner via `IndexWriter::subscribe_index_events()`.
/// Zero-cost quand personne n'écoute.
#[derive(Debug, Clone)]
pub enum IndexEvent {
    /// Un merge démarre (incrémental ou blocking).
    MergeStarted {
        /// Segments à merger.
        segment_ids: Vec<SegmentId>,
        /// Opstamp cible pour la delete queue.
        target_opstamp: Opstamp,
    },
    /// Un step du merge incrémental a été exécuté.
    MergeStepCompleted {
        /// Segments en cours de merge.
        segment_ids: Vec<SegmentId>,
        /// Steps complétés jusqu'ici.
        steps_completed: u32,
        /// Nombre total de steps estimé.
        steps_total: u32,
    },
    /// Un merge s'est terminé avec succès.
    MergeCompleted {
        /// Segments qui ont été mergés.
        segment_ids: Vec<SegmentId>,
        /// Durée du merge.
        duration: Duration,
        /// Nombre de documents dans le segment résultant.
        result_num_docs: u32,
    },
    /// Un merge a échoué.
    MergeFailed {
        /// Segments du merge en échec.
        segment_ids: Vec<SegmentId>,
        /// Description de l'erreur.
        error: String,
    },
    /// Un commit démarre.
    CommitStarted {
        /// Opstamp du commit.
        opstamp: Opstamp,
    },
    /// Un commit s'est terminé.
    CommitCompleted {
        /// Opstamp du commit.
        opstamp: Opstamp,
        /// Durée du commit.
        duration: Duration,
    },
}
