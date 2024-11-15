mod executor;
#[doc(hidden)]
pub mod json_utils;
pub mod searcher;

use std::path::Path;

use std::sync::LazyLock;

pub use self::executor::Executor;
pub use self::searcher::{Searcher, SearcherGeneration};

pub use std::result;

/// The meta file contains all the information about the list of segments and the schema
/// of the index.
pub static META_FILEPATH: LazyLock<&'static Path> = LazyLock::new(|| Path::new("meta.json"));

/// The managed file contains a list of files that were created by the tantivy
/// and will therefore be garbage collected when they are deemed useless by tantivy.
///
/// Removing this file is safe, but will prevent the garbage collection of all of the file that
/// are currently in the directory
pub static MANAGED_FILEPATH: LazyLock<&'static Path> = LazyLock::new(|| Path::new(".managed.json"));

#[cfg(test)]
mod tests;
