use anyhow::{Context, Result};
use std::path::Path;
use std::process::Stdio;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};

use crate::server::protocol::{
    self, FileData, FileDone, FileList, FileListAck, FileListEntry, Hello, MessageType,
    PROTOCOL_VERSION,
};
use crate::ssh::config::SshConfig;

/// Manages the client-side connection to a remote sy --server instance
pub struct ServerSession {
    child: Child,
    stdin: tokio::process::ChildStdin,
    stdout: tokio::process::ChildStdout,
}

impl ServerSession {
    pub async fn connect_ssh(config: &SshConfig, remote_path: &Path) -> Result<Self> {
        let mut cmd = Command::new("ssh");

        // Configure SSH options
        cmd.arg(&config.hostname);
        if !config.user.is_empty() {
            cmd.arg("-l").arg(&config.user);
        }

        // Port is u16, always valid. Only add if non-standard? Or always?
        // Let's always add it if it's not 22, or just always.
        if config.port != 22 {
            cmd.arg("-p").arg(config.port.to_string());
        }

        for key in &config.identity_file {
            cmd.arg("-i").arg(key);
        }

        // Force TTY allocation? No, we want binary stream.
        // Use compression? Yes, let SSH handle it or sy?
        // sy --server handles compression natively if implemented.

        // Remote command: sy --server <remote_path>
        // Assuming 'sy' is in PATH.
        cmd.arg("sy");
        cmd.arg("--server");
        cmd.arg(remote_path);

        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::inherit()); // Let stderr go to console for now

        let mut child = cmd.spawn().context("Failed to spawn SSH process")?;

        let stdin = child.stdin.take().context("Failed to open stdin")?;
        let stdout = child.stdout.take().context("Failed to open stdout")?;

        let mut session = Self {
            child,
            stdin,
            stdout,
        };

        session.handshake().await?;

        Ok(session)
    }

    pub async fn connect_local(remote_path: &Path) -> Result<Self> {
        // For testing/local mode: spawn sy directly
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

        let mut session = Self {
            child,
            stdin,
            stdout,
        };

        session.handshake().await?;

        Ok(session)
    }

    async fn handshake(&mut self) -> Result<()> {
        let hello = Hello {
            version: PROTOCOL_VERSION,
            flags: 0,
            capabilities: vec![],
        };

        hello.write(&mut self.stdin).await?;
        self.stdin.flush().await?;

        // Read response
        let _len = self.stdout.read_u32().await?;
        let type_byte = self.stdout.read_u8().await?;

        if type_byte == MessageType::Error as u8 {
            let err = protocol::ErrorMessage::read(&mut self.stdout).await?;
            return Err(anyhow::anyhow!("Server handshake error: {}", err.message));
        }

        if type_byte != MessageType::Hello as u8 {
            return Err(anyhow::anyhow!("Expected HELLO, got 0x{:02X}", type_byte));
        }

        let resp = Hello::read(&mut self.stdout).await?;

        if resp.version != PROTOCOL_VERSION {
            return Err(anyhow::anyhow!("Version mismatch: server {}", resp.version));
        }

        Ok(())
    }

    pub async fn send_file_list(&mut self, entries: Vec<FileListEntry>) -> Result<()> {
        let list = FileList { entries };
        list.write(&mut self.stdin).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    pub async fn read_ack(&mut self) -> Result<FileListAck> {
        let _len = self.stdout.read_u32().await?;
        let type_byte = self.stdout.read_u8().await?;

        if type_byte == MessageType::Error as u8 {
            let err = protocol::ErrorMessage::read(&mut self.stdout).await?;
            return Err(anyhow::anyhow!("Server error: {}", err.message));
        }

        if type_byte != MessageType::FileListAck as u8 {
            return Err(anyhow::anyhow!(
                "Expected FILE_LIST_ACK, got 0x{:02X}",
                type_byte
            ));
        }

        FileListAck::read(&mut self.stdout).await
    }

    pub async fn send_file_data(&mut self, index: u32, offset: u64, data: Vec<u8>) -> Result<()> {
        let file_data = FileData {
            index,
            offset,
            data,
        };
        file_data.write(&mut self.stdin).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    pub async fn read_file_done(&mut self) -> Result<FileDone> {
        let _len = self.stdout.read_u32().await?;
        let type_byte = self.stdout.read_u8().await?;

        if type_byte == MessageType::Error as u8 {
            let err = protocol::ErrorMessage::read(&mut self.stdout).await?;
            return Err(anyhow::anyhow!("Server error: {}", err.message));
        }

        if type_byte != MessageType::FileDone as u8 {
            return Err(anyhow::anyhow!(
                "Expected FILE_DONE, got 0x{:02X}",
                type_byte
            ));
        }

        FileDone::read(&mut self.stdout).await
    }
}
