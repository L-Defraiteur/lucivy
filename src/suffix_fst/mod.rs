//! Suffix FST subsystem — substring search via suffix-indexed FST, gap maps, and postings.

/// Suffix FST builder: constructs the FST from unique tokens, encodes parent references.
pub mod builder;
mod collector;
/// GapMap: binary format for storing inter-token separators per document.
pub mod gapmap;
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
pub use collector::SfxCollector;
pub(crate) use collector::encode_vint;
/// Writer and reader for the GapMap binary format (inter-token separators).
pub use gapmap::{GapMapWriter, GapMapReader};
/// Writer and reader for the `.sfx` file format, plus per-ordinal postings reader.
pub use file::{SfxFileWriter, SfxFileReader, SfxPostingsReader, SfxPostingEntry};
/// Token interceptor that captures tokens during indexing for suffix FST construction.
pub use interceptor::{SfxTokenInterceptor, CapturedToken};
/// Term dictionary backed by the suffix FST for substring and prefix lookups.
pub use term_dictionary::SfxTermDictionary;
