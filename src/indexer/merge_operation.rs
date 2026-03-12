use crate::index::SegmentId;
use crate::Opstamp;

/// Décrit une opération de merge.
///
/// Contient les informations nécessaires pour exécuter un merge :
/// - `target_opstamp` : opstamp jusqu'auquel consommer la delete queue.
/// - `segment_ids` : segments à merger.
///
/// Le tracking des segments en merge est géré par le SegmentUpdaterActor
/// (via un HashSet<SegmentId> interne), pas par cette struct.
pub struct MergeOperation {
    target_opstamp: Opstamp,
    segment_ids: Vec<SegmentId>,
}

impl MergeOperation {
    pub(crate) fn new(
        target_opstamp: Opstamp,
        segment_ids: Vec<SegmentId>,
    ) -> MergeOperation {
        MergeOperation {
            target_opstamp,
            segment_ids,
        }
    }

    pub fn target_opstamp(&self) -> Opstamp {
        self.target_opstamp
    }

    pub fn segment_ids(&self) -> &[SegmentId] {
        &self.segment_ids[..]
    }
}
