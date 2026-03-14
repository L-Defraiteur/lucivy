pub(crate) mod builder;
mod collector;
pub(crate) mod gapmap;
pub(crate) mod file;
mod interceptor;

pub use builder::{SuffixFstBuilder, ParentEntry};
pub use collector::SfxCollector;
pub use gapmap::{GapMapWriter, GapMapReader};
pub use file::{SfxFileWriter, SfxFileReader};
pub use interceptor::{SfxTokenInterceptor, CapturedToken};
