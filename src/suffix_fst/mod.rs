//! Suffix FST subsystem — substring search via suffix-indexed FST, gap maps, and postings.

/// Suffix FST builder: constructs the FST from unique tokens, encodes parent references.
pub mod builder;
/// Suffix FST builder v3: overlap-aware construction with extended encoding.
pub mod builder_v3;
/// Collector v3: overlap-aware token collection for indexation.
pub mod collector_v3;
/// Section-based binary file format — extensible container for named sections.
pub mod section_file;
/// Term texts v3: extended tokens + metadata for merge support.
pub mod termtexts_v3;
/// SFX file format v3: section-based, no sibling/gapmap.
pub mod file_v3;
mod collector;
/// GapMap: binary format for storing inter-token separators per document.
pub mod gapmap;
/// Sibling table: per-ordinal successor links for cross-token search.
pub mod sibling_table;
/// Position-to-ordinal map: (doc_id, position) → ordinal reverse index.
pub mod posmap;
/// Byte presence bitmap: 256-bit bitmap per ordinal for fast pre-filtering.
pub mod bytemap;
/// Separator byte bitmap per ordinal: which separator bytes appear after each token.
pub mod sepmap;
/// Term texts: O(1) SFX ordinal → token text (fixes ordinal mismatch with tantivy term dict).
pub mod termtexts;
/// FreqMap: doc_freq and term_freq for BM25 scoring via SFX ordinals.
pub mod freqmap;
/// SFX index file abstraction: trait + registry for per-field index files.
pub mod index_registry;
/// File I/O for `.sfx` and `.sfxpost` formats (reader/writer).
pub mod file;
mod interceptor;
pub mod sfxpost_v2;
pub(crate) mod term_dictionary;
#[cfg(test)]
mod stress_tests;

/// Builder for constructing a suffix FST from unique tokens.
pub use builder::{SuffixFstBuilder, ParentEntry};
/// Collector that captures tokens during segment writing for suffix FST construction.
pub use collector::{SfxCollector, SfxBuildOutput, SfxCollectorData};
/// Writer and reader for the GapMap binary format (inter-token separators).
pub use gapmap::{GapMapWriter, GapMapReader};
/// Writer and reader for the sibling table (cross-token successor links).
pub use sibling_table::{SiblingTableWriter, SiblingTableReader, SiblingEntry};
/// Position-to-ordinal map writer/reader.
pub use posmap::{PosMapWriter, PosMapReader};
/// Byte presence bitmap writer/reader.
pub use bytemap::{ByteBitmapWriter, ByteBitmapReader};

pub use sepmap::{SepMapWriter, SepMapReader};
/// Term texts writer/reader for SFX ordinal → token text lookup.
pub use termtexts::{TermTextsWriter, TermTextsReader};
/// FreqMap writer/reader for BM25 scoring.
pub use freqmap::{FreqMapWriter, FreqMapReader};
/// Writer and reader for the `.sfx` file format, plus per-ordinal postings reader.
pub use file::{SfxFileWriter, SfxFileReader, SfxPostingsReader, SfxPostingEntry};
/// Token interceptor that captures tokens during indexing for suffix FST construction.
pub use interceptor::{SfxTokenInterceptor, CapturedToken};
/// Term dictionary backed by the suffix FST for substring and prefix lookups.
pub use term_dictionary::SfxTermDictionary;
