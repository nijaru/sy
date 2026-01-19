//! Server session - establishes connection to remote `sy --server`
//!
//! Provides raw stdin/stdout streams. Protocol handling is done by StreamingSync.

#![allow(dead_code)]

use anyhow::{Context, Result};
use std::path::Path;
use std::process::Stdio;
use tokio::process::{Child, Command};

use crate::ssh::config::SshConfig;

/// Manages connection to a remote sy --server instance
pub struct ServerSession {
    #[allow(dead_code)]
    child: Child,
    stdin: tokio::process::ChildStdin,
    stdout: tokio::process::ChildStdout,
}

impl ServerSession {
    /// Connect to remote server via SSH
    pub async fn connect_ssh(config: &SshConfig, remote_path: &Path) -> Result<Self> {
        let mut cmd = Command::new("ssh");

        cmd.arg(&config.hostname);

        if !config.user.is_empty() {
            cmd.arg("-l").arg(&config.user);
        }

        if config.port != 22 {
            cmd.arg("-p").arg(config.port.to_string());
        }

        for key in &config.identity_file {
            cmd.arg("-i").arg(key);
        }

        // Remote command: sy --server <remote_path>
        cmd.arg("sy");
        cmd.arg("--server");
        cmd.arg(remote_path);

        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::inherit());

        let mut child = cmd.spawn().context("Failed to spawn SSH process")?;

        let stdin = child.stdin.take().context("Failed to open stdin")?;
        let stdout = child.stdout.take().context("Failed to open stdout")?;

        Ok(Self {
            child,
            stdin,
            stdout,
        })
    }

    /// Connect to local server (for testing)
    pub async fn connect_local(remote_path: &Path) -> Result<Self> {
        let exe = std::env::current_exe()?;
        let mut cmd = Command::new(exe);
        cmd.arg("--server");
        cmd.arg(remote_path);

        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::inherit());

        let mut child = cmd.spawn().context("Failed to spawn sy process")?;

        let stdin = child.stdin.take().context("Failed to open stdin")?;
        let stdout = child.stdout.take().context("Failed to open stdout")?;

        Ok(Self {
            child,
            stdin,
            stdout,
        })
    }

    /// Split into stdin/stdout for protocol handling
    pub fn split(self) -> (tokio::process::ChildStdin, tokio::process::ChildStdout) {
        (self.stdin, self.stdout)
    }
}
