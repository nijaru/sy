use anyhow::{Context, Result};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

// Protocol Constants
pub const PROTOCOL_VERSION: u16 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MessageType {
    Hello = 0x01,
    FileList = 0x02,
    FileListAck = 0x03,
    FileData = 0x04,
    FileDone = 0x05,
    MkdirBatch = 0x06,
    DeleteBatch = 0x07,
    ChecksumReq = 0x08,
    ChecksumResp = 0x09,
    DeltaData = 0x0A,
    Progress = 0x10,
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
            0x07 => Some(Self::DeleteBatch),
            0x08 => Some(Self::ChecksumReq),
            0x09 => Some(Self::ChecksumResp),
            0x0A => Some(Self::DeltaData),
            0x10 => Some(Self::Progress),
            0xFF => Some(Self::Error),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub struct Hello {
    pub version: u16,
    pub flags: u32,
    pub capabilities: Vec<u8>,
}

#[derive(Debug)]
pub struct FileListEntry {
    pub path: String,
    pub size: u64,
    pub mtime: i64,
    pub mode: u32,
    pub flags: u8, // bit 0: dir, 1: symlink
}

#[derive(Debug)]
pub struct FileList {
    pub entries: Vec<FileListEntry>,
}

#[derive(Debug)]
pub struct FileData {
    pub index: u32,
    pub offset: u64,
    pub data: Vec<u8>,
}

#[derive(Debug)]
pub struct ErrorMessage {
    pub code: u16,
    pub message: String,
}

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

// ... existing impls ...

impl FileListAck {
    pub async fn write<W: AsyncWrite + Unpin>(&self, w: &mut W) -> Result<()> {
        // count(4) + (index(4) + action(1)) * N
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
            let action = Action::from_u8(action_byte).unwrap_or(Action::Skip); // Default to Skip on unknown

            decisions.push(Decision { index, action });
        }

        Ok(FileListAck { decisions })
    }
}

#[derive(Debug)]
pub struct FileDone {
    pub index: u32,
    pub status: u8,        // 0=OK, 1=ChecksumMismatch, 2=WriteError, 3=PermDenied
    pub checksum: Vec<u8>, // 32 bytes usually
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

// Message serialization
impl Hello {
    pub async fn write<W: AsyncWrite + Unpin>(&self, w: &mut W) -> Result<()> {
        // Calculate length: version(2) + flags(4) + caps_len(4) + caps(N)
        let len = 2 + 4 + 4 + self.capabilities.len() as u32;

        w.write_u32(len).await?; // Total length
        w.write_u8(MessageType::Hello as u8).await?; // Type

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

impl FileList {
    pub async fn write<W: AsyncWrite + Unpin>(&self, w: &mut W) -> Result<()> {
        // We need to calculate size first, or write to a buffer.
        // Since we are length-prefixed, buffering is safest for now.
        // For massive lists, we'd stream, but for now let's buffer the payload.
        let mut payload = Vec::new();

        // Write count
        payload.write_u32(self.entries.len() as u32).await?;

        for entry in &self.entries {
            // Path string
            let path_bytes = entry.path.as_bytes();
            payload.write_u16(path_bytes.len() as u16).await?;
            payload.write_all(path_bytes).await?;

            payload.write_u64(entry.size).await?;
            payload.write_i64(entry.mtime).await?;
            payload.write_u32(entry.mode).await?;
            payload.write_u8(entry.flags).await?;
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

            entries.push(FileListEntry {
                path,
                size,
                mtime,
                mode,
                flags,
            });
        }

        Ok(FileList { entries })
    }
}

impl FileData {
    pub async fn write<W: AsyncWrite + Unpin>(&self, w: &mut W) -> Result<()> {
        let len = 4 + 8 + 4 + self.data.len() as u32; // index(4) + offset(8) + data_len(4) + data(N)

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

impl ErrorMessage {
    pub async fn write<W: AsyncWrite + Unpin>(&self, w: &mut W) -> Result<()> {
        let msg_bytes = self.message.as_bytes();
        let len = 2 + 2 + msg_bytes.len() as u32; // code(2) + str_len(2) + msg(N)

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
