use super::config::SshConfig;
use crate::error::{Result, SyncError};
use ssh2::Session;
use std::io::ErrorKind;
use std::net::TcpStream;
use std::time::Duration;

/// SSH connection timeout (default 30 seconds)
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Establish an SSH connection using the provided configuration
///
/// This function:
/// 1. Establishes a TCP connection to the SSH server
/// 2. Creates an SSH session
/// 3. Performs SSH handshake
/// 4. Authenticates using available methods (keys, agent, password)
pub async fn connect(config: &SshConfig) -> Result<Session> {
    // Establish TCP connection
    let tcp = connect_tcp(&config.hostname, config.port).await?;

    // Clone config data needed for authentication
    let username = config.user.clone();
    let identity_files = config.identity_file.clone();

    // Wrap all sync operations (session creation, handshake, auth) in spawn_blocking
    let session = tokio::task::spawn_blocking(move || {
        // Create SSH session
        let mut session = Session::new().map_err(|e| {
            SyncError::Io(std::io::Error::other(format!(
                "Failed to create SSH session: {}",
                e
            )))
        })?;

        // Keep session blocking for handshake and authentication
        // (we're already in spawn_blocking context)
        session.set_timeout(DEFAULT_TIMEOUT.as_millis() as u32);

        // Set TCP stream
        session.set_tcp_stream(tcp);

        // Perform SSH handshake
        session.handshake().map_err(|e| {
            SyncError::Io(std::io::Error::other(format!(
                "SSH handshake failed: {}",
                e
            )))
        })?;

        // Configure keepalive to prevent connection drops during long transfers
        // Send keepalive every 60 seconds, disconnect after 3 missed responses
        session.set_keepalive(true, 60);

        // Try authentication methods in order of preference:
        // 1. SSH agent (if available)
        // 2. Identity files (keys)
        // 3. Default keys

        // Try SSH agent first
        if let Ok(mut agent) = session.agent() {
            if agent.connect().is_ok() && agent.list_identities().is_ok() {
                if let Ok(identities) = agent.identities() {
                    for identity in identities {
                        if agent.userauth(&username, &identity).is_ok() {
                            tracing::debug!("Authenticated using SSH agent");
                            return Ok(session);
                        }
                    }
                }
            }
        }

        // Try each identity file
        for identity_file in &identity_files {
            if session
                .userauth_pubkey_file(&username, None, identity_file, None)
                .is_ok()
            {
                tracing::debug!("Authenticated using key: {}", identity_file.display());
                return Ok(session);
            }
        }

        // Try default keys if no identity files specified
        if identity_files.is_empty() {
            if let Some(home) = dirs::home_dir() {
                let default_keys = [
                    home.join(".ssh/id_rsa"),
                    home.join(".ssh/id_ed25519"),
                    home.join(".ssh/id_ecdsa"),
                ];

                for key_path in &default_keys {
                    if key_path.exists()
                        && session
                            .userauth_pubkey_file(&username, None, key_path, None)
                            .is_ok()
                    {
                        tracing::debug!("Authenticated using key: {}", key_path.display());
                        return Ok(session);
                    }
                }
            }
        }

        Err(SyncError::Io(std::io::Error::new(
            ErrorKind::PermissionDenied,
            format!("SSH authentication failed for user {}", username),
        )))
    })
    .await
    .map_err(|e| SyncError::Io(std::io::Error::other(e.to_string())))??;

    Ok(session)
}

/// Establish TCP connection to SSH server
async fn connect_tcp(hostname: &str, port: u16) -> Result<TcpStream> {
    let addr = format!("{}:{}", hostname, port);

    tokio::time::timeout(DEFAULT_TIMEOUT, async {
        TcpStream::connect(&addr).map_err(|e| {
            SyncError::Io(std::io::Error::new(
                ErrorKind::ConnectionRefused,
                format!("Failed to connect to {}: {}", addr, e),
            ))
        })
    })
    .await
    .map_err(|_| {
        SyncError::Io(std::io::Error::new(
            ErrorKind::TimedOut,
            format!("Connection to {} timed out", addr),
        ))
    })?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_ssh_config_basic() {
        let config = SshConfig {
            hostname: "localhost".to_string(),
            port: 22,
            user: "testuser".to_string(),
            identity_file: vec![PathBuf::from("~/.ssh/id_rsa")],
            proxy_jump: None,
            control_master: false,
            control_path: None,
            control_persist: None,
            compression: false,
        };

        assert_eq!(config.hostname, "localhost");
        assert_eq!(config.port, 22);
        assert_eq!(config.user, "testuser");
    }

    // Note: Actual connection tests require a running SSH server
    // These would be integration tests, not unit tests
}
