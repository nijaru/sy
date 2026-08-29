use super::domain::{EntryKind, InvalidRelativePath, InvalidTimestamp, RelativePath, Timestamp};
use std::ffi::{OsStr, OsString};
use std::io;
use std::path::PathBuf;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, SeekFrom};

const TRAILER_LEN: u64 = 4;
const MAX_RECORD_PAYLOAD: usize = 1024 * 1024;
const FIELD_MODE: u8 = 1 << 0;
const FIELD_MODIFIED: u8 = 1 << 1;
const FIELD_MASK: u8 = FIELD_MODE | FIELD_MODIFIED;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinalizeMetadata {
    pub path: RelativePath,
    pub kind: EntryKind,
    pub unix_mode: Option<u32>,
    pub modified: Option<Timestamp>,
}

#[derive(Debug, thiserror::Error)]
pub enum FinalizeJournalError {
    #[error(transparent)]
    Io(#[from] io::Error),

    #[error("finalize journal temporary-file worker failed: {0}")]
    Worker(String),

    #[error("finalize metadata request for {0} has no fields")]
    EmptyMetadata(PathBuf),

    #[error("finalize journal record is too large: {actual} bytes, maximum {maximum}")]
    RecordTooLarge { actual: usize, maximum: usize },

    #[error("invalid finalize journal record: {0}")]
    InvalidRecord(&'static str),

    #[error(transparent)]
    InvalidRelativePath(#[from] InvalidRelativePath),

    #[error(transparent)]
    InvalidTimestamp(#[from] InvalidTimestamp),
}

pub type Result<T> = std::result::Result<T, FinalizeJournalError>;

/// Exact bounded-memory spool for metadata that must run after namespace work.
///
/// Records are appended in deterministic reconciliation order and replayed from
/// the end. Directory finalization therefore runs child-before-parent without
/// retaining an in-memory directory stack or record-offset index.
pub struct FinalizeJournal {
    file: tokio::fs::File,
    records: usize,
}

impl FinalizeJournal {
    pub async fn new() -> Result<Self> {
        let file = tokio::task::spawn_blocking(tempfile::tempfile)
            .await
            .map_err(|error| FinalizeJournalError::Worker(error.to_string()))??;
        Ok(Self {
            file: tokio::fs::File::from_std(file),
            records: 0,
        })
    }

    pub async fn append(&mut self, metadata: &FinalizeMetadata) -> Result<()> {
        if metadata.unix_mode.is_none() && metadata.modified.is_none() {
            return Err(FinalizeJournalError::EmptyMetadata(
                metadata.path.as_path().to_path_buf(),
            ));
        }

        let payload = encode_metadata(metadata)?;
        if payload.len() > MAX_RECORD_PAYLOAD {
            return Err(FinalizeJournalError::RecordTooLarge {
                actual: payload.len(),
                maximum: MAX_RECORD_PAYLOAD,
            });
        }
        let payload_len =
            u32::try_from(payload.len()).map_err(|_| FinalizeJournalError::RecordTooLarge {
                actual: payload.len(),
                maximum: MAX_RECORD_PAYLOAD,
            })?;

        self.file.write_all(&payload).await?;
        self.file.write_u32(payload_len).await?;
        self.records = self.records.checked_add(1).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "finalize journal record count overflow",
            )
        })?;
        Ok(())
    }

    pub async fn seal(mut self) -> Result<FinalizeJournalReader> {
        self.file.flush().await?;
        let cursor = self.file.metadata().await?.len();
        Ok(FinalizeJournalReader {
            file: self.file,
            cursor,
            remaining: self.records,
        })
    }
}

pub struct FinalizeJournalReader {
    file: tokio::fs::File,
    cursor: u64,
    remaining: usize,
}

impl FinalizeJournalReader {
    pub async fn next(&mut self) -> Result<Option<FinalizeMetadata>> {
        if self.cursor == 0 {
            if self.remaining != 0 {
                return Err(FinalizeJournalError::InvalidRecord(
                    "journal ended before all records were decoded",
                ));
            }
            return Ok(None);
        }
        if self.remaining == 0 {
            return Err(FinalizeJournalError::InvalidRecord(
                "trailing bytes after final record",
            ));
        }
        if self.cursor < TRAILER_LEN {
            return Err(FinalizeJournalError::InvalidRecord(
                "truncated record trailer",
            ));
        }

        self.file
            .seek(SeekFrom::Start(self.cursor - TRAILER_LEN))
            .await?;
        let payload_len = self.file.read_u32().await? as usize;
        if payload_len == 0 {
            return Err(FinalizeJournalError::InvalidRecord("empty record"));
        }
        if payload_len > MAX_RECORD_PAYLOAD {
            return Err(FinalizeJournalError::RecordTooLarge {
                actual: payload_len,
                maximum: MAX_RECORD_PAYLOAD,
            });
        }

        let total_len = u64::try_from(payload_len)
            .map_err(|_| FinalizeJournalError::InvalidRecord("record length overflow"))?
            .checked_add(TRAILER_LEN)
            .ok_or(FinalizeJournalError::InvalidRecord(
                "record total length overflow",
            ))?;
        let start = self
            .cursor
            .checked_sub(total_len)
            .ok_or(FinalizeJournalError::InvalidRecord(
                "record extends before file start",
            ))?;

        self.file.seek(SeekFrom::Start(start)).await?;
        let mut payload = vec![0_u8; payload_len];
        self.file.read_exact(&mut payload).await?;
        let metadata = decode_metadata(&payload)?;

        self.cursor = start;
        self.remaining = self
            .remaining
            .checked_sub(1)
            .ok_or(FinalizeJournalError::InvalidRecord(
                "journal contains more records than expected",
            ))?;
        Ok(Some(metadata))
    }
}

fn encode_metadata(metadata: &FinalizeMetadata) -> Result<Vec<u8>> {
    let mut payload = Vec::new();
    encode_path(&mut payload, metadata.path.as_path().as_os_str())?;
    payload.push(match metadata.kind {
        EntryKind::File => 0,
        EntryKind::Directory => 1,
        EntryKind::Symlink => 2,
    });

    let mut fields = 0_u8;
    if metadata.unix_mode.is_some() {
        fields |= FIELD_MODE;
    }
    if metadata.modified.is_some() {
        fields |= FIELD_MODIFIED;
    }
    payload.push(fields);

    if let Some(mode) = metadata.unix_mode {
        payload.extend_from_slice(&mode.to_be_bytes());
    }
    if let Some(modified) = metadata.modified {
        payload.extend_from_slice(&modified.seconds().to_be_bytes());
        payload.extend_from_slice(&modified.nanoseconds().to_be_bytes());
    }
    Ok(payload)
}

fn decode_metadata(payload: &[u8]) -> Result<FinalizeMetadata> {
    let mut reader = SliceReader::new(payload);
    let path = RelativePath::new(decode_path(&mut reader)?)?;
    let kind = match reader.u8()? {
        0 => EntryKind::File,
        1 => EntryKind::Directory,
        2 => EntryKind::Symlink,
        _ => {
            return Err(FinalizeJournalError::InvalidRecord(
                "unknown entry kind",
            ))
        }
    };
    let fields = reader.u8()?;
    if fields == 0 {
        return Err(FinalizeJournalError::InvalidRecord(
            "metadata request has no fields",
        ));
    }
    if fields & !FIELD_MASK != 0 {
        return Err(FinalizeJournalError::InvalidRecord(
            "unknown metadata field bits",
        ));
    }

    let unix_mode = if fields & FIELD_MODE != 0 {
        Some(reader.u32()?)
    } else {
        None
    };
    let modified = if fields & FIELD_MODIFIED != 0 {
        Some(Timestamp::new(reader.i64()?, reader.u32()?)?)
    } else {
        None
    };
    reader.finish()?;

    Ok(FinalizeMetadata {
        path,
        kind,
        unix_mode,
        modified,
    })
}

fn encode_path(payload: &mut Vec<u8>, path: &OsStr) -> Result<()> {
    let bytes = encode_native_path(path);
    let len = u32::try_from(bytes.len()).map_err(|_| FinalizeJournalError::RecordTooLarge {
        actual: bytes.len(),
        maximum: MAX_RECORD_PAYLOAD,
    })?;
    payload.extend_from_slice(&len.to_be_bytes());
    payload.extend_from_slice(&bytes);
    Ok(())
}

fn decode_path(reader: &mut SliceReader<'_>) -> Result<PathBuf> {
    let len = reader.u32()? as usize;
    decode_native_path(reader.take(len)?)
}

#[cfg(unix)]
fn encode_native_path(path: &OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_bytes().to_vec()
}

#[cfg(unix)]
fn decode_native_path(path: &[u8]) -> Result<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    Ok(PathBuf::from(OsString::from_vec(path.to_vec())))
}

#[cfg(windows)]
fn encode_native_path(path: &OsStr) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;
    let mut bytes = Vec::new();
    for unit in path.encode_wide() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes
}

#[cfg(windows)]
fn decode_native_path(path: &[u8]) -> Result<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    if path.len() % 2 != 0 {
        return Err(FinalizeJournalError::InvalidRecord(
            "odd byte length in Windows finalize journal path",
        ));
    }
    let wide = path
        .chunks_exact(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .collect::<Vec<_>>();
    Ok(PathBuf::from(OsString::from_wide(&wide)))
}

#[cfg(not(any(unix, windows)))]
fn encode_native_path(path: &OsStr) -> Vec<u8> {
    path.to_string_lossy().into_owned().into_bytes()
}

#[cfg(not(any(unix, windows)))]
fn decode_native_path(path: &[u8]) -> Result<PathBuf> {
    let value = String::from_utf8(path.to_vec()).map_err(|_| {
        FinalizeJournalError::InvalidRecord(
            "non-Unicode path in finalize journal on unsupported platform",
        )
    })?;
    Ok(PathBuf::from(value))
}

struct SliceReader<'a> {
    payload: &'a [u8],
    offset: usize,
}

impl<'a> SliceReader<'a> {
    const fn new(payload: &'a [u8]) -> Self {
        Self { payload, offset: 0 }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self
            .offset
            .checked_add(len)
            .ok_or(FinalizeJournalError::InvalidRecord("record length overflow"))?;
        let value = self
            .payload
            .get(self.offset..end)
            .ok_or(FinalizeJournalError::InvalidRecord("truncated record"))?;
        self.offset = end;
        Ok(value)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        let bytes = self.take(N)?;
        let mut value = [0_u8; N];
        value.copy_from_slice(bytes);
        Ok(value)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.array::<4>()?))
    }

    fn i64(&mut self) -> Result<i64> {
        Ok(i64::from_be_bytes(self.array::<8>()?))
    }

    fn finish(self) -> Result<()> {
        if self.offset != self.payload.len() {
            return Err(FinalizeJournalError::InvalidRecord(
                "trailing bytes in record",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(value: &str) -> RelativePath {
        RelativePath::new(PathBuf::from(value)).unwrap()
    }

    #[tokio::test]
    async fn replays_child_before_parent() {
        let mut journal = FinalizeJournal::new().await.unwrap();
        let parent = FinalizeMetadata {
            path: path("parent"),
            kind: EntryKind::Directory,
            unix_mode: Some(0o755),
            modified: Some(Timestamp::UNIX_EPOCH),
        };
        let child = FinalizeMetadata {
            path: path("parent/child"),
            kind: EntryKind::Directory,
            unix_mode: Some(0o700),
            modified: None,
        };
        journal.append(&parent).await.unwrap();
        journal.append(&child).await.unwrap();

        let mut reader = journal.seal().await.unwrap();
        assert_eq!(reader.next().await.unwrap(), Some(child));
        assert_eq!(reader.next().await.unwrap(), Some(parent));
        assert_eq!(reader.next().await.unwrap(), None);
    }

    #[tokio::test]
    async fn rejects_empty_metadata_request() {
        let mut journal = FinalizeJournal::new().await.unwrap();
        let metadata = FinalizeMetadata {
            path: path("directory"),
            kind: EntryKind::Directory,
            unix_mode: None,
            modified: None,
        };
        assert!(matches!(
            journal.append(&metadata).await.unwrap_err(),
            FinalizeJournalError::EmptyMetadata(_)
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn preserves_non_utf8_paths() {
        use std::os::unix::ffi::OsStringExt;

        let path = RelativePath::new(PathBuf::from(OsString::from_vec(vec![
            b'd', 0x80, b'r',
        ])))
        .unwrap();
        let metadata = FinalizeMetadata {
            path,
            kind: EntryKind::Directory,
            unix_mode: None,
            modified: Some(Timestamp::UNIX_EPOCH),
        };
        let mut journal = FinalizeJournal::new().await.unwrap();
        journal.append(&metadata).await.unwrap();
        let mut reader = journal.seal().await.unwrap();
        assert_eq!(reader.next().await.unwrap(), Some(metadata));
    }

    #[tokio::test]
    async fn rejects_corrupt_trailing_length() {
        let mut journal = FinalizeJournal::new().await.unwrap();
        journal
            .append(&FinalizeMetadata {
                path: path("directory"),
                kind: EntryKind::Directory,
                unix_mode: Some(0o755),
                modified: None,
            })
            .await
            .unwrap();
        let mut reader = journal.seal().await.unwrap();
        let end = reader.cursor;
        reader
            .file
            .seek(SeekFrom::Start(end - TRAILER_LEN))
            .await
            .unwrap();
        reader.file.write_u32(u32::MAX).await.unwrap();
        reader.file.flush().await.unwrap();
        assert!(matches!(
            reader.next().await.unwrap_err(),
            FinalizeJournalError::RecordTooLarge { .. }
        ));
    }
}