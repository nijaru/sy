use crate::protocol::Operation;
use crate::remote::router::RouterConfig;
use crate::remote::runtime::ClientRemoteSession;
use crate::ssh::config::SshConfig;
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
pub struct V3SshSession {
    child: Child,
    remote: ClientRemoteSession,
}

impl V3SshSession {
    pub async fn connect(
        config: &SshConfig,
        operation: Operation,
        remote_root: &Path,
        router_config: RouterConfig,
    ) -> Result<Self> {
        let mut command = Command::new("ssh");
        command.args(ssh_arguments(config));
        command.stdin(Stdio::piped());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::inherit());
        command.kill_on_drop(true);

        let mut child = command.spawn().context("failed to spawn OpenSSH for v3 session")?;
        let writer = child
            .stdin
            .take()
            .context("failed to open v3 SSH stdin")?;
        let reader = child
            .stdout
            .take()
            .context("failed to open v3 SSH stdout")?;
        let remote = ClientRemoteSession::connect(
            reader,
            writer,
            operation,
            remote_root,
            router_config,
        )
        .await
        .context("v3 SSH handshake failed")?;

        Ok(Self { child, remote })
    }

    pub const fn remote(&self) -> &ClientRemoteSession {
        &self.remote
    }

    pub fn child_id(&self) -> Option<u32> {
        self.child.id()
    }
}

fn ssh_arguments(config: &SshConfig) -> Vec<OsString> {
    let mut args = Vec::new();

    if !config.user.is_empty() {
        args.push(OsString::from("-l"));
        args.push(OsString::from(&config.user));
    }
    if config.port != 22 {
        args.push(OsString::from("-p"));
        args.push(OsString::from(config.port.to_string()));
    }
    for identity in &config.identity_file {
        args.push(OsString::from("-i"));
        args.push(identity.as_os_str().to_os_string());
    }
    if let Some(proxy_jump) = &config.proxy_jump {
        args.push(OsString::from("-J"));
        args.push(OsString::from(proxy_jump));
    }
    if config.control_master {
        args.push(OsString::from("-o"));
        args.push(OsString::from("ControlMaster=auto"));
    }
    if let Some(control_path) = &config.control_path {
        args.push(OsString::from("-o"));
        args.push(OsString::from(format!(
            "ControlPath={}",
            control_path.display()
        )));
    }
    if let Some(control_persist) = config.control_persist {
        args.push(OsString::from("-o"));
        args.push(OsString::from(format!(
            "ControlPersist={}",
            control_persist.as_secs()
        )));
    }
    if config.compression {
        args.push(OsString::from("-C"));
    }

    args.push(OsString::from(&config.hostname));
    args.push(OsString::from("sy"));
    args.push(OsString::from("__serve"));
    args
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::Duration;

    #[test]
    fn v3_launcher_uses_private_agent_without_remote_root_argv() {
        let config = SshConfig {
            hostname: "host.example".to_string(),
            port: 2202,
            user: "alice".to_string(),
            identity_file: vec![PathBuf::from("/tmp/key")],
            proxy_jump: Some("jump.example".to_string()),
            control_master: true,
            control_path: Some(PathBuf::from("/tmp/sy-control")),
            control_persist: Some(Duration::from_secs(30)),
            compression: true,
        };

        let args = ssh_arguments(&config);
        assert_eq!(
            &args[args.len() - 3..],
            [
                OsString::from("host.example"),
                OsString::from("sy"),
                OsString::from("__serve"),
            ]
        );
        assert!(!args.iter().any(|arg| arg == "/remote/root"));
        assert!(args.iter().any(|arg| arg == "-J"));
        assert!(args.iter().any(|arg| arg == "-C"));
    }
}
