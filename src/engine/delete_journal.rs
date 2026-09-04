use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, SeekFrom};

const TRAILER_LEN: u64 = 4;
const RECORD_HEADER_LEN: usize = 5;
const MAX_RECORD_PAYLOAD: usize = 1024 * 1024;

pub type Result<T> = std::result::Result<T, io::Error>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteKind {
    FileLike,
    Directory,
    /// Marks an earlier candidate directory as non-deletable because the
    /// destination subtree contains a source-backed or excluded descendant.
    ProtectDirectory,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteRecord {
    pub path: PathBuf,
    pub kind: DeleteKind,
}

/// Exact, bounded-memory spool for destination-only paths.
///
/// Records are appended in reconciliation order and replayed from the end. The
/// trailing record length makes reverse iteration possible without retaining an
/// in-memory offset index, so memory use is independent of deletion count.
pub struct DeleteJournal {
    file: tokio::fs::File,
    records: usize,
}

impl DeleteJournal {
    pub async fn new() -> Result<Self> {
        let file = tokio::task::spawn_blocking(tempfile::tempfile)
            .await
            .map_err(join_error)??;
        Ok(Self {
            file: tokio::fs::File::from_std(file),
            records: 0,
        })
    }

    pub async fn append(&mut self, path: &Path, kind: DeleteKind) -> Result<()> {
        let path = encode_path(path.as_os_str());
        let payload_len = RECORD_HEADER_LEN
            .checked_add(path.len())
            .ok_or_else(|| invalid_data("delete journal record length overflow"))?;

        if payload_len > MAX_RECORD_PAYLOAD {
            return Err(invalid_data(format!(
                "delete journal path is too large: {} bytes",
                path.len()
            )));
        }

        let path_len = u32::try_from(path.len())
            .map_err(|_| invalid_data("delete journal path length exceeds u32"))?;
        let payload_len = u32::try_from(payload_len)
            .map_err(|_| invalid_data("delete journal record length exceeds u32"))?;

        self.file
            .write_u8(match kind {
                DeleteKind::FileLike => 0,
                DeleteKind::Directory => 1,
                DeleteKind::ProtectDirectory => 2,
            })
            .await?;
        self.file.write_u32(path_len).await?;
        self.file.write_all(&path).await?;
        self.file.write_u32(payload_len).await?;
        self.records = self
            .records
            .checked_add(1)
            .ok_or_else(|| invalid_data("delete journal record count overflow"))?;
        Ok(())
    }

    pub async fn seal(mut self) -> Result<DeleteJournalReader> {
        self.file.flush().await?;
        let cursor = self.file.metadata().await?.len();
        Ok(DeleteJournalReader {
            file: self.file,
            cursor,
            remaining: self.records,
        })
    }
}

pub struct DeleteJournalReader {
    file: tokio::fs::File,
    cursor: u64,
    remaining: usize,
}

impl DeleteJournalReader {
    pub async fn next(&mut self) -> Result<Option<DeleteRecord>> {
        if self.cursor == 0 {
            if self.remaining != 0 {
                return Err(invalid_data(
                    "delete journal ended before all records were decoded",
                ));
            }
            return Ok(None);
        }

        if self.cursor < TRAILER_LEN {
            return Err(invalid_data("truncated delete journal trailer"));
        }

        self.file
            .seek(SeekFrom::Start(self.cursor - TRAILER_LEN))
            .await?;
        let payload_len = self.file.read_u32().await? as usize;
        if !(RECORD_HEADER_LEN..=MAX_RECORD_PAYLOAD).contains(&payload_len) {
            return Err(invalid_data(format!(
                "invalid delete journal record length: {payload_len}"
            )));
        }

        let total_len = u64::try_from(payload_len)
            .map_err(|_| invalid_data("delete journal record length overflow"))?
            .checked_add(TRAILER_LEN)
            .ok_or_else(|| invalid_data("delete journal total record length overflow"))?;
        let start = self
            .cursor
            .checked_sub(total_len)
            .ok_or_else(|| invalid_data("delete journal record extends before file start"))?;

        self.file.seek(SeekFrom::Start(start)).await?;
        let kind = match self.file.read_u8().await? {
            0 => DeleteKind::FileLike,
            1 => DeleteKind::Directory,
            2 => DeleteKind::ProtectDirectory,
            value => {
                return Err(invalid_data(format!(
                    "invalid delete journal entry kind: {value}"
                )))
            }
        };
        let path_len = self.file.read_u32().await? as usize;
        if path_len != payload_len - RECORD_HEADER_LEN {
            return Err(invalid_data(format!(
                "delete journal path length {path_len} does not match record length {payload_len}"
            )));
        }

        let mut path = vec![0_u8; path_len];
        self.file.read_exact(&mut path).await?;
        let path = decode_path(path)?;

        self.cursor = start;
        self.remaining = self
            .remaining
            .checked_sub(1)
            .ok_or_else(|| invalid_data("delete journal contains more records than expected"))?;

        Ok(Some(DeleteRecord { path, kind }))
    }
}

fn join_error(error: tokio::task::JoinError) -> io::Error {
    io::Error::other(error.to_string())
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(unix)]
fn encode_path(path: &OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_bytes().to_vec()
}

#[cfg(unix)]
fn decode_path(path: Vec<u8>) -> Result<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    Ok(PathBuf::from(OsString::from_vec(path)))
}

#[cfg(windows)]
fn encode_path(path: &OsStr) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;

    let mut bytes = Vec::new();
    for unit in path.encode_wide() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes
}

#[cfg(windows)]
fn decode_path(path: Vec<u8>) -> Result<PathBuf> {
    use std::os::windows::ffi::OsStringExt;

    if path.len() % 2 != 0 {
        return Err(invalid_data(
            "odd byte length in Windows delete journal path",
        ));
    }
    let wide = path
        .chunks_exact(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .collect::<Vec<_>>();
    Ok(PathBuf::from(OsString::from_wide(&wide)))
}

#[cfg(not(any(unix, windows)))]
fn encode_path(path: &OsStr) -> Vec<u8> {
    path.to_string_lossy().into_owned().into_bytes()
}

#[cfg(not(any(unix, windows)))]
fn decode_path(path: Vec<u8>) -> Result<PathBuf> {
    let path = String::from_utf8(path)
        .map_err(|error| invalid_data(format!("invalid delete journal path encoding: {error}")))?;
    Ok(PathBuf::from(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn replays_records_in_reverse_order() {
        let mut journal = DeleteJournal::new().await.unwrap();
        journal
            .append(Path::new("parent"), DeleteKind::Directory)
            .await
            .unwrap();
        journal
            .append(Path::new("parent"), DeleteKind::ProtectDirectory)
            .await
            .unwrap();
        journal
            .append(Path::new("parent/file"), DeleteKind::FileLike)
            .await
            .unwrap();

        assert_eq!(journal.records, 3);

        let mut reader = journal.seal().await.unwrap();
        assert_eq!(reader.remaining, 3);
        assert_eq!(
            reader.next().await.unwrap(),
            Some(DeleteRecord {
                path: PathBuf::from("parent/file"),
                kind: DeleteKind::FileLike,
            })
        );
        assert_eq!(
            reader.next().await.unwrap(),
            Some(DeleteRecord {
                path: PathBuf::from("parent"),
                kind: DeleteKind::ProtectDirectory,
            })
        );
        assert_eq!(
            reader.next().await.unwrap(),
            Some(DeleteRecord {
                path: PathBuf::from("parent"),
                kind: DeleteKind::Directory,
            })
        );
        assert_eq!(reader.next().await.unwrap(), None);
        assert_eq!(reader.remaining, 0);
    }

    #[tokio::test]
    async fn empty_journal_replays_nothing() {
        let journal = DeleteJournal::new().await.unwrap();
        assert_eq!(journal.records, 0);
        let mut reader = journal.seal().await.unwrap();
        assert_eq!(reader.next().await.unwrap(), None);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn preserves_non_utf8_paths() {
        use std::os::unix::ffi::OsStringExt;

        let path = PathBuf::from(OsString::from_vec(vec![b'f', 0x80, b'o']));
        let mut journal = DeleteJournal::new().await.unwrap();
        journal.append(&path, DeleteKind::FileLike).await.unwrap();
        let mut reader = journal.seal().await.unwrap();
        assert_eq!(reader.next().await.unwrap().unwrap().path, path);
    }

    #[tokio::test]
    async fn rejects_corrupt_trailing_record_length() {
        let mut journal = DeleteJournal::new().await.unwrap();
        journal
            .append(Path::new("file"), DeleteKind::FileLike)
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

        let error = reader.next().await.unwrap_err();
        assert!(error.to_string().contains("record length"));
    }
}
