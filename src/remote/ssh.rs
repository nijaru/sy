use crate::protocol::Operation;
use crate::remote::router::RouterConfig;
use crate::remote::runtime::ClientRemoteSession;
use crate::ssh::SshTarget;
use anyhow::{Context, Result};
use std::ffi::OsString;
use std::path::Path;
use std::process::Stdio;
use tokio::process::{Child, Command};

/// Owns the OpenSSH child for one v3 remote session.
///
/// Keeping the child beside the negotiated session avoids the legacy transport's
/// detached-child lifetime. `kill_on_drop` ensures an abandoned local session
/// cannot leave a private `sy __serve` process behind indefinitely.
pub struct SshRemoteSession {
    _child: Child,
    remote: ClientRemoteSession,
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
        command.kill_on_drop(true);

        let mut child = command
            .spawn()
            .context("failed to spawn OpenSSH for v3 session")?;
        let writer = child.stdin.take().context("failed to open v3 SSH stdin")?;
        let reader = child
            .stdout
            .take()
            .context("failed to open v3 SSH stdout")?;
        let remote =
            ClientRemoteSession::connect(reader, writer, operation, remote_root, router_config)
                .await
                .context("v3 SSH handshake failed")?;

        Ok(Self {
            _child: child,
            remote,
        })
    }

    pub const fn remote(&self) -> &ClientRemoteSession {
        &self.remote
    }
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
