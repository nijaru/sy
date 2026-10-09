#![allow(dead_code)]
pub mod config;
#[cfg(all(test, unix))]
mod hardlink_tests;
mod json_path;
mod local;
pub mod output;
pub mod policy;
#[cfg(feature = "ssh")]
mod pull;
mod push;
pub mod ratelimit;
pub mod scanner;
mod selected;
pub mod session;
pub mod stats;
mod verification;
#[cfg(feature = "watch")]
pub mod watch_session;

pub use config::{
    parse_delete_limit, ComparisonConfig, DeleteMode, PreserveConfig, SyncConfig,
    VerificationConfig,
};
#[allow(unused_imports)]
pub use stats::{SyncError, SyncStats};
pub use verification::{VerificationCounts, VerificationResult};
