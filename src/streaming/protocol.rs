//! Protocol v2 message types for streaming sync.
//!
//! Clean break from v1 - no backward compatibility.
//! Unidirectional streaming with no ACKs in critical path.

use anyhow::{Context, Result};
use bytes::{Buf, BufMut, Bytes, BytesMut};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Protocol version 2 (streaming)
pub const PROTOCOL_VERSION: u16 = 2;

/// Protocol version 1 (request-response, legacy)
pub const PROTOCOL_VERSION_V1: u16 = 1;

/// Minimum supported protocol version
pub const PROTOCOL_VERSION_MIN: u16 = 2;

/// Maximum supported protocol version
pub const PROTOCOL_VERSION_MAX: u16 = 2;

/// Wire format: all multi-byte integers are big-endian
/// Strings are length-prefixed (u16 len + UTF-8)
/// Frame format: len:u32 | type:u8 | payload

// =============================================================================
// Message Types
// =============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MessageType {
    Hello = 0x01,
    FileEntry = 0x02,
    FileEnd = 0x03,
    DestFileEntry = 0x04,
    DestFileEnd = 0x05,
    Data = 0x06,
    DataEnd = 0x07,
    Delete = 0x08,
    DeleteEnd = 0x09,
    Mkdir = 0x0A,
    Symlink = 0x0B,
    Progress = 0x0C,
    Error = 0x0D,
    Fatal = 0x0E,
    Xattr = 0x0F,
    Done = 0x10,
}

impl MessageType {
    pub fn from_u8(b: u8) -> Option<Self> {
        match b {
            0x01 => Some(Self::Hello),
            0x02 => Some(Self::FileEntry),
            0x03 => Some(Self::FileEnd),
            0x04 => Some(Self::DestFileEntry),
            0x05 => Some(Self::DestFileEnd),
            0x06 => Some(Self::Data),
            0x07 => Some(Self::DataEnd),
            0x08 => Some(Self::Delete),
            0x09 => Some(Self::DeleteEnd),
            0x0A => Some(Self::Mkdir),
            0x0B => Some(Self::Symlink),
            0x0C => Some(Self::Progress),
            0x0D => Some(Self::Error),
            0x0E => Some(Self::Fatal),
            0x0F => Some(Self::Xattr),
            0x10 => Some(Self::Done),
            _ => None,
        }
    }
}

// =============================================================================
// Hello Flags
// =============================================================================

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct HelloFlags: u32 {
        const PULL = 1 << 0;
        const DELETE = 1 << 1;
        const CHECKSUM = 1 << 2;
        const COMPRESSION = 1 << 3;
        const XATTRS = 1 << 4;
        const ACLS = 1 << 5;
    }
}

// =============================================================================
// File Entry Flags
// =============================================================================

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct FileFlags: u8 {
        const DIR = 1 << 0;
        const SYMLINK = 1 << 1;
        const HARDLINK = 1 << 2;
        const HAS_XATTRS = 1 << 3;
        const SPARSE = 1 << 4;
    }
}

// =============================================================================
// Dest File Entry Flags
// =============================================================================

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct DestFileFlags: u8 {
        const DIR = 1 << 0;
        const HAS_CHECKSUMS = 1 << 1;
    }
}

// =============================================================================
// Data Flags
// =============================================================================

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct DataFlags: u8 {
        const COMPRESSED = 1 << 0;
        const DELTA = 1 << 1;
        const FINAL = 1 << 2;
    }
}

// =============================================================================
// Error Codes
// =============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum ErrorCode {
    IoError = 1,
    PermissionDenied = 2,
    NotFound = 3,
    ChecksumMismatch = 4,
    DiskFull = 5,
}

impl ErrorCode {
    pub fn from_u16(v: u16) -> Option<Self> {
        match v {
            1 => Some(Self::IoError),
            2 => Some(Self::PermissionDenied),
            3 => Some(Self::NotFound),
            4 => Some(Self::ChecksumMismatch),
            5 => Some(Self::DiskFull),
            _ => None,
        }
    }
}

// =============================================================================
// HELLO (0x01)
// =============================================================================

#[derive(Debug, Clone)]
pub struct Hello {
    pub version: u16,
    pub flags: HelloFlags,
    pub root_path: String,
}

impl Hello {
    pub fn new(flags: HelloFlags, root_path: impl Into<String>) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            flags,
            root_path: root_path.into(),
        }
    }

    pub fn is_pull(&self) -> bool {
        self.flags.contains(HelloFlags::PULL)
    }

    pub fn encode(&self) -> Bytes {
        let path_bytes = self.root_path.as_bytes();
        let payload_len = 2 + 4 + 2 + path_bytes.len();
        let mut buf = BytesMut::with_capacity(5 + payload_len);

        buf.put_u32(payload_len as u32);
        buf.put_u8(MessageType::Hello as u8);
        buf.put_u16(self.version);
        buf.put_u32(self.flags.bits());
        buf.put_u16(path_bytes.len() as u16);
        buf.put_slice(path_bytes);

        buf.freeze()
    }

    pub fn decode(mut payload: Bytes) -> Result<Self> {
        if payload.remaining() < 8 {
            anyhow::bail!("Hello payload too short");
        }
        let version = payload.get_u16();
        let flags = HelloFlags::from_bits_truncate(payload.get_u32());
        let path_len = payload.get_u16() as usize;
        if payload.remaining() < path_len {
            anyhow::bail!("Hello path truncated");
        }
        let root_path = String::from_utf8(payload.copy_to_bytes(path_len).to_vec())
            .context("Invalid UTF-8 in Hello path")?;

        Ok(Self {
            version,
            flags,
            root_path,
        })
    }
}

// =============================================================================
// FILE_ENTRY (0x02)
// =============================================================================

#[derive(Debug, Clone)]
pub struct FileEntry {
    pub path: String,
    pub size: u64,
    pub mtime: i64,
    pub mode: u32,
    pub inode: u64,
    pub flags: FileFlags,
    pub symlink_target: Option<String>,
    pub link_target: Option<String>,
}

impl FileEntry {
    pub fn is_dir(&self) -> bool {
        self.flags.contains(FileFlags::DIR)
    }

    pub fn is_symlink(&self) -> bool {
        self.flags.contains(FileFlags::SYMLINK)
    }

    pub fn is_hardlink(&self) -> bool {
        self.flags.contains(FileFlags::HARDLINK)
    }

    pub fn encode(&self) -> Bytes {
        let path_bytes = self.path.as_bytes();
        let symlink_bytes = self.symlink_target.as_ref().map(|s| s.as_bytes());
        let link_bytes = self.link_target.as_ref().map(|s| s.as_bytes());

        let mut payload_len = 2 + path_bytes.len() + 8 + 8 + 4 + 8 + 1;
        if let Some(b) = symlink_bytes {
            payload_len += 2 + b.len();
        }
        if let Some(b) = link_bytes {
            payload_len += 2 + b.len();
        }

        let mut buf = BytesMut::with_capacity(5 + payload_len);
        buf.put_u32(payload_len as u32);
        buf.put_u8(MessageType::FileEntry as u8);
        buf.put_u16(path_bytes.len() as u16);
        buf.put_slice(path_bytes);
        buf.put_u64(self.size);
        buf.put_i64(self.mtime);
        buf.put_u32(self.mode);
        buf.put_u64(self.inode);
        buf.put_u8(self.flags.bits());

        if let Some(b) = symlink_bytes {
            buf.put_u16(b.len() as u16);
            buf.put_slice(b);
        }
        if let Some(b) = link_bytes {
            buf.put_u16(b.len() as u16);
            buf.put_slice(b);
        }

        buf.freeze()
    }

    pub fn decode(mut payload: Bytes) -> Result<Self> {
        if payload.remaining() < 2 {
            anyhow::bail!("FileEntry payload too short");
        }
        let path_len = payload.get_u16() as usize;
        if payload.remaining() < path_len + 29 {
            anyhow::bail!("FileEntry payload truncated");
        }
        let path = String::from_utf8(payload.copy_to_bytes(path_len).to_vec())
            .context("Invalid UTF-8 in FileEntry path")?;
        let size = payload.get_u64();
        let mtime = payload.get_i64();
        let mode = payload.get_u32();
        let inode = payload.get_u64();
        let flags = FileFlags::from_bits_truncate(payload.get_u8());

        let symlink_target = if flags.contains(FileFlags::SYMLINK) {
            if payload.remaining() < 2 {
                anyhow::bail!("FileEntry symlink target length truncated");
            }
            let len = payload.get_u16() as usize;
            if payload.remaining() < len {
                anyhow::bail!(
                    "FileEntry symlink target truncated: expected {} bytes, got {}",
                    len,
                    payload.remaining()
                );
            }
            Some(
                String::from_utf8(payload.copy_to_bytes(len).to_vec())
                    .context("Invalid UTF-8 in symlink target")?,
            )
        } else {
            None
        };

        let link_target = if flags.contains(FileFlags::HARDLINK) {
            if payload.remaining() < 2 {
                anyhow::bail!("FileEntry hardlink target length truncated");
            }
            let len = payload.get_u16() as usize;
            if payload.remaining() < len {
                anyhow::bail!(
                    "FileEntry hardlink target truncated: expected {} bytes, got {}",
                    len,
                    payload.remaining()
                );
            }
            Some(
                String::from_utf8(payload.copy_to_bytes(len).to_vec())
                    .context("Invalid UTF-8 in link target")?,
            )
        } else {
            None
        };

        Ok(Self {
            path,
            size,
            mtime,
            mode,
            inode,
            flags,
            symlink_target,
            link_target,
        })
    }
}

// =============================================================================
// FILE_END (0x03)
// =============================================================================

#[derive(Debug, Clone, Copy)]
pub struct FileEnd {
    pub total_files: u64,
    pub total_bytes: u64,
}

impl FileEnd {
    pub fn encode(&self) -> Bytes {
        let mut buf = BytesMut::with_capacity(5 + 16);
        buf.put_u32(16);
        buf.put_u8(MessageType::FileEnd as u8);
        buf.put_u64(self.total_files);
        buf.put_u64(self.total_bytes);
        buf.freeze()
    }

    pub fn decode(mut payload: Bytes) -> Result<Self> {
        if payload.remaining() < 16 {
            anyhow::bail!("FileEnd payload too short");
        }
        Ok(Self {
            total_files: payload.get_u64(),
            total_bytes: payload.get_u64(),
        })
    }
}

// =============================================================================
// DEST_FILE_ENTRY (0x04)
// =============================================================================

#[derive(Debug, Clone)]
pub struct BlockChecksum {
    pub offset: u64,
    pub weak: u32,
    pub strong: u64,
}

impl BlockChecksum {
    pub const SIZE: usize = 20;
}

#[derive(Debug, Clone)]
pub struct DestFileEntry {
    pub path: String,
    pub size: u64,
    pub mtime: i64,
    pub mode: u32,
    pub flags: DestFileFlags,
    pub block_size: u32,
    pub checksums: Vec<BlockChecksum>,
}

impl DestFileEntry {
    pub fn encode(&self) -> Bytes {
        let path_bytes = self.path.as_bytes();
        let has_checksums = self.flags.contains(DestFileFlags::HAS_CHECKSUMS);

        let mut payload_len = 2 + path_bytes.len() + 8 + 8 + 4 + 1;
        if has_checksums {
            payload_len += 4 + 4 + self.checksums.len() * BlockChecksum::SIZE;
        }

        let mut buf = BytesMut::with_capacity(5 + payload_len);
        buf.put_u32(payload_len as u32);
        buf.put_u8(MessageType::DestFileEntry as u8);
        buf.put_u16(path_bytes.len() as u16);
        buf.put_slice(path_bytes);
        buf.put_u64(self.size);
        buf.put_i64(self.mtime);
        buf.put_u32(self.mode);
        buf.put_u8(self.flags.bits());

        if has_checksums {
            buf.put_u32(self.block_size);
            buf.put_u32(self.checksums.len() as u32);
            for cs in &self.checksums {
                buf.put_u64(cs.offset);
                buf.put_u32(cs.weak);
                buf.put_u64(cs.strong);
            }
        }

        buf.freeze()
    }

    pub fn decode(mut payload: Bytes) -> Result<Self> {
        if payload.remaining() < 2 {
            anyhow::bail!("DestFileEntry payload too short");
        }
        let path_len = payload.get_u16() as usize;
        if payload.remaining() < path_len + 21 {
            anyhow::bail!("DestFileEntry payload truncated");
        }
        let path = String::from_utf8(payload.copy_to_bytes(path_len).to_vec())
            .context("Invalid UTF-8 in DestFileEntry path")?;
        let size = payload.get_u64();
        let mtime = payload.get_i64();
        let mode = payload.get_u32();
        let flags = DestFileFlags::from_bits_truncate(payload.get_u8());

        let (block_size, checksums) = if flags.contains(DestFileFlags::HAS_CHECKSUMS) {
            if payload.remaining() < 8 {
                anyhow::bail!("DestFileEntry checksum header truncated");
            }
            let bs = payload.get_u32();
            let count = payload.get_u32() as usize;

            // Validate we have enough data for all checksums
            let required = count * BlockChecksum::SIZE;
            if payload.remaining() < required {
                anyhow::bail!(
                        "DestFileEntry checksums truncated: expected {} checksums ({} bytes), got {} bytes",
                        count,
                        required,
                        payload.remaining()
                    );
            }

            let mut checksums = Vec::with_capacity(count);
            for _ in 0..count {
                checksums.push(BlockChecksum {
                    offset: payload.get_u64(),
                    weak: payload.get_u32(),
                    strong: payload.get_u64(),
                });
            }
            (bs, checksums)
        } else {
            (0, Vec::new())
        };

        Ok(Self {
            path,
            size,
            mtime,
            mode,
            flags,
            block_size,
            checksums,
        })
    }
}

// =============================================================================
// DEST_FILE_END (0x05)
// =============================================================================

#[derive(Debug, Clone, Copy)]
pub struct DestFileEnd {
    pub total_files: u64,
    pub total_bytes: u64,
}

impl DestFileEnd {
    pub fn encode(&self) -> Bytes {
        let mut buf = BytesMut::with_capacity(5 + 16);
        buf.put_u32(16);
        buf.put_u8(MessageType::DestFileEnd as u8);
        buf.put_u64(self.total_files);
        buf.put_u64(self.total_bytes);
        buf.freeze()
    }

    pub fn decode(mut payload: Bytes) -> Result<Self> {
        if payload.remaining() < 16 {
            anyhow::bail!("DestFileEnd payload too short");
        }
        Ok(Self {
            total_files: payload.get_u64(),
            total_bytes: payload.get_u64(),
        })
    }
}

// =============================================================================
// DATA (0x06)
// =============================================================================

#[derive(Debug, Clone)]
pub struct Data {
    pub path: String,
    pub offset: u64,
    pub flags: DataFlags,
    pub data: Bytes,
}

impl Data {
    pub fn encode(&self) -> Bytes {
        let path_bytes = self.path.as_bytes();
        let payload_len = 2 + path_bytes.len() + 8 + 1 + 4 + self.data.len();

        let mut buf = BytesMut::with_capacity(5 + payload_len);
        buf.put_u32(payload_len as u32);
        buf.put_u8(MessageType::Data as u8);
        buf.put_u16(path_bytes.len() as u16);
        buf.put_slice(path_bytes);
        buf.put_u64(self.offset);
        buf.put_u8(self.flags.bits());
        buf.put_u32(self.data.len() as u32);
        buf.put_slice(&self.data);

        buf.freeze()
    }

    pub fn decode(mut payload: Bytes) -> Result<Self> {
        if payload.remaining() < 2 {
            anyhow::bail!("Data payload too short");
        }
        let path_len = payload.get_u16() as usize;
        if payload.remaining() < path_len + 13 {
            anyhow::bail!("Data payload truncated");
        }
        let path = String::from_utf8(payload.copy_to_bytes(path_len).to_vec())
            .context("Invalid UTF-8 in Data path")?;
        let offset = payload.get_u64();
        let flags = DataFlags::from_bits_truncate(payload.get_u8());
        let data_len = payload.get_u32() as usize;
        if payload.remaining() < data_len {
            anyhow::bail!("Data content truncated");
        }
        let data = payload.copy_to_bytes(data_len);

        Ok(Self {
            path,
            offset,
            flags,
            data,
        })
    }
}

// =============================================================================
// DATA_END (0x07)
// =============================================================================

#[derive(Debug, Clone)]
pub struct DataEnd {
    pub path: String,
    pub status: u8,
}

impl DataEnd {
    pub const STATUS_OK: u8 = 0;
    pub const STATUS_ERROR: u8 = 1;

    pub fn encode(&self) -> Bytes {
        let path_bytes = self.path.as_bytes();
        let payload_len = 2 + path_bytes.len() + 1;

        let mut buf = BytesMut::with_capacity(5 + payload_len);
        buf.put_u32(payload_len as u32);
        buf.put_u8(MessageType::DataEnd as u8);
        buf.put_u16(path_bytes.len() as u16);
        buf.put_slice(path_bytes);
        buf.put_u8(self.status);

        buf.freeze()
    }

    pub fn decode(mut payload: Bytes) -> Result<Self> {
        if payload.remaining() < 2 {
            anyhow::bail!("DataEnd payload too short");
        }
        let path_len = payload.get_u16() as usize;
        if payload.remaining() < path_len + 1 {
            anyhow::bail!("DataEnd payload truncated");
        }
        let path = String::from_utf8(payload.copy_to_bytes(path_len).to_vec())
            .context("Invalid UTF-8 in DataEnd path")?;
        let status = payload.get_u8();

        Ok(Self { path, status })
    }
}

// =============================================================================
// DELETE (0x08)
// =============================================================================

#[derive(Debug, Clone)]
pub struct Delete {
    pub path: String,
    pub is_dir: bool,
}

impl Delete {
    pub fn encode(&self) -> Bytes {
        let path_bytes = self.path.as_bytes();
        let payload_len = 2 + path_bytes.len() + 1;

        let mut buf = BytesMut::with_capacity(5 + payload_len);
        buf.put_u32(payload_len as u32);
        buf.put_u8(MessageType::Delete as u8);
        buf.put_u16(path_bytes.len() as u16);
        buf.put_slice(path_bytes);
        buf.put_u8(self.is_dir as u8);

        buf.freeze()
    }

    pub fn decode(mut payload: Bytes) -> Result<Self> {
        if payload.remaining() < 2 {
            anyhow::bail!("Delete payload too short");
        }
        let path_len = payload.get_u16() as usize;
        if payload.remaining() < path_len + 1 {
            anyhow::bail!("Delete payload truncated");
        }
        let path = String::from_utf8(payload.copy_to_bytes(path_len).to_vec())
            .context("Invalid UTF-8 in Delete path")?;
        let is_dir = payload.get_u8() != 0;

        Ok(Self { path, is_dir })
    }
}

// =============================================================================
// DELETE_END (0x09)
// =============================================================================

#[derive(Debug, Clone, Copy)]
pub struct DeleteEnd {
    pub count: u64,
}

impl DeleteEnd {
    pub fn encode(&self) -> Bytes {
        let mut buf = BytesMut::with_capacity(5 + 8);
        buf.put_u32(8);
        buf.put_u8(MessageType::DeleteEnd as u8);
        buf.put_u64(self.count);
        buf.freeze()
    }

    pub fn decode(mut payload: Bytes) -> Result<Self> {
        if payload.remaining() < 8 {
            anyhow::bail!("DeleteEnd payload too short");
        }
        Ok(Self {
            count: payload.get_u64(),
        })
    }
}

// =============================================================================
// MKDIR (0x0A)
// =============================================================================

#[derive(Debug, Clone)]
pub struct Mkdir {
    pub path: String,
    pub mode: u32,
}

impl Mkdir {
    pub fn encode(&self) -> Bytes {
        let path_bytes = self.path.as_bytes();
        let payload_len = 2 + path_bytes.len() + 4;

        let mut buf = BytesMut::with_capacity(5 + payload_len);
        buf.put_u32(payload_len as u32);
        buf.put_u8(MessageType::Mkdir as u8);
        buf.put_u16(path_bytes.len() as u16);
        buf.put_slice(path_bytes);
        buf.put_u32(self.mode);

        buf.freeze()
    }

    pub fn decode(mut payload: Bytes) -> Result<Self> {
        if payload.remaining() < 2 {
            anyhow::bail!("Mkdir payload too short");
        }
        let path_len = payload.get_u16() as usize;
        if payload.remaining() < path_len + 4 {
            anyhow::bail!("Mkdir payload truncated");
        }
        let path = String::from_utf8(payload.copy_to_bytes(path_len).to_vec())
            .context("Invalid UTF-8 in Mkdir path")?;
        let mode = payload.get_u32();

        Ok(Self { path, mode })
    }
}

// =============================================================================
// SYMLINK (0x0B)
// =============================================================================

#[derive(Debug, Clone)]
pub struct Symlink {
    pub path: String,
    pub target: String,
}

impl Symlink {
    pub fn encode(&self) -> Bytes {
        let path_bytes = self.path.as_bytes();
        let target_bytes = self.target.as_bytes();
        let payload_len = 2 + path_bytes.len() + 2 + target_bytes.len();

        let mut buf = BytesMut::with_capacity(5 + payload_len);
        buf.put_u32(payload_len as u32);
        buf.put_u8(MessageType::Symlink as u8);
        buf.put_u16(path_bytes.len() as u16);
        buf.put_slice(path_bytes);
        buf.put_u16(target_bytes.len() as u16);
        buf.put_slice(target_bytes);

        buf.freeze()
    }

    pub fn decode(mut payload: Bytes) -> Result<Self> {
        if payload.remaining() < 2 {
            anyhow::bail!("Symlink payload too short");
        }
        let path_len = payload.get_u16() as usize;
        if payload.remaining() < path_len + 2 {
            anyhow::bail!("Symlink payload truncated");
        }
        let path = String::from_utf8(payload.copy_to_bytes(path_len).to_vec())
            .context("Invalid UTF-8 in Symlink path")?;
        let target_len = payload.get_u16() as usize;
        if payload.remaining() < target_len {
            anyhow::bail!("Symlink target truncated");
        }
        let target = String::from_utf8(payload.copy_to_bytes(target_len).to_vec())
            .context("Invalid UTF-8 in Symlink target")?;

        Ok(Self { path, target })
    }
}

// =============================================================================
// PROGRESS (0x0C)
// =============================================================================

#[derive(Debug, Clone, Copy)]
pub struct Progress {
    pub files: u64,
    pub bytes: u64,
    pub files_total: u64,
    pub bytes_total: u64,
}

impl Progress {
    pub fn encode(&self) -> Bytes {
        let mut buf = BytesMut::with_capacity(5 + 32);
        buf.put_u32(32);
        buf.put_u8(MessageType::Progress as u8);
        buf.put_u64(self.files);
        buf.put_u64(self.bytes);
        buf.put_u64(self.files_total);
        buf.put_u64(self.bytes_total);
        buf.freeze()
    }

    pub fn decode(mut payload: Bytes) -> Result<Self> {
        if payload.remaining() < 32 {
            anyhow::bail!("Progress payload too short");
        }
        Ok(Self {
            files: payload.get_u64(),
            bytes: payload.get_u64(),
            files_total: payload.get_u64(),
            bytes_total: payload.get_u64(),
        })
    }
}

// =============================================================================
// ERROR (0x0D)
// =============================================================================

#[derive(Debug, Clone)]
pub struct Error {
    pub path: String,
    pub code: u16,
    pub message: String,
}

impl Error {
    pub fn encode(&self) -> Bytes {
        let path_bytes = self.path.as_bytes();
        let msg_bytes = self.message.as_bytes();
        let payload_len = 2 + path_bytes.len() + 2 + 2 + msg_bytes.len();

        let mut buf = BytesMut::with_capacity(5 + payload_len);
        buf.put_u32(payload_len as u32);
        buf.put_u8(MessageType::Error as u8);
        buf.put_u16(path_bytes.len() as u16);
        buf.put_slice(path_bytes);
        buf.put_u16(self.code);
        buf.put_u16(msg_bytes.len() as u16);
        buf.put_slice(msg_bytes);

        buf.freeze()
    }

    pub fn decode(mut payload: Bytes) -> Result<Self> {
        if payload.remaining() < 2 {
            anyhow::bail!("Error payload too short");
        }
        let path_len = payload.get_u16() as usize;
        if payload.remaining() < path_len + 4 {
            anyhow::bail!("Error payload truncated");
        }
        let path = String::from_utf8(payload.copy_to_bytes(path_len).to_vec())
            .context("Invalid UTF-8 in Error path")?;
        let code = payload.get_u16();
        let msg_len = payload.get_u16() as usize;
        if payload.remaining() < msg_len {
            anyhow::bail!("Error message truncated");
        }
        let message = String::from_utf8(payload.copy_to_bytes(msg_len).to_vec())
            .context("Invalid UTF-8 in Error message")?;

        Ok(Self {
            path,
            code,
            message,
        })
    }
}

// =============================================================================
// FATAL (0x0E)
// =============================================================================

#[derive(Debug, Clone)]
pub struct Fatal {
    pub code: u16,
    pub message: String,
}

impl Fatal {
    pub fn encode(&self) -> Bytes {
        let msg_bytes = self.message.as_bytes();
        let payload_len = 2 + 2 + msg_bytes.len();

        let mut buf = BytesMut::with_capacity(5 + payload_len);
        buf.put_u32(payload_len as u32);
        buf.put_u8(MessageType::Fatal as u8);
        buf.put_u16(self.code);
        buf.put_u16(msg_bytes.len() as u16);
        buf.put_slice(msg_bytes);

        buf.freeze()
    }

    pub fn decode(mut payload: Bytes) -> Result<Self> {
        if payload.remaining() < 4 {
            anyhow::bail!("Fatal payload too short");
        }
        let code = payload.get_u16();
        let msg_len = payload.get_u16() as usize;
        if payload.remaining() < msg_len {
            anyhow::bail!("Fatal message truncated");
        }
        let message = String::from_utf8(payload.copy_to_bytes(msg_len).to_vec())
            .context("Invalid UTF-8 in Fatal message")?;

        Ok(Self { code, message })
    }
}

// =============================================================================
// XATTR (0x0F)
// =============================================================================

#[derive(Debug, Clone)]
pub struct XattrEntry {
    pub name: String,
    pub value: Bytes,
}

#[derive(Debug, Clone)]
pub struct Xattr {
    pub path: String,
    pub entries: Vec<XattrEntry>,
}

impl Xattr {
    pub fn encode(&self) -> Bytes {
        let path_bytes = self.path.as_bytes();
        let mut payload_len = 2 + path_bytes.len() + 2;
        for entry in &self.entries {
            payload_len += 2 + entry.name.len() + 4 + entry.value.len();
        }

        let mut buf = BytesMut::with_capacity(5 + payload_len);
        buf.put_u32(payload_len as u32);
        buf.put_u8(MessageType::Xattr as u8);
        buf.put_u16(path_bytes.len() as u16);
        buf.put_slice(path_bytes);
        buf.put_u16(self.entries.len() as u16);

        for entry in &self.entries {
            let name_bytes = entry.name.as_bytes();
            buf.put_u16(name_bytes.len() as u16);
            buf.put_slice(name_bytes);
            buf.put_u32(entry.value.len() as u32);
            buf.put_slice(&entry.value);
        }

        buf.freeze()
    }

    pub fn decode(mut payload: Bytes) -> Result<Self> {
        if payload.remaining() < 2 {
            anyhow::bail!("Xattr payload too short");
        }
        let path_len = payload.get_u16() as usize;
        if payload.remaining() < path_len + 2 {
            anyhow::bail!("Xattr payload truncated");
        }
        let path = String::from_utf8(payload.copy_to_bytes(path_len).to_vec())
            .context("Invalid UTF-8 in Xattr path")?;
        let count = payload.get_u16() as usize;

        let mut entries = Vec::with_capacity(count);
        for i in 0..count {
            if payload.remaining() < 2 {
                anyhow::bail!(
                    "Xattr entry {} name length truncated: expected 2 bytes, got {}",
                    i,
                    payload.remaining()
                );
            }
            let name_len = payload.get_u16() as usize;
            if payload.remaining() < name_len + 4 {
                anyhow::bail!(
                    "Xattr entry {} truncated: expected {} bytes for name + value length, got {}",
                    i,
                    name_len + 4,
                    payload.remaining()
                );
            }
            let name = String::from_utf8(payload.copy_to_bytes(name_len).to_vec())
                .context("Invalid UTF-8 in Xattr name")?;
            let value_len = payload.get_u32() as usize;
            if payload.remaining() < value_len {
                anyhow::bail!(
                    "Xattr entry {} value truncated: expected {} bytes, got {}",
                    i,
                    value_len,
                    payload.remaining()
                );
            }
            let value = payload.copy_to_bytes(value_len);
            entries.push(XattrEntry { name, value });
        }

        Ok(Self { path, entries })
    }
}

// =============================================================================
// DONE (0x10)
// =============================================================================

#[derive(Debug, Clone, Copy)]
pub struct Done {
    pub files_ok: u64,
    pub files_err: u64,
    pub bytes: u64,
    pub duration_ms: u64,
}

impl Done {
    pub fn encode(&self) -> Bytes {
        let mut buf = BytesMut::with_capacity(5 + 32);
        buf.put_u32(32);
        buf.put_u8(MessageType::Done as u8);
        buf.put_u64(self.files_ok);
        buf.put_u64(self.files_err);
        buf.put_u64(self.bytes);
        buf.put_u64(self.duration_ms);
        buf.freeze()
    }

    pub fn decode(mut payload: Bytes) -> Result<Self> {
        if payload.remaining() < 32 {
            anyhow::bail!("Done payload too short");
        }
        Ok(Self {
            files_ok: payload.get_u64(),
            files_err: payload.get_u64(),
            bytes: payload.get_u64(),
            duration_ms: payload.get_u64(),
        })
    }
}

// =============================================================================
// Frame reading/writing
// =============================================================================

/// Maximum frame size (64MB) - prevents OOM from malicious/corrupted frames
pub const MAX_FRAME_SIZE: u32 = 64 * 1024 * 1024;

/// Read a single frame from the stream.
/// Returns (message_type, payload).
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<(MessageType, Bytes)> {
    let len = r.read_u32().await.context("Failed to read frame length")?;

    // Validate frame size before allocation
    if len > MAX_FRAME_SIZE {
        anyhow::bail!(
            "Frame size {} exceeds maximum allowed size {}",
            len,
            MAX_FRAME_SIZE
        );
    }

    let msg_type = r.read_u8().await.context("Failed to read message type")?;
    let msg_type = MessageType::from_u8(msg_type).context("Unknown message type")?;

    let payload_len = len as usize;
    let mut payload = vec![0u8; payload_len];
    r.read_exact(&mut payload)
        .await
        .context("Failed to read frame payload")?;

    Ok((msg_type, Bytes::from(payload)))
}

/// Write a pre-encoded frame to the stream.
pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, frame: &Bytes) -> Result<()> {
    w.write_all(frame).await.context("Failed to write frame")?;
    Ok(())
}

// =============================================================================
// Version Negotiation
// =============================================================================

/// Result of version negotiation
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionNegotiationResult {
    /// Version is supported
    Supported(u16),
    /// Version is too old (client needs upgrade)
    TooOld { client: u16, min_supported: u16 },
    /// Version is too new (server needs upgrade)
    TooNew { client: u16, max_supported: u16 },
}

/// Check if a client protocol version is supported.
pub fn negotiate_version(client_version: u16) -> VersionNegotiationResult {
    if client_version < PROTOCOL_VERSION_MIN {
        VersionNegotiationResult::TooOld {
            client: client_version,
            min_supported: PROTOCOL_VERSION_MIN,
        }
    } else if client_version > PROTOCOL_VERSION_MAX {
        VersionNegotiationResult::TooNew {
            client: client_version,
            max_supported: PROTOCOL_VERSION_MAX,
        }
    } else {
        VersionNegotiationResult::Supported(client_version)
    }
}

/// Check if a protocol version indicates v2 streaming protocol.
pub fn is_streaming_protocol(version: u16) -> bool {
    version >= 2
}

/// Check if a protocol version indicates v1 request-response protocol.
pub fn is_legacy_protocol(version: u16) -> bool {
    version == 1
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hello_roundtrip() {
        let hello = Hello::new(HelloFlags::PULL | HelloFlags::DELETE, "/tmp/dest");
        let encoded = hello.encode();

        // Skip frame header (4 bytes len + 1 byte type)
        let payload = Bytes::copy_from_slice(&encoded[5..]);
        let decoded = Hello::decode(payload).unwrap();

        assert_eq!(decoded.version, PROTOCOL_VERSION);
        assert!(decoded.is_pull());
        assert!(decoded.flags.contains(HelloFlags::DELETE));
        assert_eq!(decoded.root_path, "/tmp/dest");
    }

    #[test]
    fn test_file_entry_roundtrip() {
        let entry = FileEntry {
            path: "test/file.txt".to_string(),
            size: 1024,
            mtime: 1234567890,
            mode: 0o644,
            inode: 12345,
            flags: FileFlags::empty(),
            symlink_target: None,
            link_target: None,
        };
        let encoded = entry.encode();
        let payload = Bytes::copy_from_slice(&encoded[5..]);
        let decoded = FileEntry::decode(payload).unwrap();

        assert_eq!(decoded.path, "test/file.txt");
        assert_eq!(decoded.size, 1024);
        assert_eq!(decoded.mtime, 1234567890);
        assert_eq!(decoded.mode, 0o644);
        assert_eq!(decoded.inode, 12345);
    }

    #[test]
    fn test_file_entry_symlink() {
        let entry = FileEntry {
            path: "link".to_string(),
            size: 0,
            mtime: 1234567890,
            mode: 0o777,
            inode: 0,
            flags: FileFlags::SYMLINK,
            symlink_target: Some("target.txt".to_string()),
            link_target: None,
        };
        let encoded = entry.encode();
        let payload = Bytes::copy_from_slice(&encoded[5..]);
        let decoded = FileEntry::decode(payload).unwrap();

        assert!(decoded.is_symlink());
        assert_eq!(decoded.symlink_target, Some("target.txt".to_string()));
    }

    #[test]
    fn test_file_entry_hardlink() {
        let entry = FileEntry {
            path: "hardlink".to_string(),
            size: 1024,
            mtime: 1234567890,
            mode: 0o644,
            inode: 12345,
            flags: FileFlags::HARDLINK,
            symlink_target: None,
            link_target: Some("original.txt".to_string()),
        };
        let encoded = entry.encode();
        let payload = Bytes::copy_from_slice(&encoded[5..]);
        let decoded = FileEntry::decode(payload).unwrap();

        assert!(decoded.is_hardlink());
        assert_eq!(decoded.link_target, Some("original.txt".to_string()));
    }

    #[test]
    fn test_dest_file_entry_with_checksums() {
        let entry = DestFileEntry {
            path: "large.bin".to_string(),
            size: 1024 * 1024,
            mtime: 1234567890,
            mode: 0o644,
            flags: DestFileFlags::HAS_CHECKSUMS,
            block_size: 4096,
            checksums: vec![
                BlockChecksum {
                    offset: 0,
                    weak: 0xDEADBEEF,
                    strong: 0x123456789ABCDEF0,
                },
                BlockChecksum {
                    offset: 4096,
                    weak: 0xCAFEBABE,
                    strong: 0x0FEDCBA987654321,
                },
            ],
        };
        let encoded = entry.encode();
        let payload = Bytes::copy_from_slice(&encoded[5..]);
        let decoded = DestFileEntry::decode(payload).unwrap();

        assert_eq!(decoded.path, "large.bin");
        assert!(decoded.flags.contains(DestFileFlags::HAS_CHECKSUMS));
        assert_eq!(decoded.block_size, 4096);
        assert_eq!(decoded.checksums.len(), 2);
        assert_eq!(decoded.checksums[0].weak, 0xDEADBEEF);
        assert_eq!(decoded.checksums[1].strong, 0x0FEDCBA987654321);
    }

    #[test]
    fn test_data_roundtrip() {
        let data = Data {
            path: "file.txt".to_string(),
            offset: 1024,
            flags: DataFlags::COMPRESSED,
            data: Bytes::from(vec![1, 2, 3, 4, 5]),
        };
        let encoded = data.encode();
        let payload = Bytes::copy_from_slice(&encoded[5..]);
        let decoded = Data::decode(payload).unwrap();

        assert_eq!(decoded.path, "file.txt");
        assert_eq!(decoded.offset, 1024);
        assert!(decoded.flags.contains(DataFlags::COMPRESSED));
        assert_eq!(decoded.data.as_ref(), &[1, 2, 3, 4, 5]);
    }

    #[test]
    fn test_progress_roundtrip() {
        let progress = Progress {
            files: 100,
            bytes: 1024 * 1024,
            files_total: 1000,
            bytes_total: 1024 * 1024 * 100,
        };
        let encoded = progress.encode();
        let payload = Bytes::copy_from_slice(&encoded[5..]);
        let decoded = Progress::decode(payload).unwrap();

        assert_eq!(decoded.files, 100);
        assert_eq!(decoded.bytes, 1024 * 1024);
        assert_eq!(decoded.files_total, 1000);
    }

    #[test]
    fn test_xattr_roundtrip() {
        let xattr = Xattr {
            path: "file.txt".to_string(),
            entries: vec![
                XattrEntry {
                    name: "user.comment".to_string(),
                    value: Bytes::from("test comment"),
                },
                XattrEntry {
                    name: "user.author".to_string(),
                    value: Bytes::from("test"),
                },
            ],
        };
        let encoded = xattr.encode();
        let payload = Bytes::copy_from_slice(&encoded[5..]);
        let decoded = Xattr::decode(payload).unwrap();

        assert_eq!(decoded.path, "file.txt");
        assert_eq!(decoded.entries.len(), 2);
        assert_eq!(decoded.entries[0].name, "user.comment");
    }

    #[test]
    fn test_done_roundtrip() {
        let done = Done {
            files_ok: 100,
            files_err: 2,
            bytes: 1024 * 1024 * 50,
            duration_ms: 5000,
        };
        let encoded = done.encode();
        let payload = Bytes::copy_from_slice(&encoded[5..]);
        let decoded = Done::decode(payload).unwrap();

        assert_eq!(decoded.files_ok, 100);
        assert_eq!(decoded.files_err, 2);
        assert_eq!(decoded.bytes, 1024 * 1024 * 50);
        assert_eq!(decoded.duration_ms, 5000);
    }

    #[test]
    fn test_message_type_from_u8() {
        assert_eq!(MessageType::from_u8(0x01), Some(MessageType::Hello));
        assert_eq!(MessageType::from_u8(0x06), Some(MessageType::Data));
        assert_eq!(MessageType::from_u8(0x10), Some(MessageType::Done));
        assert_eq!(MessageType::from_u8(0xFF), None);
    }

    #[test]
    fn test_version_negotiation_supported() {
        let result = negotiate_version(2);
        assert_eq!(result, VersionNegotiationResult::Supported(2));
    }

    #[test]
    fn test_version_negotiation_too_old() {
        let result = negotiate_version(1);
        match result {
            VersionNegotiationResult::TooOld {
                client,
                min_supported,
            } => {
                assert_eq!(client, 1);
                assert_eq!(min_supported, PROTOCOL_VERSION_MIN);
            }
            _ => panic!("Expected TooOld result"),
        }
    }

    #[test]
    fn test_version_negotiation_too_new() {
        let result = negotiate_version(99);
        match result {
            VersionNegotiationResult::TooNew {
                client,
                max_supported,
            } => {
                assert_eq!(client, 99);
                assert_eq!(max_supported, PROTOCOL_VERSION_MAX);
            }
            _ => panic!("Expected TooNew result"),
        }
    }

    #[test]
    fn test_is_streaming_protocol() {
        assert!(!is_streaming_protocol(1));
        assert!(is_streaming_protocol(2));
        assert!(is_streaming_protocol(3));
    }

    #[test]
    fn test_is_legacy_protocol() {
        assert!(is_legacy_protocol(1));
        assert!(!is_legacy_protocol(2));
        assert!(!is_legacy_protocol(0));
    }
}
