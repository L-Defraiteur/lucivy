//! Suffix FST subsystem — substring search via suffix-indexed FST, gap maps, and postings.

/// Suffix FST builder: constructs the FST from unique tokens, encodes parent references.
pub mod builder;
mod collector;
/// GapMap: binary format for storing inter-token separators per document.
pub mod gapmap;
/// Sibling table: per-ordinal successor links for cross-token search.
pub mod sibling_table;
/// Position-to-ordinal map: (doc_id, position) → ordinal reverse index.
pub mod posmap;
/// Byte presence bitmap: 256-bit bitmap per ordinal for fast pre-filtering.
pub mod bytemap;
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
pub use collector::{SfxCollector, SfxBuildOutput};
pub(crate) use collector::encode_vint;
/// Writer and reader for the GapMap binary format (inter-token separators).
pub use gapmap::{GapMapWriter, GapMapReader};
/// Writer and reader for the sibling table (cross-token successor links).
pub use sibling_table::{SiblingTableWriter, SiblingTableReader, SiblingEntry};
/// Position-to-ordinal map writer/reader.
pub use posmap::{PosMapWriter, PosMapReader};
/// Byte presence bitmap writer/reader.
pub use bytemap::{ByteBitmapWriter, ByteBitmapReader};
/// Writer and reader for the `.sfx` file format, plus per-ordinal postings reader.
pub use file::{SfxFileWriter, SfxFileReader, SfxPostingsReader, SfxPostingEntry};
/// Token interceptor that captures tokens during indexing for suffix FST construction.
pub use interceptor::{SfxTokenInterceptor, CapturedToken};
/// Term dictionary backed by the suffix FST for substring and prefix lookups.
pub use term_dictionary::SfxTermDictionary;
