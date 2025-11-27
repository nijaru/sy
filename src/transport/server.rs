// Server session client - used by server_mode.rs for connecting to `sy --server`
// Some methods are reserved for future features (chunked transfer, graceful close)
#![allow(dead_code)]

use anyhow::{Context, Result};
use std::path::Path;
use std::process::Stdio;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};

use crate::server::protocol::{
    self, ChecksumReq, ChecksumResp, Decision, DeltaData, DeltaOp, FileData, FileDone, FileList,
    FileListAck, FileListEntry, Hello, MessageType, MkdirBatch, MkdirBatchAck, SymlinkBatch,
    SymlinkBatchAck, SymlinkEntry, HELLO_FLAG_PULL, PROTOCOL_VERSION,
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

        let mut session = Self {
            child,
            stdin,
            stdout,
        };

        session.handshake().await?;

        Ok(session)
    }

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

    // =========================================================================
    // FILE_LIST
    // =========================================================================

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

    // =========================================================================
    // MKDIR_BATCH
    // =========================================================================

    pub async fn send_mkdir_batch(&mut self, paths: Vec<String>) -> Result<()> {
        let batch = MkdirBatch { paths };
        batch.write(&mut self.stdin).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    pub async fn read_mkdir_ack(&mut self) -> Result<MkdirBatchAck> {
        let _len = self.stdout.read_u32().await?;
        let type_byte = self.stdout.read_u8().await?;

        if type_byte == MessageType::Error as u8 {
            let err = protocol::ErrorMessage::read(&mut self.stdout).await?;
            return Err(anyhow::anyhow!("Server error: {}", err.message));
        }

        if type_byte != MessageType::MkdirBatchAck as u8 {
            return Err(anyhow::anyhow!(
                "Expected MKDIR_BATCH_ACK, got 0x{:02X}",
                type_byte
            ));
        }

        MkdirBatchAck::read(&mut self.stdout).await
    }

    // =========================================================================
    // SYMLINK_BATCH
    // =========================================================================

    pub async fn send_symlink_batch(&mut self, entries: Vec<SymlinkEntry>) -> Result<()> {
        let batch = SymlinkBatch { entries };
        batch.write(&mut self.stdin).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    pub async fn read_symlink_ack(&mut self) -> Result<SymlinkBatchAck> {
        let _len = self.stdout.read_u32().await?;
        let type_byte = self.stdout.read_u8().await?;

        if type_byte == MessageType::Error as u8 {
            let err = protocol::ErrorMessage::read(&mut self.stdout).await?;
            return Err(anyhow::anyhow!("Server error: {}", err.message));
        }

        if type_byte != MessageType::SymlinkBatchAck as u8 {
            return Err(anyhow::anyhow!(
                "Expected SYMLINK_BATCH_ACK, got 0x{:02X}",
                type_byte
            ));
        }

        SymlinkBatchAck::read(&mut self.stdout).await
    }

    // =========================================================================
    // FILE_DATA
    // =========================================================================

    pub async fn send_file_data(&mut self, index: u32, offset: u64, data: Vec<u8>) -> Result<()> {
        let file_data = FileData {
            index,
            offset,
            flags: 0,
            data,
        };
        file_data.write(&mut self.stdin).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    /// Send file data with flags (e.g., compressed), without flushing - use flush() after batch
    pub async fn send_file_data_no_flush(
        &mut self,
        index: u32,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<()> {
        let file_data = FileData {
            index,
            offset,
            flags: 0,
            data,
        };
        file_data.write(&mut self.stdin).await?;
        Ok(())
    }

    /// Send file data with explicit flags, without flushing
    pub async fn send_file_data_with_flags(
        &mut self,
        index: u32,
        offset: u64,
        flags: u8,
        data: Vec<u8>,
    ) -> Result<()> {
        let file_data = FileData {
            index,
            offset,
            flags,
            data,
        };
        file_data.write(&mut self.stdin).await?;
        Ok(())
    }

    /// Flush the write buffer
    pub async fn flush(&mut self) -> Result<()> {
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

    // =========================================================================
    // DELTA SYNC
    // =========================================================================

    /// Request block checksums for a file (for delta sync)
    pub async fn send_checksum_req(&mut self, index: u32, block_size: u32) -> Result<()> {
        let req = ChecksumReq { index, block_size };
        req.write(&mut self.stdin).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    /// Read checksum response
    pub async fn read_checksum_resp(&mut self) -> Result<ChecksumResp> {
        let _len = self.stdout.read_u32().await?;
        let type_byte = self.stdout.read_u8().await?;

        if type_byte == MessageType::Error as u8 {
            let err = protocol::ErrorMessage::read(&mut self.stdout).await?;
            return Err(anyhow::anyhow!("Server error: {}", err.message));
        }

        if type_byte != MessageType::ChecksumResp as u8 {
            return Err(anyhow::anyhow!(
                "Expected CHECKSUM_RESP, got 0x{:02X}",
                type_byte
            ));
        }

        ChecksumResp::read(&mut self.stdout).await
    }

    /// Send delta data (for updating existing file)
    pub async fn send_delta_data(
        &mut self,
        index: u32,
        flags: u8,
        ops: Vec<DeltaOp>,
    ) -> Result<()> {
        let delta = DeltaData { index, flags, ops };
        delta.write(&mut self.stdin).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    /// Send delta data without flushing
    pub async fn send_delta_data_no_flush(
        &mut self,
        index: u32,
        flags: u8,
        ops: Vec<DeltaOp>,
    ) -> Result<()> {
        let delta = DeltaData { index, flags, ops };
        delta.write(&mut self.stdin).await?;
        Ok(())
    }

    // =========================================================================
    // PULL MODE (client receives files from server)
    // =========================================================================

    /// Connect to SSH server in PULL mode (server sends files to client)
    pub async fn connect_ssh_pull(config: &SshConfig, remote_path: &Path) -> Result<Self> {
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

        cmd.arg("sy");
        cmd.arg("--server");
        cmd.arg(remote_path);

        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::inherit());

        let mut child = cmd.spawn().context("Failed to spawn SSH process")?;

        let stdin = child.stdin.take().context("Failed to open stdin")?;
        let stdout = child.stdout.take().context("Failed to open stdout")?;

        let mut session = Self {
            child,
            stdin,
            stdout,
        };

        session.handshake_pull().await?;

        Ok(session)
    }

    /// Connect to local sy --server in PULL mode
    pub async fn connect_local_pull(remote_path: &Path) -> Result<Self> {
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

        session.handshake_pull().await?;

        Ok(session)
    }

    /// Handshake with PULL flag set
    async fn handshake_pull(&mut self) -> Result<()> {
        let hello = Hello {
            version: PROTOCOL_VERSION,
            flags: HELLO_FLAG_PULL,
            capabilities: vec![],
        };

        hello.write(&mut self.stdin).await?;
        self.stdin.flush().await?;

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

    /// Read MKDIR_BATCH from server (PULL mode)
    pub async fn read_mkdir_batch(&mut self) -> Result<Option<MkdirBatch>> {
        let _len = self.stdout.read_u32().await?;
        let type_byte = self.stdout.read_u8().await?;

        // Server might send FILE_LIST directly if no directories
        if type_byte == MessageType::FileList as u8 {
            // Put it back by storing the type - caller should call read_file_list
            return Ok(None);
        }

        if type_byte == MessageType::Error as u8 {
            let err = protocol::ErrorMessage::read(&mut self.stdout).await?;
            return Err(anyhow::anyhow!("Server error: {}", err.message));
        }

        if type_byte != MessageType::MkdirBatch as u8 {
            return Err(anyhow::anyhow!(
                "Expected MKDIR_BATCH or FILE_LIST, got 0x{:02X}",
                type_byte
            ));
        }

        let batch = MkdirBatch::read(&mut self.stdout).await?;
        Ok(Some(batch))
    }

    /// Send MKDIR_BATCH_ACK to server (PULL mode)
    pub async fn send_mkdir_batch_ack(
        &mut self,
        created: u32,
        failed: Vec<(String, String)>,
    ) -> Result<()> {
        let ack = MkdirBatchAck { created, failed };
        ack.write(&mut self.stdin).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    /// Read FILE_LIST from server (PULL mode)
    pub async fn read_file_list(&mut self) -> Result<FileList> {
        let _len = self.stdout.read_u32().await?;
        let type_byte = self.stdout.read_u8().await?;

        if type_byte == MessageType::Error as u8 {
            let err = protocol::ErrorMessage::read(&mut self.stdout).await?;
            return Err(anyhow::anyhow!("Server error: {}", err.message));
        }

        if type_byte != MessageType::FileList as u8 {
            return Err(anyhow::anyhow!(
                "Expected FILE_LIST, got 0x{:02X}",
                type_byte
            ));
        }

        FileList::read(&mut self.stdout).await
    }

    /// Send FILE_LIST_ACK to server (PULL mode)
    pub async fn send_file_list_ack(&mut self, decisions: Vec<Decision>) -> Result<()> {
        let ack = FileListAck { decisions };
        ack.write(&mut self.stdin).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    /// Read FILE_DATA from server (PULL mode)
    pub async fn read_file_data(&mut self) -> Result<Option<FileData>> {
        let _len = self.stdout.read_u32().await?;
        let type_byte = self.stdout.read_u8().await?;

        // Server might send SYMLINK_BATCH if done with files
        if type_byte == MessageType::SymlinkBatch as u8 {
            return Ok(None);
        }

        if type_byte == MessageType::Error as u8 {
            let err = protocol::ErrorMessage::read(&mut self.stdout).await?;
            return Err(anyhow::anyhow!("Server error: {}", err.message));
        }

        if type_byte != MessageType::FileData as u8 {
            return Err(anyhow::anyhow!(
                "Expected FILE_DATA or SYMLINK_BATCH, got 0x{:02X}",
                type_byte
            ));
        }

        let data = FileData::read(&mut self.stdout).await?;
        Ok(Some(data))
    }

    /// Send FILE_DONE to server (PULL mode)
    pub async fn send_file_done(&mut self, index: u32, status: u8) -> Result<()> {
        let done = FileDone {
            index,
            status,
            checksum: vec![],
        };
        done.write(&mut self.stdin).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    /// Read SYMLINK_BATCH from server (PULL mode) - assumes type byte already read
    pub async fn read_symlink_batch_body(&mut self) -> Result<SymlinkBatch> {
        SymlinkBatch::read(&mut self.stdout).await
    }

    /// Send SYMLINK_BATCH_ACK to server (PULL mode)
    pub async fn send_symlink_batch_ack(
        &mut self,
        created: u32,
        failed: Vec<(String, String)>,
    ) -> Result<()> {
        let ack = SymlinkBatchAck { created, failed };
        ack.write(&mut self.stdin).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    // =========================================================================
    // Lifecycle
    // =========================================================================

    /// Close the session gracefully
    pub async fn close(mut self) -> Result<()> {
        drop(self.stdin);
        let _ = self.child.wait().await;
        Ok(())
    }
}
