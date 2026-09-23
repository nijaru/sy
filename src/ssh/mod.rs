//! SSH destination identity for the v3 adapter.
//!
//! OpenSSH owns configuration resolution: `Include`, `Match`, wildcard
//! precedence, `HostName`, `User`, `ProxyJump`, `IdentityFile`,
//! `ControlMaster`, and `Compression` all come from the user's own `ssh_config`
//! applied to the alias exactly as written. `sy` never parses or reconstructs
//! that file, so it cannot change the meaning of a configured alias.

use std::ffi::OsString;

/// One SSH destination as the user wrote it, plus explicit CLI overrides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshTarget {
    /// Host alias exactly as provided; passed to OpenSSH as one argv word.
    pub alias: OsString,
    /// Explicit `user@` from the command line; overrides config `User=` via
    /// `-l`. `None` leaves the user choice entirely to OpenSSH.
    pub user: Option<String>,
}

impl SshTarget {
    pub fn new(alias: impl Into<OsString>, user: Option<String>) -> Self {
        Self {
            alias: alias.into(),
            user,
        }
    }
}
