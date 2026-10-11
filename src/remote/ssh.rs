use crate::protocol::Operation;
use crate::remote::router::RouterConfig;
use crate::remote::runtime::ClientRemoteSession;
use crate::ssh::SshTarget;
use anyhow::{Context, Result};
use std::ffi::OsString;
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;
use tokio::process::{Child, Command};

/// Owns the OpenSSH child for one v3 remote session.
///
/// Ordinary paths explicitly finish or abort and reap the child. Cancellation
/// drops this owner: router tasks are aborted and `kill_on_drop` kills OpenSSH
/// as emergency cleanup (Tokio provides best-effort reaping on that path).
pub struct SshRemoteSession {
    child: Child,
    remote: ClientRemoteSession,
}

// Completion is not a transfer timeout: even without --timeout, a peer must
// not keep an otherwise completed sync waiting indefinitely for EOF or exit.
const COMPLETION_TIMEOUT: Duration = Duration::from_secs(10);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub enum SshCompletionError {
    #[error("OpenSSH exited unsuccessfully after the protocol operation: {0}")]
    ExitStatus(ExitStatus),
    #[error("OpenSSH session completion exceeded its shutdown deadline")]
    Timeout,
    #[error("OpenSSH session cleanup exceeded its shutdown deadline")]
    CleanupTimeout,
}

/// SSH session launch options that the v3 adapter maps from CLI flags.
#[derive(Debug, Clone, Default)]
pub struct SshLaunchOptions {
    /// `--contimeout`: passed to OpenSSH as `-o ConnectTimeout=N`. `None`
    /// leaves OpenSSH's own default (the system TCP connect timeout).
    pub connect_timeout: Option<u64>,
}

impl SshRemoteSession {
    pub async fn connect(
        target: &SshTarget,
        operation: Operation,
        remote_root: &Path,
        router_config: RouterConfig,
    ) -> Result<Self> {
        Self::connect_with_options(
            target,
            operation,
            remote_root,
            router_config,
            SshLaunchOptions::default(),
        )
        .await
    }

    pub async fn connect_with_options(
        target: &SshTarget,
        operation: Operation,
        remote_root: &Path,
        router_config: RouterConfig,
        launch: SshLaunchOptions,
    ) -> Result<Self> {
        let mut command = Command::new("ssh");
        command.args(ssh_arguments(target, &launch));
        command.stdin(Stdio::piped());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::inherit());
        Self::connect_command(command, operation, remote_root, router_config).await
    }

    pub async fn connect_operand_with_options(
        target: &SshTarget,
        operation: Operation,
        path: &Path,
        operand: &crate::remote::operand::RemoteOperand,
        router_config: RouterConfig,
        launch: SshLaunchOptions,
    ) -> Result<Self> {
        let mut command = Command::new("ssh");
        command.args(ssh_arguments(target, &launch));
        command.stdin(Stdio::piped());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::inherit());
        Self::connect_command_operand(command, operation, path, Some(operand), router_config).await
    }

    async fn connect_command(
        command: Command,
        operation: Operation,
        path: &Path,
        config: RouterConfig,
    ) -> Result<Self> {
        Self::connect_command_operand(command, operation, path, None, config).await
    }

    async fn connect_command_operand(
        mut command: Command,
        operation: Operation,
        remote_root: &Path,
        operand: Option<&crate::remote::operand::RemoteOperand>,
        router_config: RouterConfig,
    ) -> Result<Self> {
        command.kill_on_drop(true);
        let mut child = command
            .spawn()
            .context("failed to spawn OpenSSH for v3 session")?;
        let connection = async {
            let writer = child.stdin.take().context("failed to open v3 SSH stdin")?;
            let reader = child
                .stdout
                .take()
                .context("failed to open v3 SSH stdout")?;
            match operand {
                Some(operand) => {
                    ClientRemoteSession::connect_operand(
                        reader,
                        writer,
                        operation,
                        remote_root,
                        operand,
                        router_config,
                    )
                    .await
                }
                None => {
                    ClientRemoteSession::connect(
                        reader,
                        writer,
                        operation,
                        remote_root,
                        router_config,
                    )
                    .await
                }
            }
            .context("v3 SSH handshake failed")
        }
        .await;
        match connection {
            Ok(remote) => Ok(Self { child, remote }),
            Err(error) => {
                // Preserve the handshake/I/O error, not the exit caused by our
                // own termination. Cancellation still drops the local Child.
                if let Err(cleanup) = terminate_child(&mut child).await {
                    tracing::warn!(%cleanup, "failed to clean up SSH handshake");
                }
                Err(error)
            }
        }
    }

    pub const fn remote(&self) -> &ClientRemoteSession {
        &self.remote
    }

    /// After all requests complete, close stdin cleanly, join transport actors,
    /// and reap OpenSSH. A valid protocol Ack does not waive its exit status.
    /// Failure here does not roll back already published destination entries.
    pub async fn finish(mut self) -> Result<()> {
        let completion = tokio::time::timeout(COMPLETION_TIMEOUT, async {
            self.remote.finish().await?;
            let status = self.child.wait().await.context("failed to reap OpenSSH")?;
            if !status.success() {
                return Err(SshCompletionError::ExitStatus(status).into());
            }
            Ok(())
        })
        .await
        .unwrap_or_else(|_| Err(SshCompletionError::Timeout.into()));
        if completion.is_err() {
            if let Err(cleanup) = self.shutdown().await {
                tracing::warn!(%cleanup, "failed to clean up SSH completion");
            }
        }
        completion
    }

    /// Terminate and reap after an operation error. The caller must retain that
    /// authoritative error rather than replace it with termination/cleanup status.
    pub async fn abort(mut self) -> Result<()> {
        self.shutdown().await
    }

    async fn shutdown(&mut self) -> Result<()> {
        // Signal the process before waiting for actors; neither a stalled peer
        // nor an actor waiting on its pipe can delay termination indefinitely.
        let kill = self.child.start_kill();
        tokio::time::timeout(CLEANUP_TIMEOUT, async {
            let transport = self.remote.shutdown().await;
            let reaped = self.child.wait().await;
            kill.context("failed to terminate OpenSSH")?;
            reaped.context("failed to reap terminated OpenSSH")?;
            transport.context("failed to join SSH transport actors")?;
            Ok(())
        })
        .await
        .unwrap_or_else(|_| Err(SshCompletionError::CleanupTimeout.into()))
    }
}

async fn terminate_child(child: &mut Child) -> Result<()> {
    tokio::time::timeout(CLEANUP_TIMEOUT, child.kill())
        .await
        .map_err(|_| SshCompletionError::CleanupTimeout)?
        .context("failed to terminate and reap OpenSSH")
}

fn ssh_arguments(target: &SshTarget, launch: &SshLaunchOptions) -> Vec<OsString> {
    let mut args = Vec::new();

    // Only explicit command-line overrides are passed; everything else is
    // resolved by OpenSSH from the user's own ssh_config against the alias.
    if let Some(user) = &target.user {
        args.push(OsString::from("-l"));
        args.push(OsString::from(user));
    }
    if let Some(connect_timeout) = launch.connect_timeout {
        args.push(OsString::from("-o"));
        args.push(OsString::from(format!("ConnectTimeout={connect_timeout}")));
    }

    // The alias is one argv word and goes through untouched: Host matching,
    // Include, Match, HostName, ProxyJump, IdentityFile, ControlMaster and
    // Compression all belong to OpenSSH. No shell is involved.
    args.push(target.alias.clone());
    args.push(OsString::from("sy"));
    args.push(OsString::from("__serve"));
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    async fn assert_process_gone(pid: u32) {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let status = Command::new("/bin/kill")
                    .args(["-0", &pid.to_string()])
                    .stderr(Stdio::null())
                    .status()
                    .await
                    .unwrap();
                if !status.success() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("owned SSH child was not killed/reaped");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn handshake_error_terminates_and_reaps_spawned_process() {
        let root = tempfile::tempdir().unwrap();
        let pid_file = root.path().join("pid");
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "printf '%s' \"$$\" > \"$PID_FILE\"; printf 'invalid handshake header................'; exec /bin/sleep 60"])
            .env("PID_FILE", &pid_file)
            .stdin(Stdio::piped()).stdout(Stdio::piped());
        let error = match SshRemoteSession::connect_command(
            command,
            Operation::Pull,
            root.path(),
            RouterConfig::default(),
        )
        .await
        {
            Ok(_) => panic!("malformed handshake must fail"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("v3 SSH handshake failed"));
        let pid = std::fs::read_to_string(pid_file).unwrap().parse().unwrap();
        assert_process_gone(pid).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn drop_and_cancelled_finish_keep_emergency_child_cleanup() {
        use crate::remote::runtime::ServerRemoteSession;
        for cancel_finish in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let (client_io, server_io) = tokio::io::duplex(64 * 1024);
            let (reader, writer) = tokio::io::split(client_io);
            let (server_reader, server_writer) = tokio::io::split(server_io);
            let (client, server) = tokio::join!(
                ClientRemoteSession::connect(
                    reader,
                    writer,
                    Operation::Pull,
                    root.path(),
                    RouterConfig::default()
                ),
                ServerRemoteSession::accept(server_reader, server_writer, RouterConfig::default()),
            );
            let mut server = server.unwrap();
            // Here the duplex holds the negotiated transport open while the
            // real sleeping subprocess exercises only emergency ownership.
            // CLI process-pipe tests cover ordinary responses and exit status.
            let child = Command::new("/bin/sleep")
                .arg("60")
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            let pid = child.id().unwrap();
            let session = SshRemoteSession {
                child,
                remote: client.unwrap(),
            };
            if cancel_finish {
                assert!(
                    tokio::time::timeout(Duration::from_millis(50), session.finish())
                        .await
                        .is_err()
                );
            } else {
                drop(session);
            }
            assert_process_gone(pid).await;
            server.shutdown().await.unwrap();
        }
    }

    #[test]
    fn launcher_passes_alias_untouched_with_private_agent() {
        // The alias is one argv word: no shell, no parsing, no reconstruction.
        // Even hostile-looking aliases belong to OpenSSH's own resolution.
        let target = SshTarget::new("host alias;$(touch /tmp/pwn)", Some("alice".to_string()));
        let args = ssh_arguments(&target, &SshLaunchOptions::default());

        assert_eq!(
            &args[args.len() - 3..],
            [
                OsString::from("host alias;$(touch /tmp/pwn)"),
                OsString::from("sy"),
                OsString::from("__serve"),
            ]
        );
        assert_eq!(&args[..2], [OsString::from("-l"), OsString::from("alice")]);
    }

    #[test]
    fn launcher_defers_configuration_to_openssh() {
        let target = SshTarget::new("gh", None);
        let args = ssh_arguments(&target, &SshLaunchOptions::default());
        let rendered: Vec<String> = args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();

        // Port, identities, jump hosts, control masters, and compression all
        // come from the user's ssh_config; sy must not reconstruct them.
        for reconstructed in ["-p", "-i", "-J", "-C", "-o", "-l"] {
            assert!(
                !rendered.iter().any(|arg| arg == reconstructed),
                "{reconstructed} must not be synthesized from ssh_config"
            );
        }
        // No remote root on argv: roots are protocol data.
        assert!(!rendered.iter().any(|arg| arg.contains("/remote/root")));
        // No user: OpenSSH applies the alias's own User= (or the local user).
        assert_eq!(rendered, vec!["gh", "sy", "__serve"]);
    }

    #[test]
    fn contimeout_maps_to_openssh_connect_timeout() {
        let target = SshTarget::new("host.example", None);
        let launch = SshLaunchOptions {
            connect_timeout: Some(10),
        };
        let args = ssh_arguments(&target, &launch);
        let options: Vec<String> = args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            options,
            vec!["-o", "ConnectTimeout=10", "host.example", "sy", "__serve"]
        );
    }
}
