pub(crate) mod builder;
mod collector;
pub(crate) mod gapmap;
pub(crate) mod file;
mod interceptor;
pub(crate) mod term_dictionary;
#[cfg(test)]
mod stress_tests;

pub use builder::{SuffixFstBuilder, ParentEntry};
pub use collector::SfxCollector;
pub(crate) use collector::encode_vint;
pub use gapmap::{GapMapWriter, GapMapReader};
pub use file::{SfxFileWriter, SfxFileReader, SfxPostingsReader, SfxPostingEntry};
pub use interceptor::{SfxTokenInterceptor, CapturedToken};
pub use term_dictionary::SfxTermDictionary;
