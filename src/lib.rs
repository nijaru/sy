extern crate self as sy;

// Public API — these are the stable library exports
pub mod cli;
pub mod compress;
pub mod error;
pub mod integrity;
pub mod sparse;
pub mod sync;
pub mod temp_file;

// Internal modules — public for binary/test access, not part of stable API
#[doc(hidden)]
pub mod config;
#[doc(hidden)]
pub mod endpoint;
#[doc(hidden)]
pub mod engine;
#[doc(hidden)]
pub mod filter;
pub mod fs_util;
#[doc(hidden)]
pub mod hooks;
pub mod path;
#[doc(hidden)]
pub mod protocol;
#[doc(hidden)]
pub mod remote;
#[doc(hidden)]
pub mod resource;
#[doc(hidden)]
pub mod rooted_fs;
#[cfg(feature = "ssh")]
#[doc(hidden)]
pub mod ssh;
#[doc(hidden)]
pub mod transfer;
