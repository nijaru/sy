extern crate self as sy;

// Public API — these are the stable library exports
pub mod cli;
pub mod compress;
pub mod delta;
pub mod error;
pub mod integrity;
pub mod sparse;
pub mod sync;
pub mod temp_file;

// Internal modules — public for binary/test access, not part of stable API
#[doc(hidden)]
pub mod bisync;
#[doc(hidden)]
pub mod endpoint;
#[doc(hidden)]
pub mod engine;
#[doc(hidden)]
pub mod protocol;
#[doc(hidden)]
pub mod remote;
#[doc(hidden)]
pub mod rooted_fs;
#[doc(hidden)]
pub mod server;
#[cfg(feature = "ssh")]
#[doc(hidden)]
pub mod ssh;
#[doc(hidden)]
pub mod transport;

// Private modules
pub(crate) mod binary;
pub(crate) mod config;
pub(crate) mod filter;
pub(crate) mod fs_util;
pub(crate) mod hooks;
pub mod path;
pub(crate) mod perf;
pub(crate) mod resource;
pub(crate) mod resume;
pub mod retry;
pub(crate) mod streaming;
