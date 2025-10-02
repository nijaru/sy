pub mod config;

// Re-export for convenience when SSH transport is implemented
#[allow(unused_imports)]
pub use config::{SshConfig, parse_ssh_config};
