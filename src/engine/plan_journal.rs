use super::domain::{
    Entry, EntryIdentity, EntryKind, InvalidRelativePath, InvalidTimestamp, RelativePath,
    SkipReason, SyncOp, Timestamp,
};
use std::ffi::{OsStr, OsString};
use std::io;
use std::path::PathBuf;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, SeekFrom};

const MAX_RECORD_PAYLOAD: usize = 1024 * 1024;
const ENTRY_OPTION_MODE: u8 = 1 << 0;
const ENTRY_OPTION_SYMLINK_TARGET: u8 = 1 << 1;
const ENTRY_OPTION_IDENTITY: u8 = 1 << 2;
const ENTRY_OPTION_HARDLINK_GROUP: u8 = 1 << 3;
const ENTRY_OPTION_MASK: u8 = ENTRY_OPTION_MODE
    | ENTRY_OPTION_SYMLINK_TARGET
    | ENTRY_OPTION_IDENTITY
    | ENTRY_OPTION_HARDLINK_GROUP;

#[derive(Debug, thiserror::Error)]
pub enum PlanJournalError {
    #[error(transparent)]
    Io(#[from] io::Error),

    #[error("plan journal temporary-file worker failed: {0}")]
    Worker(String),

    #[error("plan journal record is empty")]
    EmptyRecord,

    #[error("plan journal record is too large: {actual} bytes, maximum {maximum}")]
    RecordTooLarge { actual: usize, maximum: usize },

    #[error("invalid plan journal record: {0}")]
    InvalidRecord(&'static str),

    #[error(transparent)]
    InvalidRelativePath(#[from] InvalidRelativePath),

    #[error(transparent)]
    InvalidTimestamp(#[from] InvalidTimestamp),
}

pub type Result<T> = std::result::Result<T, PlanJournalError>;

/// Exact disk-backed spool for semantic planner output.
///
/// Delete-enabled synchronization requires a complete no-mutation merge before
/// execution. This journal lets that barrier remain bounded in RAM without
/// teaching the planner about a concrete endpoint or transfer strategy.
pub struct PlanJournal {
    file: tokio::fs::File,
    records: usize,
}

impl PlanJournal {
    pub async fn new() -> Result<Self> {
        let file = tokio::task::spawn_blocking(tempfile::tempfile)
            .await
            .map_err(|error| PlanJournalError::Worker(error.to_string()))??;
        Ok(Self {
            file: tokio::fs::File::from_std(file),
            records: 0,
        })
    }

    pub async fn append(&mut self, operation: &SyncOp) -> Result<()> {
        let payload = encode_operation(operation)?;
        if payload.is_empty() {
            return Err(PlanJournalError::EmptyRecord);
        }
        if payload.len() > MAX_RECORD_PAYLOAD {
            return Err(PlanJournalError::RecordTooLarge {
                actual: payload.len(),
                maximum: MAX_RECORD_PAYLOAD,
            });
        }
        let payload_len =
            u32::try_from(payload.len()).map_err(|_| PlanJournalError::RecordTooLarge {
                actual: payload.len(),
                maximum: MAX_RECORD_PAYLOAD,
            })?;

        self.file.write_u32(payload_len).await?;
        self.file.write_all(&payload).await?;
        self.records = self.records.checked_add(1).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "plan journal record count overflow",
            )
        })?;
        Ok(())
    }

    pub async fn seal(mut self) -> Result<PlanJournalReader> {
        self.file.flush().await?;
        let end = self.file.metadata().await?.len();
        self.file.seek(SeekFrom::Start(0)).await?;
        Ok(PlanJournalReader {
            file: self.file,
            remaining: self.records,
            end,
        })
    }
}

pub struct PlanJournalReader {
    file: tokio::fs::File,
    remaining: usize,
    end: u64,
}

impl PlanJournalReader {
    pub async fn next(&mut self) -> Result<Option<SyncOp>> {
        if self.remaining == 0 {
            self.reject_trailing_data().await?;
            return Ok(None);
        }

        let payload_len = self.file.read_u32().await? as usize;
        if payload_len == 0 {
            return Err(PlanJournalError::EmptyRecord);
        }
        if payload_len > MAX_RECORD_PAYLOAD {
            return Err(PlanJournalError::RecordTooLarge {
                actual: payload_len,
                maximum: MAX_RECORD_PAYLOAD,
            });
        }

        let mut payload = vec![0_u8; payload_len];
        self.file.read_exact(&mut payload).await?;
        let operation = decode_operation(&payload)?;
        self.remaining = self.remaining.checked_sub(1).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "plan journal contains more records than expected",
            )
        })?;
        if self.remaining == 0 {
            self.reject_trailing_data().await?;
        }
        Ok(Some(operation))
    }

    async fn reject_trailing_data(&mut self) -> Result<()> {
        let position = self.file.stream_position().await?;
        if position != self.end {
            return Err(PlanJournalError::InvalidRecord(
                "trailing bytes after final plan journal record",
            ));
        }
        Ok(())
    }
}

fn encode_operation(operation: &SyncOp) -> Result<Vec<u8>> {
    let mut payload = Vec::new();
    match operation {
        SyncOp::Create { source } => {
            payload.push(0);
            encode_entry(&mut payload, source)?;
        }
        SyncOp::Update {
            source,
            destination,
        } => {
            payload.push(1);
            encode_entry(&mut payload, source)?;
            encode_entry(&mut payload, destination)?;
        }
        SyncOp::Replace {
            source,
            destination,
        } => {
            payload.push(2);
            encode_entry(&mut payload, source)?;
            encode_entry(&mut payload, destination)?;
        }
        SyncOp::Metadata {
            source,
            destination,
        } => {
            payload.push(3);
            encode_entry(&mut payload, source)?;
            encode_entry(&mut payload, destination)?;
        }
        SyncOp::Skip { path, reason } => {
            payload.push(4);
            encode_relative_path(&mut payload, path)?;
            payload.push(match reason {
                SkipReason::Unchanged => 0,
                SkipReason::Filtered => 1,
                SkipReason::ExistingOnly => 2,
                SkipReason::DestinationNewer => 3,
                SkipReason::MissingDestination => 4,
            });
        }
    }
    Ok(payload)
}

fn decode_operation(payload: &[u8]) -> Result<SyncOp> {
    let mut reader = SliceReader::new(payload);
    let operation = match reader.u8()? {
        0 => SyncOp::Create {
            source: decode_entry(&mut reader)?,
        },
        1 => SyncOp::Update {
            source: decode_entry(&mut reader)?,
            destination: decode_entry(&mut reader)?,
        },
        2 => SyncOp::Replace {
            source: decode_entry(&mut reader)?,
            destination: decode_entry(&mut reader)?,
        },
        3 => SyncOp::Metadata {
            source: decode_entry(&mut reader)?,
            destination: decode_entry(&mut reader)?,
        },
        4 => {
            let path = decode_relative_path(&mut reader)?;
            let reason = match reader.u8()? {
                0 => SkipReason::Unchanged,
                1 => SkipReason::Filtered,
                2 => SkipReason::ExistingOnly,
                3 => SkipReason::DestinationNewer,
                4 => SkipReason::MissingDestination,
                _ => {
                    return Err(PlanJournalError::InvalidRecord(
                        "unknown plan journal skip reason",
                    ))
                }
            };
            SyncOp::Skip { path, reason }
        }
        _ => {
            return Err(PlanJournalError::InvalidRecord(
                "unknown plan journal operation kind",
            ))
        }
    };
    reader.finish()?;
    Ok(operation)
}

fn encode_entry(payload: &mut Vec<u8>, entry: &Entry) -> Result<()> {
    encode_relative_path(payload, &entry.path)?;
    payload.push(match entry.kind {
        EntryKind::File => 0,
        EntryKind::Directory => 1,
        EntryKind::Symlink => 2,
    });
    payload.extend_from_slice(&entry.size.to_be_bytes());
    encode_timestamp(payload, entry.modified);

    let mut options = 0_u8;
    if entry.unix_mode.is_some() {
        options |= ENTRY_OPTION_MODE;
    }
    if entry.symlink_target.is_some() {
        options |= ENTRY_OPTION_SYMLINK_TARGET;
    }
    if entry.identity.is_some() {
        options |= ENTRY_OPTION_IDENTITY;
    }
    if entry.hardlink_group.is_some() {
        options |= ENTRY_OPTION_HARDLINK_GROUP;
    }
    payload.push(options);

    if let Some(mode) = entry.unix_mode {
        payload.extend_from_slice(&mode.to_be_bytes());
    }
    if let Some(target) = &entry.symlink_target {
        encode_path(payload, target.as_os_str())?;
    }
    if let Some(identity) = entry.identity {
        payload.extend_from_slice(identity.as_bytes());
    }
    if let Some(group) = entry.hardlink_group {
        payload.extend_from_slice(group.as_bytes());
    }
    Ok(())
}

fn decode_entry(reader: &mut SliceReader<'_>) -> Result<Entry> {
    let path = decode_relative_path(reader)?;
    let kind = match reader.u8()? {
        0 => EntryKind::File,
        1 => EntryKind::Directory,
        2 => EntryKind::Symlink,
        _ => {
            return Err(PlanJournalError::InvalidRecord(
                "unknown plan journal entry kind",
            ))
        }
    };
    let size = reader.u64()?;
    let modified = decode_timestamp(reader)?;
    let options = reader.u8()?;
    if options & !ENTRY_OPTION_MASK != 0 {
        return Err(PlanJournalError::InvalidRecord(
            "unknown plan journal entry option bits",
        ));
    }

    let unix_mode = if options & ENTRY_OPTION_MODE != 0 {
        Some(reader.u32()?)
    } else {
        None
    };
    let symlink_target = if options & ENTRY_OPTION_SYMLINK_TARGET != 0 {
        Some(decode_path(reader)?)
    } else {
        None
    };
    let identity = if options & ENTRY_OPTION_IDENTITY != 0 {
        Some(EntryIdentity::from_bytes(reader.array::<32>()?))
    } else {
        None
    };
    let hardlink_group = if options & ENTRY_OPTION_HARDLINK_GROUP != 0 {
        Some(EntryIdentity::from_bytes(reader.array::<32>()?))
    } else {
        None
    };

    Ok(Entry {
        path,
        kind,
        size,
        modified,
        unix_mode,
        symlink_target,
        identity,
        hardlink_group,
    })
}

fn encode_timestamp(payload: &mut Vec<u8>, timestamp: Timestamp) {
    payload.extend_from_slice(&timestamp.seconds().to_be_bytes());
    payload.extend_from_slice(&timestamp.nanoseconds().to_be_bytes());
}

fn decode_timestamp(reader: &mut SliceReader<'_>) -> Result<Timestamp> {
    Ok(Timestamp::new(reader.i64()?, reader.u32()?)?)
}

fn encode_relative_path(payload: &mut Vec<u8>, path: &RelativePath) -> Result<()> {
    encode_path(payload, path.as_path().as_os_str())
}

fn decode_relative_path(reader: &mut SliceReader<'_>) -> Result<RelativePath> {
    Ok(RelativePath::new(decode_path(reader)?)?)
}

fn encode_path(payload: &mut Vec<u8>, path: &OsStr) -> Result<()> {
    let bytes = encode_native_path(path);
    let len = u32::try_from(bytes.len()).map_err(|_| PlanJournalError::RecordTooLarge {
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
        return Err(PlanJournalError::InvalidRecord(
            "odd byte length in Windows plan journal path",
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
        PlanJournalError::InvalidRecord("non-Unicode path in plan journal on unsupported platform")
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
            .ok_or(PlanJournalError::InvalidRecord("record length overflow"))?;
        let value = self
            .payload
            .get(self.offset..end)
            .ok_or(PlanJournalError::InvalidRecord(
                "truncated plan journal record",
            ))?;
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

    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_be_bytes(self.array::<8>()?))
    }

    fn i64(&mut self) -> Result<i64> {
        Ok(i64::from_be_bytes(self.array::<8>()?))
    }

    fn finish(self) -> Result<()> {
        if self.offset != self.payload.len() {
            return Err(PlanJournalError::InvalidRecord(
                "trailing bytes in plan journal record",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn path(value: &str) -> RelativePath {
        RelativePath::new(PathBuf::from(value)).unwrap()
    }

    fn file(value: &str, size: u64, byte: u8) -> Entry {
        let mut entry = Entry::file(path(value), size, Timestamp::new(12, 34).unwrap());
        entry.unix_mode = Some(0o640);
        entry.identity = Some(EntryIdentity::from_bytes([byte; 32]));
        entry.hardlink_group = Some(EntryIdentity::from_bytes([byte.wrapping_add(1); 32]));
        entry
    }

    #[tokio::test]
    async fn semantic_operations_round_trip_in_forward_order() {
        let source = file("dir/file", 12, 1);
        let destination = file("dir/file", 8, 2);
        let mut link = Entry::symlink(
            path("link"),
            PathBuf::from("../target"),
            Timestamp::new(55, 66).unwrap(),
        );
        link.unix_mode = Some(0o777);
        link.identity = Some(EntryIdentity::from_bytes([3; 32]));

        let operations = vec![
            SyncOp::Create {
                source: source.clone(),
            },
            SyncOp::Update {
                source: source.clone(),
                destination: destination.clone(),
            },
            SyncOp::Replace {
                source: link,
                destination: destination.clone(),
            },
            SyncOp::Metadata {
                source: source.clone(),
                destination,
            },
            SyncOp::Skip {
                path: path("skip"),
                reason: SkipReason::DestinationNewer,
            },
            SyncOp::Skip {
                path: path("missing"),
                reason: SkipReason::MissingDestination,
            },
        ];

        let mut journal = PlanJournal::new().await.unwrap();
        for operation in &operations {
            journal.append(operation).await.unwrap();
        }
        let mut reader = journal.seal().await.unwrap();
        for expected in operations {
            assert_eq!(reader.next().await.unwrap(), Some(expected));
        }
        assert_eq!(reader.next().await.unwrap(), None);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn journal_preserves_non_utf8_relative_paths() {
        use std::os::unix::ffi::OsStringExt;
        let raw = OsString::from_vec(vec![b'f', 0x80, b'o']);
        let relative = RelativePath::new(PathBuf::from(raw)).unwrap();
        let operation = SyncOp::Skip {
            path: relative.clone(),
            reason: SkipReason::Filtered,
        };
        let mut journal = PlanJournal::new().await.unwrap();
        journal.append(&operation).await.unwrap();
        let mut reader = journal.seal().await.unwrap();
        assert_eq!(reader.next().await.unwrap(), Some(operation));
        assert_eq!(reader.next().await.unwrap(), None);
        assert_eq!(relative.as_path().components().count(), 1);
    }

    #[tokio::test]
    async fn oversized_record_is_rejected_before_write() {
        let mut entry = Entry::symlink(
            path("link"),
            PathBuf::from("x".repeat(MAX_RECORD_PAYLOAD)),
            Timestamp::UNIX_EPOCH,
        );
        entry.identity = Some(EntryIdentity::from_bytes([4; 32]));
        let mut journal = PlanJournal::new().await.unwrap();
        let error = journal
            .append(&SyncOp::Create { source: entry })
            .await
            .unwrap_err();
        assert!(matches!(error, PlanJournalError::RecordTooLarge { .. }));
    }

    #[tokio::test]
    async fn corrupt_record_length_is_rejected_before_allocation() {
        let mut journal = PlanJournal::new().await.unwrap();
        journal
            .append(&SyncOp::Skip {
                path: path("skip"),
                reason: SkipReason::Unchanged,
            })
            .await
            .unwrap();
        let mut reader = journal.seal().await.unwrap();
        reader.file.seek(SeekFrom::Start(0)).await.unwrap();
        reader
            .file
            .write_u32(u32::try_from(MAX_RECORD_PAYLOAD + 1).unwrap())
            .await
            .unwrap();
        reader.file.flush().await.unwrap();
        reader.file.seek(SeekFrom::Start(0)).await.unwrap();

        let error = reader.next().await.unwrap_err();
        assert!(matches!(error, PlanJournalError::RecordTooLarge { .. }));
    }

    #[test]
    fn decoder_rejects_trailing_bytes_and_unknown_tags() {
        let operation = SyncOp::Skip {
            path: path("skip"),
            reason: SkipReason::Unchanged,
        };
        let mut payload = encode_operation(&operation).unwrap();
        payload.push(0);
        assert!(decode_operation(&payload).is_err());
        assert!(decode_operation(&[255]).is_err());
        assert_eq!(Path::new("skip"), operation.path().as_path());
    }
}
