use anyhow::{Context, Result};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

// Protocol Constants
pub const PROTOCOL_VERSION: u16 = 1;

// File entry flags
pub const FLAG_IS_DIR: u8 = 0x01;
pub const FLAG_IS_SYMLINK: u8 = 0x02;
pub const FLAG_IS_HARDLINK: u8 = 0x04;
pub const FLAG_HAS_XATTRS: u8 = 0x08;

// FileDone status codes
pub const STATUS_OK: u8 = 0;
pub const STATUS_CHECKSUM_MISMATCH: u8 = 1;
pub const STATUS_WRITE_ERROR: u8 = 2;
pub const STATUS_PERMISSION_DENIED: u8 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MessageType {
    Hello = 0x01,
    FileList = 0x02,
    FileListAck = 0x03,
    FileData = 0x04,
    FileDone = 0x05,
    MkdirBatch = 0x06,
    MkdirBatchAck = 0x07,
    SymlinkBatch = 0x08,
    SymlinkBatchAck = 0x09,
    DeleteBatch = 0x0A,
    DeleteBatchAck = 0x0B,
    ChecksumReq = 0x10,
    ChecksumResp = 0x11,
    DeltaData = 0x12,
    Progress = 0x20,
    Error = 0xFF,
}

impl MessageType {
    pub fn from_u8(b: u8) -> Option<Self> {
        match b {
            0x01 => Some(Self::Hello),
            0x02 => Some(Self::FileList),
            0x03 => Some(Self::FileListAck),
            0x04 => Some(Self::FileData),
            0x05 => Some(Self::FileDone),
            0x06 => Some(Self::MkdirBatch),
            0x07 => Some(Self::MkdirBatchAck),
            0x08 => Some(Self::SymlinkBatch),
            0x09 => Some(Self::SymlinkBatchAck),
            0x0A => Some(Self::DeleteBatch),
            0x0B => Some(Self::DeleteBatchAck),
            0x10 => Some(Self::ChecksumReq),
            0x11 => Some(Self::ChecksumResp),
            0x12 => Some(Self::DeltaData),
            0x20 => Some(Self::Progress),
            0xFF => Some(Self::Error),
            _ => None,
        }
    }
}

// Helper functions for serialization
async fn write_string<W: AsyncWrite + Unpin>(w: &mut W, s: &str) -> Result<()> {
    let bytes = s.as_bytes();
    w.write_u16(bytes.len() as u16).await?;
    w.write_all(bytes).await?;
    Ok(())
}

async fn read_string<R: AsyncRead + Unpin>(r: &mut R) -> Result<String> {
    let len = r.read_u16().await? as usize;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await?;
    String::from_utf8(buf).context("Invalid UTF-8 string")
}

async fn write_bytes<W: AsyncWrite + Unpin>(w: &mut W, b: &[u8]) -> Result<()> {
    w.write_u32(b.len() as u32).await?;
    w.write_all(b).await?;
    Ok(())
}

async fn read_bytes<R: AsyncRead + Unpin>(r: &mut R) -> Result<Vec<u8>> {
    let len = r.read_u32().await? as usize;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await?;
    Ok(buf)
}

// ============================================================================
// HELLO (0x01)
// ============================================================================

#[derive(Debug)]
pub struct Hello {
    pub version: u16,
    pub flags: u32,
    pub capabilities: Vec<u8>,
}

impl Hello {
    pub async fn write<W: AsyncWrite + Unpin>(&self, w: &mut W) -> Result<()> {
        let len = 2 + 4 + 4 + self.capabilities.len() as u32;
        w.write_u32(len).await?;
        w.write_u8(MessageType::Hello as u8).await?;
        w.write_u16(self.version).await?;
        w.write_u32(self.flags).await?;
        write_bytes(w, &self.capabilities).await?;
        Ok(())
    }

    pub async fn read<R: AsyncRead + Unpin>(r: &mut R) -> Result<Self> {
        let version = r.read_u16().await?;
        let flags = r.read_u32().await?;
        let capabilities = read_bytes(r).await?;
        Ok(Hello {
            version,
            flags,
            capabilities,
        })
    }
}

// ============================================================================
// FILE_LIST (0x02)
// ============================================================================

#[derive(Debug, Clone)]
pub struct FileListEntry {
    pub path: String,
    pub size: u64,
    pub mtime: i64,
    pub mode: u32,
    pub flags: u8,
    pub symlink_target: Option<String>,
}

impl FileListEntry {
    pub fn is_dir(&self) -> bool {
        self.flags & FLAG_IS_DIR != 0
    }

    pub fn is_symlink(&self) -> bool {
        self.flags & FLAG_IS_SYMLINK != 0
    }
}

#[derive(Debug)]
pub struct FileList {
    pub entries: Vec<FileListEntry>,
}

impl FileList {
    pub async fn write<W: AsyncWrite + Unpin>(&self, w: &mut W) -> Result<()> {
        let mut payload = Vec::new();
        payload.write_u32(self.entries.len() as u32).await?;

        for entry in &self.entries {
            let path_bytes = entry.path.as_bytes();
            payload.write_u16(path_bytes.len() as u16).await?;
            payload.write_all(path_bytes).await?;
            payload.write_u64(entry.size).await?;
            payload.write_i64(entry.mtime).await?;
            payload.write_u32(entry.mode).await?;
            payload.write_u8(entry.flags).await?;

            // Write symlink target if present
            if let Some(ref target) = entry.symlink_target {
                let target_bytes = target.as_bytes();
                payload.write_u16(target_bytes.len() as u16).await?;
                payload.write_all(target_bytes).await?;
            } else if entry.is_symlink() {
                // Empty target for broken symlinks
                payload.write_u16(0).await?;
            }
        }

        w.write_u32(payload.len() as u32).await?;
        w.write_u8(MessageType::FileList as u8).await?;
        w.write_all(&payload).await?;
        Ok(())
    }

    pub async fn read<R: AsyncRead + Unpin>(r: &mut R) -> Result<Self> {
        let count = r.read_u32().await? as usize;
        let mut entries = Vec::with_capacity(count);

        for _ in 0..count {
            let path = read_string(r).await?;
            let size = r.read_u64().await?;
            let mtime = r.read_i64().await?;
            let mode = r.read_u32().await?;
            let flags = r.read_u8().await?;

            let symlink_target = if flags & FLAG_IS_SYMLINK != 0 {
                let target = read_string(r).await?;
                if target.is_empty() {
                    None
                } else {
                    Some(target)
                }
            } else {
                None
            };

            entries.push(FileListEntry {
                path,
                size,
                mtime,
                mode,
                flags,
                symlink_target,
            });
        }

        Ok(FileList { entries })
    }
}

// ============================================================================
// FILE_LIST_ACK (0x03)
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Action {
    Skip = 0,
    Create = 1,
    Update = 2,
    Delete = 3,
}

impl Action {
    pub fn from_u8(b: u8) -> Option<Self> {
        match b {
            0 => Some(Self::Skip),
            1 => Some(Self::Create),
            2 => Some(Self::Update),
            3 => Some(Self::Delete),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub struct Decision {
    pub index: u32,
    pub action: Action,
}

#[derive(Debug)]
pub struct FileListAck {
    pub decisions: Vec<Decision>,
}

impl FileListAck {
    pub async fn write<W: AsyncWrite + Unpin>(&self, w: &mut W) -> Result<()> {
        let len = 4 + self.decisions.len() as u32 * 5;
        w.write_u32(len).await?;
        w.write_u8(MessageType::FileListAck as u8).await?;
        w.write_u32(self.decisions.len() as u32).await?;
        for d in &self.decisions {
            w.write_u32(d.index).await?;
            w.write_u8(d.action as u8).await?;
        }
        Ok(())
    }

    pub async fn read<R: AsyncRead + Unpin>(r: &mut R) -> Result<Self> {
        let count = r.read_u32().await? as usize;
        let mut decisions = Vec::with_capacity(count);
        for _ in 0..count {
            let index = r.read_u32().await?;
            let action_byte = r.read_u8().await?;
            let action = Action::from_u8(action_byte).unwrap_or(Action::Skip);
            decisions.push(Decision { index, action });
        }
        Ok(FileListAck { decisions })
    }
}

// ============================================================================
// FILE_DATA (0x04)
// ============================================================================

#[derive(Debug)]
pub struct FileData {
    pub index: u32,
    pub offset: u64,
    pub data: Vec<u8>,
}

impl FileData {
    pub async fn write<W: AsyncWrite + Unpin>(&self, w: &mut W) -> Result<()> {
        let len = 4 + 8 + 4 + self.data.len() as u32;
        w.write_u32(len).await?;
        w.write_u8(MessageType::FileData as u8).await?;
        w.write_u32(self.index).await?;
        w.write_u64(self.offset).await?;
        write_bytes(w, &self.data).await?;
        Ok(())
    }

    pub async fn read<R: AsyncRead + Unpin>(r: &mut R) -> Result<Self> {
        let index = r.read_u32().await?;
        let offset = r.read_u64().await?;
        let data = read_bytes(r).await?;
        Ok(FileData {
            index,
            offset,
            data,
        })
    }
}

// ============================================================================
// FILE_DONE (0x05)
// ============================================================================

#[derive(Debug)]
pub struct FileDone {
    pub index: u32,
    pub status: u8,
    pub checksum: Vec<u8>,
}

impl FileDone {
    pub async fn write<W: AsyncWrite + Unpin>(&self, w: &mut W) -> Result<()> {
        let len = 4 + 1 + 4 + self.checksum.len() as u32;
        w.write_u32(len).await?;
        w.write_u8(MessageType::FileDone as u8).await?;
        w.write_u32(self.index).await?;
        w.write_u8(self.status).await?;
        write_bytes(w, &self.checksum).await?;
        Ok(())
    }

    pub async fn read<R: AsyncRead + Unpin>(r: &mut R) -> Result<Self> {
        let index = r.read_u32().await?;
        let status = r.read_u8().await?;
        let checksum = read_bytes(r).await?;
        Ok(FileDone {
            index,
            status,
            checksum,
        })
    }
}

// ============================================================================
// MKDIR_BATCH (0x06)
// ============================================================================

#[derive(Debug)]
pub struct MkdirBatch {
    pub paths: Vec<String>,
}

impl MkdirBatch {
    pub async fn write<W: AsyncWrite + Unpin>(&self, w: &mut W) -> Result<()> {
        let mut payload = Vec::new();
        payload.write_u32(self.paths.len() as u32).await?;
        for path in &self.paths {
            let bytes = path.as_bytes();
            payload.write_u16(bytes.len() as u16).await?;
            payload.write_all(bytes).await?;
        }
        w.write_u32(payload.len() as u32).await?;
        w.write_u8(MessageType::MkdirBatch as u8).await?;
        w.write_all(&payload).await?;
        Ok(())
    }

    pub async fn read<R: AsyncRead + Unpin>(r: &mut R) -> Result<Self> {
        let count = r.read_u32().await? as usize;
        let mut paths = Vec::with_capacity(count);
        for _ in 0..count {
            paths.push(read_string(r).await?);
        }
        Ok(MkdirBatch { paths })
    }
}

// ============================================================================
// MKDIR_BATCH_ACK (0x07)
// ============================================================================

#[derive(Debug)]
pub struct MkdirBatchAck {
    pub created: u32,
    pub failed: Vec<(String, String)>, // path, error message
}

impl MkdirBatchAck {
    pub async fn write<W: AsyncWrite + Unpin>(&self, w: &mut W) -> Result<()> {
        let mut payload = Vec::new();
        payload.write_u32(self.created).await?;
        payload.write_u32(self.failed.len() as u32).await?;
        for (path, err) in &self.failed {
            let path_bytes = path.as_bytes();
            payload.write_u16(path_bytes.len() as u16).await?;
            payload.write_all(path_bytes).await?;
            let err_bytes = err.as_bytes();
            payload.write_u16(err_bytes.len() as u16).await?;
            payload.write_all(err_bytes).await?;
        }
        w.write_u32(payload.len() as u32).await?;
        w.write_u8(MessageType::MkdirBatchAck as u8).await?;
        w.write_all(&payload).await?;
        Ok(())
    }

    pub async fn read<R: AsyncRead + Unpin>(r: &mut R) -> Result<Self> {
        let created = r.read_u32().await?;
        let failed_count = r.read_u32().await? as usize;
        let mut failed = Vec::with_capacity(failed_count);
        for _ in 0..failed_count {
            let path = read_string(r).await?;
            let err = read_string(r).await?;
            failed.push((path, err));
        }
        Ok(MkdirBatchAck { created, failed })
    }
}

// ============================================================================
// SYMLINK_BATCH (0x08)
// ============================================================================

#[derive(Debug)]
pub struct SymlinkEntry {
    pub path: String,
    pub target: String,
}

#[derive(Debug)]
pub struct SymlinkBatch {
    pub entries: Vec<SymlinkEntry>,
}

impl SymlinkBatch {
    pub async fn write<W: AsyncWrite + Unpin>(&self, w: &mut W) -> Result<()> {
        let mut payload = Vec::new();
        payload.write_u32(self.entries.len() as u32).await?;
        for entry in &self.entries {
            let path_bytes = entry.path.as_bytes();
            payload.write_u16(path_bytes.len() as u16).await?;
            payload.write_all(path_bytes).await?;
            let target_bytes = entry.target.as_bytes();
            payload.write_u16(target_bytes.len() as u16).await?;
            payload.write_all(target_bytes).await?;
        }
        w.write_u32(payload.len() as u32).await?;
        w.write_u8(MessageType::SymlinkBatch as u8).await?;
        w.write_all(&payload).await?;
        Ok(())
    }

    pub async fn read<R: AsyncRead + Unpin>(r: &mut R) -> Result<Self> {
        let count = r.read_u32().await? as usize;
        let mut entries = Vec::with_capacity(count);
        for _ in 0..count {
            let path = read_string(r).await?;
            let target = read_string(r).await?;
            entries.push(SymlinkEntry { path, target });
        }
        Ok(SymlinkBatch { entries })
    }
}

// ============================================================================
// SYMLINK_BATCH_ACK (0x09)
// ============================================================================

#[derive(Debug)]
pub struct SymlinkBatchAck {
    pub created: u32,
    pub failed: Vec<(String, String)>,
}

impl SymlinkBatchAck {
    pub async fn write<W: AsyncWrite + Unpin>(&self, w: &mut W) -> Result<()> {
        let mut payload = Vec::new();
        payload.write_u32(self.created).await?;
        payload.write_u32(self.failed.len() as u32).await?;
        for (path, err) in &self.failed {
            let path_bytes = path.as_bytes();
            payload.write_u16(path_bytes.len() as u16).await?;
            payload.write_all(path_bytes).await?;
            let err_bytes = err.as_bytes();
            payload.write_u16(err_bytes.len() as u16).await?;
            payload.write_all(err_bytes).await?;
        }
        w.write_u32(payload.len() as u32).await?;
        w.write_u8(MessageType::SymlinkBatchAck as u8).await?;
        w.write_all(&payload).await?;
        Ok(())
    }

    pub async fn read<R: AsyncRead + Unpin>(r: &mut R) -> Result<Self> {
        let created = r.read_u32().await?;
        let failed_count = r.read_u32().await? as usize;
        let mut failed = Vec::with_capacity(failed_count);
        for _ in 0..failed_count {
            let path = read_string(r).await?;
            let err = read_string(r).await?;
            failed.push((path, err));
        }
        Ok(SymlinkBatchAck { created, failed })
    }
}

// ============================================================================
// ERROR (0xFF)
// ============================================================================

#[derive(Debug)]
pub struct ErrorMessage {
    pub code: u16,
    pub message: String,
}

impl ErrorMessage {
    pub async fn write<W: AsyncWrite + Unpin>(&self, w: &mut W) -> Result<()> {
        let msg_bytes = self.message.as_bytes();
        let len = 2 + 2 + msg_bytes.len() as u32;
        w.write_u32(len).await?;
        w.write_u8(MessageType::Error as u8).await?;
        w.write_u16(self.code).await?;
        write_string(w, &self.message).await?;
        Ok(())
    }

    pub async fn read<R: AsyncRead + Unpin>(r: &mut R) -> Result<Self> {
        let code = r.read_u16().await?;
        let message = read_string(r).await?;
        Ok(ErrorMessage { code, message })
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[tokio::test]
    async fn test_hello_roundtrip() {
        let hello = Hello {
            version: PROTOCOL_VERSION,
            flags: 0x03,
            capabilities: vec![1, 2, 3],
        };

        let mut buf = Vec::new();
        hello.write(&mut buf).await.unwrap();

        // Skip length and type
        let mut cursor = Cursor::new(&buf[5..]);
        let decoded = Hello::read(&mut cursor).await.unwrap();

        assert_eq!(decoded.version, hello.version);
        assert_eq!(decoded.flags, hello.flags);
        assert_eq!(decoded.capabilities, hello.capabilities);
    }

    #[tokio::test]
    async fn test_file_list_with_symlink() {
        let list = FileList {
            entries: vec![
                FileListEntry {
                    path: "file.txt".to_string(),
                    size: 100,
                    mtime: 1234567890,
                    mode: 0o644,
                    flags: 0,
                    symlink_target: None,
                },
                FileListEntry {
                    path: "link".to_string(),
                    size: 0,
                    mtime: 1234567890,
                    mode: 0o777,
                    flags: FLAG_IS_SYMLINK,
                    symlink_target: Some("file.txt".to_string()),
                },
            ],
        };

        let mut buf = Vec::new();
        list.write(&mut buf).await.unwrap();

        let mut cursor = Cursor::new(&buf[5..]);
        let decoded = FileList::read(&mut cursor).await.unwrap();

        assert_eq!(decoded.entries.len(), 2);
        assert!(!decoded.entries[0].is_symlink());
        assert!(decoded.entries[1].is_symlink());
        assert_eq!(
            decoded.entries[1].symlink_target,
            Some("file.txt".to_string())
        );
    }

    #[tokio::test]
    async fn test_mkdir_batch_roundtrip() {
        let batch = MkdirBatch {
            paths: vec!["a/b/c".to_string(), "d/e".to_string()],
        };

        let mut buf = Vec::new();
        batch.write(&mut buf).await.unwrap();

        let mut cursor = Cursor::new(&buf[5..]);
        let decoded = MkdirBatch::read(&mut cursor).await.unwrap();

        assert_eq!(decoded.paths, batch.paths);
    }

    #[tokio::test]
    async fn test_symlink_batch_roundtrip() {
        let batch = SymlinkBatch {
            entries: vec![
                SymlinkEntry {
                    path: "link1".to_string(),
                    target: "target1".to_string(),
                },
                SymlinkEntry {
                    path: "link2".to_string(),
                    target: "../other/target".to_string(),
                },
            ],
        };

        let mut buf = Vec::new();
        batch.write(&mut buf).await.unwrap();

        let mut cursor = Cursor::new(&buf[5..]);
        let decoded = SymlinkBatch::read(&mut cursor).await.unwrap();

        assert_eq!(decoded.entries.len(), 2);
        assert_eq!(decoded.entries[0].path, "link1");
        assert_eq!(decoded.entries[0].target, "target1");
    }
}
