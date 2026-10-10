//! Positioned plan reads: cloned descriptors must never share a seek cursor.
use super::{decode_operation, PlanJournalError, Result, MAX_RECORD_PAYLOAD};
use crate::engine::domain::SyncOp;
use std::fs::File;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub(super) const HEADER_BYTES: u64 = 12;

/// An append-time record commitment, not permission to read an arbitrary offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PlanRecordPosition {
    pub(crate) offset: u64,
    pub(crate) ordinal: u64,
    pub(crate) payload_bytes: u32,
    pub(crate) digest: [u8; 32],
}

pub(crate) struct PlanRecord {
    pub(crate) position: PlanRecordPosition,
    pub(crate) operation: SyncOp,
}

pub struct PlanJournalReader {
    file: Arc<File>,
    healthy: Arc<AtomicBool>,
    records: u64,
    end: u64,
    ordinal: u64,
    offset: u64,
    manifest: super::manifest::ManifestReader,
}

impl PlanJournalReader {
    pub(super) fn new(
        file: File,
        records: u64,
        end: u64,
        manifest: super::manifest::ManifestReader,
    ) -> Self {
        Self {
            file: Arc::new(file),
            healthy: Arc::new(AtomicBool::new(true)),
            records,
            end,
            ordinal: 0,
            offset: 0,
            manifest,
        }
    }

    fn ensure_healthy(&self) -> Result<()> {
        if self.healthy.load(Ordering::Relaxed) {
            Ok(())
        } else {
            Err(PlanJournalError::InvalidRecord("poisoned plan journal"))
        }
    }

    pub(crate) fn rewind(&mut self) -> Result<()> {
        self.ensure_healthy()?;
        self.ordinal = 0;
        self.offset = 0;
        Ok(())
    }

    pub async fn next(&mut self) -> Result<Option<SyncOp>> {
        Ok(self.next_record().await?.map(|record| record.operation))
    }

    pub(crate) async fn next_record(&mut self) -> Result<Option<PlanRecord>> {
        self.ensure_healthy()?;
        if self.ordinal == self.records {
            let file = Arc::clone(&self.file);
            let end = self.end;
            let offset = self.offset;
            self.join_read(move || {
                if offset != end || file.metadata()?.len() != end {
                    return Err(PlanJournalError::InvalidRecord(
                        "trailing bytes after final plan journal record",
                    ));
                }
                Ok(())
            })
            .await?;
            return Ok(None);
        }
        let file = Arc::clone(&self.file);
        let (offset, ordinal, end) = (self.offset, self.ordinal, self.end);
        let manifest = self.manifest.clone();
        let record = self
            .join_read(move || {
                let position = manifest.position(ordinal)?;
                if position.offset != offset {
                    return Err(PlanJournalError::InvalidRecord(
                        "manifest position differs from sequential replay",
                    ));
                }
                read_record(&file, position, end)
            })
            .await?;
        let next = record.position.offset + HEADER_BYTES + u64::from(record.position.payload_bytes);
        if self.ordinal + 1 == self.records && next != self.end {
            self.healthy.store(false, Ordering::Relaxed);
            return Err(PlanJournalError::InvalidRecord(
                "trailing bytes after final plan journal record",
            ));
        }
        self.ordinal += 1;
        self.offset = next;
        Ok(Some(record))
    }

    /// Check the original ordinal, length and payload seal without changing
    /// sequential replay state. All blocking reads use explicit file offsets.
    pub(crate) async fn read_at(&self, position: PlanRecordPosition) -> Result<SyncOp> {
        self.ensure_healthy()?;
        let file = Arc::clone(&self.file);
        let (records, end) = (self.records, self.end);
        let manifest = self.manifest.clone();
        self.join_read(move || {
            if position.ordinal >= records {
                return Err(PlanJournalError::InvalidRecord(
                    "plan position ordinal outside sealed records",
                ));
            }
            if manifest.position(position.ordinal)? != position {
                return Err(PlanJournalError::InvalidRecord(
                    "position differs from append-time manifest",
                ));
            }
            Ok(read_record(&file, position, end)?.operation)
        })
        .await
    }

    async fn join_read<T: Send + 'static>(
        &self,
        read: impl FnOnce() -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let healthy = Arc::clone(&self.healthy);
        // The worker poisons failed reads even when its joining future is
        // cancelled. Successful positional reads do not mutate replay state.
        let result = tokio::task::spawn_blocking(move || {
            let result = read();
            if result.is_err() {
                healthy.store(false, Ordering::Relaxed);
            }
            result
        })
        .await;
        match result {
            Ok(result) => result,
            Err(error) => {
                self.healthy.store(false, Ordering::Relaxed);
                Err(PlanJournalError::Worker(error.to_string()))
            }
        }
    }

    #[cfg(test)]
    pub(super) fn manifest_file(&self) -> &File {
        self.manifest.file()
    }

    #[cfg(test)]
    pub(super) fn file(&self) -> &File {
        &self.file
    }
}

fn read_record(file: &File, expected: PlanRecordPosition, end: u64) -> Result<PlanRecord> {
    let (offset, ordinal) = (expected.offset, expected.ordinal);
    if file.metadata()?.len() != end {
        return Err(PlanJournalError::InvalidRecord(
            "plan length changed after sealing",
        ));
    }
    let payload_start = offset
        .checked_add(HEADER_BYTES)
        .filter(|start| *start <= end)
        .ok_or(PlanJournalError::InvalidRecord(
            "plan position outside sealed extent",
        ))?;
    let mut header = [0; HEADER_BYTES as usize];
    read_exact_at(file, &mut header, offset)?;
    let payload_bytes = u32::from_be_bytes(
        header[..4]
            .try_into()
            .map_err(|_| PlanJournalError::InvalidRecord("invalid record length"))?,
    );
    if payload_bytes == 0 {
        return Err(PlanJournalError::EmptyRecord);
    }
    if payload_bytes as usize > MAX_RECORD_PAYLOAD {
        return Err(PlanJournalError::RecordTooLarge {
            actual: payload_bytes as usize,
            maximum: MAX_RECORD_PAYLOAD,
        });
    }
    if u64::from_be_bytes(
        header[4..]
            .try_into()
            .map_err(|_| PlanJournalError::InvalidRecord("invalid ordinal"))?,
    ) != ordinal
    {
        return Err(PlanJournalError::InvalidRecord(
            "plan record ordinal changed",
        ));
    }
    if payload_start
        .checked_add(u64::from(payload_bytes))
        .is_none_or(|next| next > end)
    {
        return Err(PlanJournalError::InvalidRecord(
            "plan payload outside sealed extent",
        ));
    }
    if expected.payload_bytes != payload_bytes {
        return Err(PlanJournalError::InvalidRecord(
            "plan record length changed",
        ));
    }
    let mut payload = vec![0; payload_bytes as usize];
    read_exact_at(file, &mut payload, payload_start)?;
    let digest = *blake3::hash(&payload).as_bytes();
    if expected.digest != digest {
        return Err(PlanJournalError::InvalidRecord(
            "plan record payload changed",
        ));
    }
    let operation = decode_operation(&payload)?;
    Ok(PlanRecord {
        position: PlanRecordPosition {
            offset,
            ordinal,
            payload_bytes,
            digest,
        },
        operation,
    })
}

pub(super) fn read_exact_at(file: &File, mut bytes: &mut [u8], mut offset: u64) -> io::Result<()> {
    while !bytes.is_empty() {
        #[cfg(unix)]
        let read = {
            use std::os::unix::fs::FileExt;
            file.read_at(bytes, offset)
        };
        #[cfg(windows)]
        let read = {
            use std::os::windows::fs::FileExt;
            file.seek_read(bytes, offset)
        };
        #[cfg(not(any(unix, windows)))]
        let read: io::Result<usize> = Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "positioned plan reads unsupported",
        ));
        match read {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "truncated plan record",
                ))
            }
            Ok(count) => {
                offset = offset
                    .checked_add(count as u64)
                    .ok_or_else(|| io::Error::other("plan offset overflow"))?;
                bytes = &mut bytes[count..];
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}
