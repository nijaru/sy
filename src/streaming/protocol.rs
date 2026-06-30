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

fn u16_len(label: &str, len: usize) -> Result<u16> {
    u16::try_from(len).with_context(|| format!("{label} too long for protocol u16 length: {len}"))
}

fn u32_len(label: &str, len: usize) -> Result<u32> {
    u32::try_from(len).with_context(|| format!("{label} too long for protocol u32 length: {len}"))
}

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct HelloFlags: u32 {
        const PULL = 1 << 0;
        const DELETE = 1 << 1;
        const CHECKSUM = 1 << 2;
        const COMPRESSION = 1 << 3;
        const XATTRS = 1 << 4;
        const ACLS = 1 << 5;
        const DRY_RUN = 1 << 6;
        const FORCE_DELETE = 1 << 7;
        const RESPECT_GITIGNORE = 1 << 8;
        const EXCLUDE_GIT_DIR = 1 << 9;
        const DIRS_ONLY = 1 << 10;
        const VERIFY = 1 << 11;
    }
}

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

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct DestFileFlags: u8 {
        const DIR = 1 << 0;
        const HAS_CHECKSUMS = 1 << 1;
    }
}

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct DataFlags: u8 {
        const COMPRESSED = 1 << 0;
        const DELTA = 1 << 1;
        const FINAL = 1 << 2;
    }
}

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

#[derive(Debug, Clone)]
pub struct Hello {
    pub version: u16,
    pub flags: HelloFlags,
    pub root_path: String,
    /// Optional max-delete threshold propagated to the remote generator in PULL
    /// mode so the server enforces the client's `--max-delete`.
    pub max_delete: Option<String>,
    /// Optional filter patterns (rsync-style "- pattern" / "+ pattern" lines,
    /// newline-separated) propagated to the server in PULL mode.
    pub filter_patterns: Option<String>,
    /// Optional comparison flags (bitfield) propagated to the server.
    /// Bit 0: checksum, Bit 1: update_only, Bit 2: ignore_existing,
    /// Bit 3: ignore_times, Bit 4: size_only
    pub comparison_flags: Option<u8>,
}

impl Hello {
    pub fn new(flags: HelloFlags, root_path: impl Into<String>) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            flags,
            root_path: root_path.into(),
            max_delete: None,
            filter_patterns: None,
            comparison_flags: None,
        }
    }

    /// Attach the client's max-delete threshold to propagate to the server.
    pub fn with_max_delete(mut self, max_delete: Option<String>) -> Self {
        self.max_delete = max_delete;
        self
    }

    /// Attach the client's filter patterns to propagate to the server.
    pub fn with_filter_patterns(mut self, patterns: Option<String>) -> Self {
        self.filter_patterns = patterns;
        self
    }

    /// Attach comparison flags to propagate to the server.
    pub fn with_comparison_flags(mut self, flags: u8) -> Self {
        self.comparison_flags = Some(flags);
        self
    }

    /// Decode comparison_flags bitfield into (checksum, update_only, ignore_existing, ignore_times, size_only).
    pub fn comparison_flags_tuple(&self) -> (bool, bool, bool, bool, bool) {
        match self.comparison_flags {
            Some(f) => (
                f & 0x01 != 0,
                f & 0x02 != 0,
                f & 0x04 != 0,
                f & 0x08 != 0,
                f & 0x10 != 0,
            ),
            None => (false, false, false, false, false),
        }
    }

    pub fn is_pull(&self) -> bool {
        self.flags.contains(HelloFlags::PULL)
    }

    pub fn encode(&self) -> Result<Bytes> {
        let path_bytes = self.root_path.as_bytes();
        // Trailing optional fields: each is u8 present + (u16 len + bytes) when present.
        let max_len = self.max_delete.as_ref().map(|m| m.len()).unwrap_or(0);
        let max_field_len = 1 + if self.max_delete.is_some() {
            2 + max_len
        } else {
            0
        };
        let filter_len = self.filter_patterns.as_ref().map(|f| f.len()).unwrap_or(0);
        let filter_field_len = 1 + if self.filter_patterns.is_some() {
            2 + filter_len
        } else {
            0
        };
        // comparison_flags: 1 byte present flag + 1 byte value when present.
        let comp_field_len = if self.comparison_flags.is_some() {
            2
        } else {
            1
        };
        let payload_len =
            2 + 4 + 2 + path_bytes.len() + max_field_len + filter_field_len + comp_field_len;
        let mut buf = BytesMut::with_capacity(5 + payload_len);

        buf.put_u32(u32_len("payload", payload_len)?);
        buf.put_u8(MessageType::Hello as u8);
        buf.put_u16(self.version);
        buf.put_u32(self.flags.bits());
        buf.put_u16(u16_len("path", path_bytes.len())?);
        buf.put_slice(path_bytes);

        // Trailing max_delete: present flag + optional length-prefixed string.
        // Old peers ignore trailing bytes, so this is backwards compatible.
        if let Some(max_delete) = &self.max_delete {
            buf.put_u8(1);
            buf.put_u16(u16_len("max_delete", max_delete.len())?);
            buf.put_slice(max_delete.as_bytes());
        } else {
            buf.put_u8(0);
        }

        // Trailing filter_patterns: newline-separated rsync-style rules.
        if let Some(filter_patterns) = &self.filter_patterns {
            buf.put_u8(1);
            buf.put_u16(u16_len("filter_patterns", filter_patterns.len())?);
            buf.put_slice(filter_patterns.as_bytes());
        } else {
            buf.put_u8(0);
        }

        // Trailing comparison_flags: single u8 bitfield.
        if let Some(flags) = self.comparison_flags {
            buf.put_u8(1);
            buf.put_u8(flags);
        } else {
            buf.put_u8(0);
        }

        Ok(buf.freeze())
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

        // Optional trailing max_delete (absent on old peers → None).
        let max_delete = if payload.remaining() >= 1 {
            let present = payload.get_u8();
            if present == 1 && payload.remaining() >= 2 {
                let max_len = payload.get_u16() as usize;
                if payload.remaining() >= max_len {
                    Some(
                        String::from_utf8(payload.copy_to_bytes(max_len).to_vec())
                            .context("Invalid UTF-8 in Hello max_delete")?,
                    )
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };

        // Optional trailing filter_patterns (absent on old peers → None).
        let filter_patterns = if payload.remaining() >= 1 {
            let present = payload.get_u8();
            if present == 1 && payload.remaining() >= 2 {
                let filter_len = payload.get_u16() as usize;
                if payload.remaining() >= filter_len {
                    Some(
                        String::from_utf8(payload.copy_to_bytes(filter_len).to_vec())
                            .context("Invalid UTF-8 in Hello filter_patterns")?,
                    )
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };

        // Optional trailing comparison_flags (absent on old peers → None).
        let comparison_flags = if payload.remaining() >= 1 {
            let present = payload.get_u8();
            if present == 1 && payload.remaining() >= 1 {
                Some(payload.get_u8())
            } else {
                None
            }
        } else {
            None
        };

        Ok(Self {
            version,
            flags,
            root_path,
            max_delete,
            filter_patterns,
            comparison_flags,
        })
    }
}

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

    pub fn encode(&self) -> Result<Bytes> {
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
        buf.put_u32(u32_len("payload", payload_len)?);
        buf.put_u8(MessageType::FileEntry as u8);
        buf.put_u16(u16_len("path", path_bytes.len())?);
        buf.put_slice(path_bytes);
        buf.put_u64(self.size);
        buf.put_i64(self.mtime);
        buf.put_u32(self.mode);
        buf.put_u64(self.inode);
        buf.put_u8(self.flags.bits());

        if let Some(b) = symlink_bytes {
            buf.put_u16(u16_len("optional path", b.len())?);
            buf.put_slice(b);
        }
        if let Some(b) = link_bytes {
            buf.put_u16(u16_len("optional path", b.len())?);
            buf.put_slice(b);
        }

        Ok(buf.freeze())
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

// FILE_END (0x03)

#[derive(Debug, Clone, Copy)]
pub struct FileEnd {
    pub total_files: u64,
    pub total_bytes: u64,
}

impl FileEnd {
    pub fn encode(&self) -> Result<Bytes> {
        let mut buf = BytesMut::with_capacity(5 + 16);
        buf.put_u32(16);
        buf.put_u8(MessageType::FileEnd as u8);
        buf.put_u64(self.total_files);
        buf.put_u64(self.total_bytes);
        Ok(buf.freeze())
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

// DEST_FILE_ENTRY (0x04)

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
    pub fn encode(&self) -> Result<Bytes> {
        let path_bytes = self.path.as_bytes();
        let has_checksums = self.flags.contains(DestFileFlags::HAS_CHECKSUMS);

        let mut payload_len = 2 + path_bytes.len() + 8 + 8 + 4 + 1;
        if has_checksums {
            payload_len += 4 + 4 + self.checksums.len() * BlockChecksum::SIZE;
        }

        let mut buf = BytesMut::with_capacity(5 + payload_len);
        buf.put_u32(u32_len("payload", payload_len)?);
        buf.put_u8(MessageType::DestFileEntry as u8);
        buf.put_u16(u16_len("path", path_bytes.len())?);
        buf.put_slice(path_bytes);
        buf.put_u64(self.size);
        buf.put_i64(self.mtime);
        buf.put_u32(self.mode);
        buf.put_u8(self.flags.bits());

        if has_checksums {
            buf.put_u32(self.block_size);
            buf.put_u32(u32_len("checksums", self.checksums.len())?);
            for cs in &self.checksums {
                buf.put_u64(cs.offset);
                buf.put_u32(cs.weak);
                buf.put_u64(cs.strong);
            }
        }

        Ok(buf.freeze())
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

            // Validate we have enough data for all checksums BEFORE allocating
            // (prevents OOM from a malicious count value in a small frame)
            let required = match count.checked_mul(BlockChecksum::SIZE) {
                Some(r) => r,
                None => anyhow::bail!("DestFileEntry checksum count overflow"),
            };
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

// DEST_FILE_END (0x05)

#[derive(Debug, Clone, Copy)]
pub struct DestFileEnd {
    pub total_files: u64,
    pub total_bytes: u64,
}

impl DestFileEnd {
    pub fn encode(&self) -> Result<Bytes> {
        let mut buf = BytesMut::with_capacity(5 + 16);
        buf.put_u32(16);
        buf.put_u8(MessageType::DestFileEnd as u8);
        buf.put_u64(self.total_files);
        buf.put_u64(self.total_bytes);
        Ok(buf.freeze())
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

// DATA (0x06)

#[derive(Debug, Clone)]
pub struct Data {
    pub path: String,
    pub offset: u64,
    pub flags: DataFlags,
    pub data: Bytes,
}

impl Data {
    pub fn encode(&self) -> Result<Bytes> {
        let path_bytes = self.path.as_bytes();
        let payload_len = 2 + path_bytes.len() + 8 + 1 + 4 + self.data.len();

        let mut buf = BytesMut::with_capacity(5 + payload_len);
        buf.put_u32(u32_len("payload", payload_len)?);
        buf.put_u8(MessageType::Data as u8);
        buf.put_u16(u16_len("path", path_bytes.len())?);
        buf.put_slice(path_bytes);
        buf.put_u64(self.offset);
        buf.put_u8(self.flags.bits());
        buf.put_u32(u32_len("data", self.data.len())?);
        buf.put_slice(&self.data);

        Ok(buf.freeze())
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

// DATA_END (0x07)

#[derive(Debug, Clone)]
pub struct DataEnd {
    pub path: String,
    pub status: u8,
}

impl DataEnd {
    pub const STATUS_OK: u8 = 0;
    pub const STATUS_ERROR: u8 = 1;

    pub fn encode(&self) -> Result<Bytes> {
        let path_bytes = self.path.as_bytes();
        let payload_len = 2 + path_bytes.len() + 1;

        let mut buf = BytesMut::with_capacity(5 + payload_len);
        buf.put_u32(u32_len("payload", payload_len)?);
        buf.put_u8(MessageType::DataEnd as u8);
        buf.put_u16(u16_len("path", path_bytes.len())?);
        buf.put_slice(path_bytes);
        buf.put_u8(self.status);

        Ok(buf.freeze())
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

// DELETE (0x08)

#[derive(Debug, Clone)]
pub struct Delete {
    pub path: String,
    pub is_dir: bool,
}

impl Delete {
    pub fn encode(&self) -> Result<Bytes> {
        let path_bytes = self.path.as_bytes();
        let payload_len = 2 + path_bytes.len() + 1;

        let mut buf = BytesMut::with_capacity(5 + payload_len);
        buf.put_u32(u32_len("payload", payload_len)?);
        buf.put_u8(MessageType::Delete as u8);
        buf.put_u16(u16_len("path", path_bytes.len())?);
        buf.put_slice(path_bytes);
        buf.put_u8(self.is_dir as u8);

        Ok(buf.freeze())
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

// DELETE_END (0x09)

#[derive(Debug, Clone, Copy)]
pub struct DeleteEnd {
    pub count: u64,
}

impl DeleteEnd {
    pub fn encode(&self) -> Result<Bytes> {
        let mut buf = BytesMut::with_capacity(5 + 8);
        buf.put_u32(8);
        buf.put_u8(MessageType::DeleteEnd as u8);
        buf.put_u64(self.count);
        Ok(buf.freeze())
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

// MKDIR (0x0A)

#[derive(Debug, Clone)]
pub struct Mkdir {
    pub path: String,
    pub mode: u32,
}

impl Mkdir {
    pub fn encode(&self) -> Result<Bytes> {
        let path_bytes = self.path.as_bytes();
        let payload_len = 2 + path_bytes.len() + 4;

        let mut buf = BytesMut::with_capacity(5 + payload_len);
        buf.put_u32(u32_len("payload", payload_len)?);
        buf.put_u8(MessageType::Mkdir as u8);
        buf.put_u16(u16_len("path", path_bytes.len())?);
        buf.put_slice(path_bytes);
        buf.put_u32(self.mode);

        Ok(buf.freeze())
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

// SYMLINK (0x0B)

#[derive(Debug, Clone)]
pub struct Symlink {
    pub path: String,
    pub target: String,
}

impl Symlink {
    pub fn encode(&self) -> Result<Bytes> {
        let path_bytes = self.path.as_bytes();
        let target_bytes = self.target.as_bytes();
        let payload_len = 2 + path_bytes.len() + 2 + target_bytes.len();

        let mut buf = BytesMut::with_capacity(5 + payload_len);
        buf.put_u32(u32_len("payload", payload_len)?);
        buf.put_u8(MessageType::Symlink as u8);
        buf.put_u16(u16_len("path", path_bytes.len())?);
        buf.put_slice(path_bytes);
        buf.put_u16(u16_len("symlink target", target_bytes.len())?);
        buf.put_slice(target_bytes);

        Ok(buf.freeze())
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

// PROGRESS (0x0C)

#[derive(Debug, Clone, Copy)]
pub struct Progress {
    pub files: u64,
    pub bytes: u64,
    pub files_total: u64,
    pub bytes_total: u64,
}

impl Progress {
    pub fn encode(&self) -> Result<Bytes> {
        let mut buf = BytesMut::with_capacity(5 + 32);
        buf.put_u32(32);
        buf.put_u8(MessageType::Progress as u8);
        buf.put_u64(self.files);
        buf.put_u64(self.bytes);
        buf.put_u64(self.files_total);
        buf.put_u64(self.bytes_total);
        Ok(buf.freeze())
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

// ERROR (0x0D)

#[derive(Debug, Clone)]
pub struct Error {
    pub path: String,
    pub code: u16,
    pub message: String,
}

impl Error {
    pub fn encode(&self) -> Result<Bytes> {
        let path_bytes = self.path.as_bytes();
        let msg_bytes = self.message.as_bytes();
        let payload_len = 2 + path_bytes.len() + 2 + 2 + msg_bytes.len();

        let mut buf = BytesMut::with_capacity(5 + payload_len);
        buf.put_u32(u32_len("payload", payload_len)?);
        buf.put_u8(MessageType::Error as u8);
        buf.put_u16(u16_len("path", path_bytes.len())?);
        buf.put_slice(path_bytes);
        buf.put_u16(self.code);
        buf.put_u16(u16_len("message", msg_bytes.len())?);
        buf.put_slice(msg_bytes);

        Ok(buf.freeze())
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

// FATAL (0x0E)

#[derive(Debug, Clone)]
pub struct Fatal {
    pub code: u16,
    pub message: String,
}

impl Fatal {
    pub fn encode(&self) -> Result<Bytes> {
        let msg_bytes = self.message.as_bytes();
        let payload_len = 2 + 2 + msg_bytes.len();

        let mut buf = BytesMut::with_capacity(5 + payload_len);
        buf.put_u32(u32_len("payload", payload_len)?);
        buf.put_u8(MessageType::Fatal as u8);
        buf.put_u16(self.code);
        buf.put_u16(u16_len("message", msg_bytes.len())?);
        buf.put_slice(msg_bytes);

        Ok(buf.freeze())
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

// XATTR (0x0F)

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
    pub fn encode(&self) -> Result<Bytes> {
        let path_bytes = self.path.as_bytes();
        let mut payload_len = 2 + path_bytes.len() + 2;
        for entry in &self.entries {
            payload_len += 2 + entry.name.len() + 4 + entry.value.len();
        }

        let mut buf = BytesMut::with_capacity(5 + payload_len);
        buf.put_u32(u32_len("payload", payload_len)?);
        buf.put_u8(MessageType::Xattr as u8);
        buf.put_u16(u16_len("path", path_bytes.len())?);
        buf.put_slice(path_bytes);
        buf.put_u16(u16_len("xattr entries", self.entries.len())?);

        for entry in &self.entries {
            let name_bytes = entry.name.as_bytes();
            buf.put_u16(u16_len("xattr name", name_bytes.len())?);
            buf.put_slice(name_bytes);
            buf.put_u32(u32_len("xattr value", entry.value.len())?);
            buf.put_slice(&entry.value);
        }

        Ok(buf.freeze())
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

        // Validate minimum entry size before allocating
        // (prevents unnecessary allocation from a malicious count value)
        const MIN_XATTR_ENTRY_SIZE: usize = 2 + 4; // u16 name_len + u32 value_len
        if payload.remaining() < count * MIN_XATTR_ENTRY_SIZE {
            anyhow::bail!(
                "Xattr entries truncated: expected at least {} bytes for {} entries, got {}",
                count * MIN_XATTR_ENTRY_SIZE,
                count,
                payload.remaining()
            );
        }

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

// DONE (0x10)

#[derive(Debug, Clone, Copy)]
pub struct Done {
    pub files_ok: u64,
    pub files_err: u64,
    pub bytes: u64,
    pub duration_ms: u64,
    /// Total source entries scanned (including skipped/filtered). Optional trailing
    /// field for backwards compat — old peers return 0.
    pub files_scanned: u64,
}

impl Done {
    pub fn encode(&self) -> Result<Bytes> {
        // Fixed: 4×u64 = 32 bytes. Optional trailing: 1 + 8 = 9 bytes when present.
        let trailing_len = if self.files_scanned > 0 { 9 } else { 1 };
        let mut buf = BytesMut::with_capacity(5 + 32 + trailing_len);
        buf.put_u32(32 + trailing_len as u32);
        buf.put_u8(MessageType::Done as u8);
        buf.put_u64(self.files_ok);
        buf.put_u64(self.files_err);
        buf.put_u64(self.bytes);
        buf.put_u64(self.duration_ms);
        // Trailing files_scanned: present flag + optional u64.
        if self.files_scanned > 0 {
            buf.put_u8(1);
            buf.put_u64(self.files_scanned);
        } else {
            buf.put_u8(0);
        }
        Ok(buf.freeze())
    }

    pub fn decode(mut payload: Bytes) -> Result<Self> {
        if payload.remaining() < 32 {
            anyhow::bail!("Done payload too short");
        }
        let files_ok = payload.get_u64();
        let files_err = payload.get_u64();
        let bytes = payload.get_u64();
        let duration_ms = payload.get_u64();
        // Optional trailing files_scanned.
        let files_scanned = if payload.remaining() >= 1 {
            let present = payload.get_u8();
            if present == 1 && payload.remaining() >= 8 {
                payload.get_u64()
            } else {
                0
            }
        } else {
            0
        };
        Ok(Self {
            files_ok,
            files_err,
            bytes,
            duration_ms,
            files_scanned,
        })
    }
}

// Frame reading/writing

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

// Version Negotiation

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

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hello_roundtrip() {
        let hello = Hello::new(HelloFlags::PULL | HelloFlags::DELETE, "/tmp/dest");
        let encoded = hello.encode().unwrap();

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
        let encoded = entry.encode().unwrap();
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
        let encoded = entry.encode().unwrap();
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
        let encoded = entry.encode().unwrap();
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
        let encoded = entry.encode().unwrap();
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
        let encoded = data.encode().unwrap();
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
        let encoded = progress.encode().unwrap();
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
        let encoded = xattr.encode().unwrap();
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
            files_scanned: 200,
        };
        let encoded = done.encode().unwrap();
        let payload = Bytes::copy_from_slice(&encoded[5..]);
        let decoded = Done::decode(payload).unwrap();

        assert_eq!(decoded.files_ok, 100);
        assert_eq!(decoded.files_err, 2);
        assert_eq!(decoded.bytes, 1024 * 1024 * 50);
        assert_eq!(decoded.duration_ms, 5000);
        assert_eq!(decoded.files_scanned, 200);
    }

    #[test]
    fn test_done_backward_compat_no_scanned() {
        // Old peer sends Done without trailing files_scanned
        let mut buf = BytesMut::new();
        buf.put_u64(10);
        buf.put_u64(0);
        buf.put_u64(1024);
        buf.put_u64(100);
        let decoded = Done::decode(buf.freeze()).unwrap();

        assert_eq!(decoded.files_ok, 10);
        assert_eq!(decoded.files_scanned, 0);
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

    #[test]
    fn test_hello_flags_combinations() {
        // Test all flag combinations
        let flags = HelloFlags::PULL
            | HelloFlags::DELETE
            | HelloFlags::CHECKSUM
            | HelloFlags::COMPRESSION
            | HelloFlags::XATTRS
            | HelloFlags::ACLS
            | HelloFlags::DRY_RUN;
        assert!(flags.contains(HelloFlags::PULL));
        assert!(flags.contains(HelloFlags::DELETE));
        assert!(flags.contains(HelloFlags::CHECKSUM));
        assert!(flags.contains(HelloFlags::COMPRESSION));
        assert!(flags.contains(HelloFlags::XATTRS));
        assert!(flags.contains(HelloFlags::ACLS));
        assert!(flags.contains(HelloFlags::DRY_RUN));
    }

    #[test]
    fn test_hello_empty_path() {
        let hello = Hello::new(HelloFlags::empty(), "");
        let encoded = hello.encode().unwrap();
        let payload = Bytes::copy_from_slice(&encoded[5..]);
        let decoded = Hello::decode(payload).unwrap();
        assert_eq!(decoded.root_path, "");
    }

    #[test]
    fn test_hello_long_path() {
        let long_path = "a".repeat(10000);
        let hello = Hello::new(HelloFlags::empty(), &long_path);
        let encoded = hello.encode().unwrap();
        let payload = Bytes::copy_from_slice(&encoded[5..]);
        let decoded = Hello::decode(payload).unwrap();
        assert_eq!(decoded.root_path, long_path);
    }

    #[test]
    fn test_file_entry_special_characters() {
        let entry = FileEntry {
            path: "file with spaces and \"quotes\" and 'single'".to_string(),
            size: 1024,
            mtime: 1234567890,
            mode: 0o644,
            inode: 0,
            flags: FileFlags::empty(),
            symlink_target: None,
            link_target: None,
        };
        let encoded = entry.encode().unwrap();
        let payload = Bytes::copy_from_slice(&encoded[5..]);
        let decoded = FileEntry::decode(payload).unwrap();
        assert_eq!(decoded.path, entry.path);
    }

    #[test]
    fn test_dest_file_entry_empty_checksums() {
        let entry = DestFileEntry {
            path: "small.txt".to_string(),
            size: 100,
            mtime: 1234567890,
            mode: 0o644,
            flags: DestFileFlags::empty(),
            block_size: 0,
            checksums: Vec::new(),
        };
        let encoded = entry.encode().unwrap();
        let payload = Bytes::copy_from_slice(&encoded[5..]);
        let decoded = DestFileEntry::decode(payload).unwrap();
        assert_eq!(decoded.path, entry.path);
        assert!(decoded.checksums.is_empty());
    }

    #[test]
    fn test_dest_file_entry_many_checksums() {
        // Simulate a large file with many blocks
        let mut checksums = Vec::new();
        for i in 0..10000 {
            checksums.push(BlockChecksum {
                offset: i * 4096,
                weak: i as u32,
                strong: i,
            });
        }
        let entry = DestFileEntry {
            path: "large.bin".to_string(),
            size: 10000 * 4096,
            mtime: 1234567890,
            mode: 0o644,
            flags: DestFileFlags::HAS_CHECKSUMS,
            block_size: 4096,
            checksums,
        };
        let encoded = entry.encode().unwrap();
        let payload = Bytes::copy_from_slice(&encoded[5..]);
        let decoded = DestFileEntry::decode(payload).unwrap();
        assert_eq!(decoded.checksums.len(), 10000);
        assert_eq!(decoded.checksums[0].offset, 0);
        assert_eq!(decoded.checksums[9999].offset, 9999 * 4096);
    }

    #[test]
    fn test_data_empty_payload() {
        let data = Data {
            path: "file.txt".to_string(),
            offset: 0,
            flags: DataFlags::empty(),
            data: Bytes::new(),
        };
        let encoded = data.encode().unwrap();
        let payload = Bytes::copy_from_slice(&encoded[5..]);
        let decoded = Data::decode(payload).unwrap();
        assert!(decoded.data.is_empty());
    }

    #[test]
    fn test_data_compressed_flag() {
        let data = Data {
            path: "file.txt".to_string(),
            offset: 1024,
            flags: DataFlags::COMPRESSED,
            data: Bytes::from(vec![0u8; 100]),
        };
        let encoded = data.encode().unwrap();
        let payload = Bytes::copy_from_slice(&encoded[5..]);
        let decoded = Data::decode(payload).unwrap();
        assert!(decoded.flags.contains(DataFlags::COMPRESSED));
    }

    #[test]
    fn test_progress_zero_values() {
        let progress = Progress {
            files: 0,
            bytes: 0,
            files_total: 0,
            bytes_total: 0,
        };
        let encoded = progress.encode().unwrap();
        let payload = Bytes::copy_from_slice(&encoded[5..]);
        let decoded = Progress::decode(payload).unwrap();
        assert_eq!(decoded.files, 0);
        assert_eq!(decoded.bytes, 0);
    }

    #[test]
    fn test_fatal_long_message() {
        let long_msg = "error ".repeat(1000);
        let fatal = Fatal {
            code: 1,
            message: long_msg.clone(),
        };
        let encoded = fatal.encode().unwrap();
        let payload = Bytes::copy_from_slice(&encoded[5..]);
        let decoded = Fatal::decode(payload).unwrap();
        assert_eq!(decoded.message, long_msg);
    }

    // --- Error path tests: truncated/malformed messages ---

    #[test]
    fn test_hello_payload_too_short() {
        let payload = Bytes::from(vec![0u8; 7]); // Needs 8
        let result = Hello::decode(payload);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("too short"));
    }

    #[test]
    fn test_file_entry_payload_too_short() {
        let payload = Bytes::from(vec![0u8; 1]); // Needs 2 for path_len
        let result = FileEntry::decode(payload);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("too short"));
    }

    #[test]
    fn test_file_entry_path_truncated() {
        // path_len says 100 but only 2 bytes follow
        let mut data = vec![0u8; 3];
        data[0] = 0;
        data[1] = 100; // path_len = 100
        let payload = Bytes::from(data);
        let result = FileEntry::decode(payload);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("truncated"));
    }

    #[test]
    fn test_data_payload_too_short() {
        // Data needs at least: 2 (path_len) + path + 8 (offset) + 1 (flags) + data
        // With 1 byte, path_len read fails
        let payload = Bytes::from(vec![0u8; 1]);
        let result = Data::decode(payload);
        assert!(result.is_err());
    }

    #[test]
    fn test_dest_file_entry_payload_too_short() {
        let payload = Bytes::from(vec![0u8; 1]); // Needs 2 for path_len
        let result = DestFileEntry::decode(payload);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("too short"));
    }

    #[test]
    fn test_error_payload_too_short() {
        let payload = Bytes::from(vec![0u8; 1]); // Too short for Error
        let result = Error::decode(payload);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("too short"));
    }

    #[test]
    fn test_file_entry_symlink_target_truncated() {
        // Create a FileEntry with symlink flag but truncated target length
        let mut data = vec![0u8; 30];
        data[24] = 0x01; // set symlink flag
                         // symlink_target_length at offset 25-26, set to 100 but don't provide data
        data[25] = 100;
        let payload = Bytes::from(data);
        let result = FileEntry::decode(payload);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("truncated"));
    }

    #[test]
    fn test_hello_filter_patterns_roundtrip() {
        let patterns = "- .git\n- .git/**\n+ *.rs\n- *.py".to_string();
        let hello =
            Hello::new(HelloFlags::PULL, "/src").with_filter_patterns(Some(patterns.clone()));
        let encoded = hello.encode().unwrap();
        let payload = Bytes::copy_from_slice(&encoded[5..]);
        let decoded = Hello::decode(payload).unwrap();

        assert_eq!(decoded.filter_patterns, Some(patterns));
        assert!(decoded.max_delete.is_none());
    }

    #[test]
    fn test_hello_dirs_only_flag() {
        let hello = Hello::new(HelloFlags::PULL | HelloFlags::DIRS_ONLY, "/src");
        let encoded = hello.encode().unwrap();
        let payload = Bytes::copy_from_slice(&encoded[5..]);
        let decoded = Hello::decode(payload).unwrap();

        assert!(decoded.flags.contains(HelloFlags::DIRS_ONLY));
    }

    #[test]
    fn test_hello_all_trailing_fields() {
        let patterns = "- build".to_string();
        let max_delete = "50%".to_string();
        let hello = Hello::new(HelloFlags::PULL | HelloFlags::DELETE, "/src")
            .with_max_delete(Some(max_delete.clone()))
            .with_filter_patterns(Some(patterns.clone()))
            .with_comparison_flags(0x1F); // all flags set
        let encoded = hello.encode().unwrap();
        let payload = Bytes::copy_from_slice(&encoded[5..]);
        let decoded = Hello::decode(payload).unwrap();

        assert_eq!(decoded.max_delete, Some(max_delete));
        assert_eq!(decoded.filter_patterns, Some(patterns));
        assert_eq!(decoded.comparison_flags, Some(0x1F));
        let (checksum, update, existing, ignore_times, size_only) =
            decoded.comparison_flags_tuple();
        assert!(checksum);
        assert!(update);
        assert!(existing);
        assert!(ignore_times);
        assert!(size_only);
    }

    /// Frame length header must match actual payload size — otherwise the
    /// receiver reads the wrong number of bytes and the stream corrupts.
    #[test]
    fn test_hello_frame_length_matches_payload() {
        let cases = [
            Hello::new(HelloFlags::empty(), "/tmp"),
            Hello::new(HelloFlags::PULL, "/src").with_comparison_flags(0x01),
            Hello::new(HelloFlags::DELETE, "/a").with_max_delete(Some("50%".into())),
            Hello::new(HelloFlags::PULL, "/b")
                .with_filter_patterns(Some("- build".into()))
                .with_comparison_flags(0x1F),
            Hello::new(HelloFlags::PULL | HelloFlags::DELETE, "/c")
                .with_max_delete(Some("100".into()))
                .with_filter_patterns(Some("+ *.rs\n- target".into()))
                .with_comparison_flags(0x08),
        ];
        for hello in &cases {
            let encoded = hello.encode().unwrap();
            let frame_len =
                u32::from_be_bytes([encoded[0], encoded[1], encoded[2], encoded[3]]) as usize;
            let actual_payload = encoded.len() - 5; // 4 len + 1 type
            assert_eq!(
                frame_len, actual_payload,
                "frame length mismatch for Hello({:?})",
                hello.flags
            );
        }
    }

    #[test]
    fn test_hello_backward_compat_no_trailing_fields() {
        // Simulate old peer: only version+flags+path
        let mut buf = BytesMut::new();
        buf.put_u16(PROTOCOL_VERSION);
        buf.put_u32(HelloFlags::PULL.bits());
        buf.put_u16(5);
        buf.put_slice(b"/dest");
        let decoded = Hello::decode(buf.freeze()).unwrap();

        assert!(decoded.max_delete.is_none());
        assert!(decoded.filter_patterns.is_none());
        assert!(decoded.comparison_flags.is_none());
    }

    #[test]
    fn test_hello_backward_compat_max_delete_no_filter() {
        // Simulate peer with max_delete but no filter_patterns (previous version)
        let mut buf = BytesMut::new();
        buf.put_u16(PROTOCOL_VERSION);
        buf.put_u32(HelloFlags::PULL.bits());
        buf.put_u16(5);
        buf.put_slice(b"/dest");
        buf.put_u8(1);
        let md = b"30%";
        buf.put_u16(md.len() as u16);
        buf.put_slice(md);
        let decoded = Hello::decode(buf.freeze()).unwrap();

        assert_eq!(decoded.max_delete, Some("30%".to_string()));
        assert!(decoded.filter_patterns.is_none());
    }

    // === Fuzz tests — verify decoders don't panic on arbitrary input ===

    use proptest::prelude::*;

    proptest! {
        /// Fuzz the frame reader: arbitrary bytes must not panic
        #[test]
        fn prop_read_frame_no_panic(data in prop::collection::vec(any::<u8>(), 0..4096)) {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let mut cursor = std::io::Cursor::new(data);
                // read_frame should return Result, never panic
                let _result = super::read_frame(&mut cursor).await;
            });
        }

        /// MessageType::from_u8 must never panic on any u8 value
        #[test]
        fn prop_message_type_from_u8_no_panic(b in any::<u8>()) {
            let _ = super::MessageType::from_u8(b);
        }

        #[test]
        fn prop_file_entry_decode_no_panic(payload in prop::collection::vec(any::<u8>(), 0..2048)) {
            let _ = super::FileEntry::decode(bytes::Bytes::from(payload));
        }

        #[test]
        fn prop_hello_decode_no_panic(payload in prop::collection::vec(any::<u8>(), 0..2048)) {
            let _ = super::Hello::decode(bytes::Bytes::from(payload));
        }

        #[test]
        fn prop_dest_file_entry_decode_no_panic(payload in prop::collection::vec(any::<u8>(), 0..2048)) {
            let _ = super::DestFileEntry::decode(bytes::Bytes::from(payload));
        }

        #[test]
        fn prop_delete_decode_no_panic(payload in prop::collection::vec(any::<u8>(), 0..2048)) {
            let _ = super::Delete::decode(bytes::Bytes::from(payload));
        }

        #[test]
        fn prop_error_decode_no_panic(payload in prop::collection::vec(any::<u8>(), 0..2048)) {
            let _ = super::Error::decode(bytes::Bytes::from(payload));
        }
    }
}
