use std::path::PathBuf;

/// Describes a file belonging to a lucivy segment.
///
/// Each segment is composed of multiple files. Some are fixed
/// (one per segment: postings, store, etc.) and some are per-field
/// (one per indexed field: suffix FST, suffix postings).
///
/// Pattern: `{segment_uuid}.{extension}` for fixed components,
///          `{segment_uuid}.{field_id}.{extension}` for per-field components.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub enum SegmentComponent {
    /// Postings (inverted lists). Sorted doc_id lists per term.
    Postings,
    /// Term positions within each document.
    Positions,
    /// Column-oriented random-access storage (fast fields).
    FastFields,
    /// Field norms: sum of term count per field per document.
    FieldNorms,
    /// Term dictionary: maps terms to posting list addresses.
    Terms,
    /// Row-oriented compressed document store.
    Store,
    /// Temporary document store (before streamed to Store).
    TempStore,
    /// Alive bitset (which documents are not deleted).
    Delete,
    /// Byte offsets for each term occurrence (for highlights).
    Offsets,
    /// Suffix FST for a specific field (contains search).
    SuffixFst {
        /// Schema field ID this suffix FST belongs to.
        field_id: u32,
    },
    /// Suffix postings for a specific field (doc_id + offsets per term).
    SuffixPost {
        /// Schema field ID this suffix postings file belongs to.
        field_id: u32,
    },
    /// Position-to-ordinal map for a specific field (reverse posting index).
    PosMap {
        /// Schema field ID.
        field_id: u32,
    },
    /// Byte presence bitmap for a specific field (256-bit per ordinal).
    ByteMap {
        /// Schema field ID.
        field_id: u32,
    },
    /// Custom per-field SFX index file (from the registry).
    CustomSfxIndex {
        field_id: u32,
        extension: String,
    },
}

impl SegmentComponent {
    /// File extension for this component.
    pub fn extension(&self) -> String {
        match self {
            SegmentComponent::Postings => "idx".to_string(),
            SegmentComponent::Positions => "pos".to_string(),
            SegmentComponent::FastFields => "fast".to_string(),
            SegmentComponent::FieldNorms => "fieldnorm".to_string(),
            SegmentComponent::Terms => "term".to_string(),
            SegmentComponent::Store => "store".to_string(),
            SegmentComponent::TempStore => "temp_store".to_string(),
            SegmentComponent::Delete => "del".to_string(),
            SegmentComponent::Offsets => "offsets".to_string(),
            SegmentComponent::SuffixFst { field_id } => format!("{}.sfx", field_id),
            SegmentComponent::SuffixPost { field_id } => format!("{}.sfxpost", field_id),
            SegmentComponent::PosMap { field_id } => format!("{}.posmap", field_id),
            SegmentComponent::ByteMap { field_id } => format!("{}.bytemap", field_id),
            SegmentComponent::CustomSfxIndex { field_id, extension } => format!("{}.{}", field_id, extension),
        }
    }

    /// Build the relative file path for this component in a segment.
    pub fn file_path(&self, segment_uuid: &str) -> PathBuf {
        PathBuf::from(format!("{}.{}", segment_uuid, self.extension()))
    }

    /// The fixed components that every segment has (one file each).
    pub fn fixed_components() -> &'static [SegmentComponent] {
        static FIXED: [SegmentComponent; 9] = [
            SegmentComponent::Postings,
            SegmentComponent::Positions,
            SegmentComponent::FastFields,
            SegmentComponent::FieldNorms,
            SegmentComponent::Terms,
            SegmentComponent::Store,
            SegmentComponent::TempStore,
            SegmentComponent::Delete,
            SegmentComponent::Offsets,
        ];
        &FIXED
    }

    /// List ALL components for a segment, including per-field SFX.
    /// Uses the SFX index registry so new index types are automatically protected from GC.
    pub fn all_components(sfx_field_ids: &[u32]) -> Vec<SegmentComponent> {
        let mut components: Vec<SegmentComponent> = Self::fixed_components().to_vec();
        for &fid in sfx_field_ids {
            components.push(SegmentComponent::SuffixFst { field_id: fid });
            // All registry index files (sfxpost, posmap, bytemap, termtexts, ...)
            for index in crate::suffix_fst::index_registry::all_indexes() {
                components.push(SegmentComponent::CustomSfxIndex {
                    field_id: fid,
                    extension: index.extension().to_string(),
                });
            }
        }
        components
    }

    /// Legacy iterator over fixed components only.
    /// Use `all_components()` when you need per-field SFX too.
    pub fn iterator() -> std::slice::Iter<'static, SegmentComponent> {
        Self::fixed_components().iter()
    }

    /// Is this a per-field component?
    pub fn is_per_field(&self) -> bool {
        matches!(self, SegmentComponent::SuffixFst { .. } | SegmentComponent::SuffixPost { .. }
            | SegmentComponent::PosMap { .. } | SegmentComponent::ByteMap { .. }
            | SegmentComponent::CustomSfxIndex { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_extensions() {
        assert_eq!(SegmentComponent::Postings.extension(), "idx");
        assert_eq!(SegmentComponent::Store.extension(), "store");
        assert_eq!(SegmentComponent::Offsets.extension(), "offsets");
    }

    #[test]
    fn per_field_extensions() {
        assert_eq!(SegmentComponent::SuffixFst { field_id: 1 }.extension(), "1.sfx");
        assert_eq!(SegmentComponent::SuffixPost { field_id: 2 }.extension(), "2.sfxpost");
    }

    #[test]
    fn file_paths() {
        let uuid = "abc123";
        assert_eq!(
            SegmentComponent::Postings.file_path(uuid),
            PathBuf::from("abc123.idx")
        );
        assert_eq!(
            SegmentComponent::SuffixFst { field_id: 1 }.file_path(uuid),
            PathBuf::from("abc123.1.sfx")
        );
        assert_eq!(
            SegmentComponent::SuffixPost { field_id: 3 }.file_path(uuid),
            PathBuf::from("abc123.3.sfxpost")
        );
    }

    #[test]
    fn all_components_includes_sfx() {
        let all = SegmentComponent::all_components(&[1, 2]);
        assert!(all.contains(&SegmentComponent::Postings)); // fixed
        assert!(all.contains(&SegmentComponent::SuffixFst { field_id: 1 }));
        assert!(all.contains(&SegmentComponent::SuffixFst { field_id: 2 }));
        // Registry indexes (sfxpost, posmap, bytemap) are now CustomSfxIndex
        let has_sfxpost_1 = all.iter().any(|c| matches!(c,
            SegmentComponent::CustomSfxIndex { field_id: 1, extension } if extension == "sfxpost"));
        assert!(has_sfxpost_1, "should have sfxpost for field 1 via registry");
        let num_registry = crate::suffix_fst::index_registry::all_indexes().len();
        // 9 fixed + 2×(sfx + N registry indexes)
        assert_eq!(all.len(), 9 + 2 * (1 + num_registry));
    }

    #[test]
    fn legacy_iterator_works() {
        let count = SegmentComponent::iterator().count();
        assert_eq!(count, 9); // fixed only
    }
}
