use crate::engine::bisync_state::{
    BaselineRecord, BisyncStateError, CurrentPointer, GenerationHeader, GenerationId,
    GenerationReader, GenerationSummary, GenerationWriter, PolicyFingerprint, RecoveryMarker,
};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

const LOCK_FILE: &str = "lock";
const CURRENT_FILE: &str = "current";
const CURRENT_TEMP_FILE: &str = ".current.tmp";
const RECOVERY_FILE: &str = "recovery";
const RECOVERY_TEMP_FILE: &str = ".recovery.tmp";

#[derive(Debug, thiserror::Error)]
pub enum BisyncStoreError {
    #[error(transparent)]
    Io(#[from] io::Error),

    #[error(transparent)]
    State(#[from] BisyncStateError),

    #[error("another bidirectional sync is already using this state directory")]
    Busy,

    #[error("bidirectional state storage is not implemented on this platform")]
    UnsupportedPlatform,

    #[error("bidirectional sync recovery is required before another run can start")]
    RecoveryRequired,

    #[error("requested base generation does not match the current trusted generation")]
    BaseGenerationMismatch,

    #[error("recovery target generation does not match the generation being committed")]
    TargetGenerationMismatch,

    #[error("generation {0} already exists")]
    GenerationExists(u64),

    #[error("current pointer digest does not match generation {generation}")]
    PointerDigestMismatch { generation: u64 },

    #[error("recovery marker and current pointer describe an inconsistent state")]
    InconsistentRecovery,
}

pub type Result<T> = std::result::Result<T, BisyncStoreError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryState {
    Clean,
    /// Replica mutation may have started, but the durable baseline did not
    /// advance. Automatic guessing is unsafe; the controller must require an
    /// explicit recovery/resync path.
    Interrupted(RecoveryMarker),
    /// The durable pointer advanced to the target generation. The only crash
    /// residue is the marker itself and it may be removed after verification.
    Committed(RecoveryMarker),
}

/// Exclusive, crash-aware storage for one bidirectional-sync pair.
///
/// All methods perform blocking filesystem operations. Async callers must use
/// a blocking worker. The held file lock serializes writers for the lifetime of
/// this value; no file-handle clones are exposed because locking a clone of an
/// already locked file has platform-dependent behavior.
pub struct BisyncStateStore {
    directory: PathBuf,
    _lock: File,
}

impl BisyncStateStore {
    pub fn open(directory: impl Into<PathBuf>) -> Result<Self> {
        #[cfg(not(unix))]
        {
            let _ = directory;
            return Err(BisyncStoreError::UnsupportedPlatform);
        }

        #[cfg(unix)]
        {
            let directory = directory.into();
            fs::create_dir_all(&directory)?;
            let lock_path = directory.join(LOCK_FILE);
            let lock = OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .truncate(false)
                .open(lock_path)?;
            match lock.try_lock() {
                Ok(()) => {}
                Err(TryLockError::WouldBlock) => return Err(BisyncStoreError::Busy),
                Err(TryLockError::Error(error)) => return Err(error.into()),
            }
            Ok(Self {
                directory,
                _lock: lock,
            })
        }
    }

    pub fn current_pointer(&self) -> Result<Option<CurrentPointer>> {
        read_optional_exact(&self.directory.join(CURRENT_FILE))?
            .map(|bytes| CurrentPointer::decode(&bytes).map_err(Into::into))
            .transpose()
    }

    pub fn recovery_marker(&self) -> Result<Option<RecoveryMarker>> {
        read_optional_exact(&self.directory.join(RECOVERY_FILE))?
            .map(|bytes| RecoveryMarker::decode(&bytes).map_err(Into::into))
            .transpose()
    }

    /// Determine whether a stale recovery marker means the prior transaction
    /// committed or stopped after mutation began.
    pub fn recovery_state(&self) -> Result<RecoveryState> {
        let Some(marker) = self.recovery_marker()? else {
            return Ok(RecoveryState::Clean);
        };
        let current = self.current_pointer()?;

        if let Some(pointer) = current {
            if pointer.generation == marker.target_generation {
                self.verify_pointer(pointer)?;
                return Ok(RecoveryState::Committed(marker));
            }
        }

        if current.map(|pointer| pointer.generation) == marker.base_generation {
            return Ok(RecoveryState::Interrupted(marker));
        }

        if current.is_none() && marker.base_generation.is_none() {
            return Ok(RecoveryState::Interrupted(marker));
        }

        Err(BisyncStoreError::InconsistentRecovery)
    }

    /// Remove a stale recovery marker only when the target generation is fully
    /// installed and verified. Interrupted mutations intentionally remain
    /// blocked for an explicit recovery/resync operation.
    pub fn clean_committed_recovery(&self) -> Result<bool> {
        match self.recovery_state()? {
            RecoveryState::Committed(_) => {
                remove_if_exists(&self.directory.join(RECOVERY_FILE))?;
                sync_directory(&self.directory)?;
                Ok(true)
            }
            RecoveryState::Clean | RecoveryState::Interrupted(_) => Ok(false),
        }
    }

    /// Prepare an interrupted pair for an explicit recovery/resync pass.
    ///
    /// A crash may leave an immutable target generation after its rename but
    /// before the current pointer advances. That generation is not trusted: the
    /// replicas may have been only partially mutated. Remove target/temp state
    /// so the recovery pass can reuse the reserved generation id, but retain the
    /// durable recovery marker until a verified resync commits successfully.
    pub fn prepare_interrupted_resync(&self) -> Result<RecoveryMarker> {
        let marker = match self.recovery_state()? {
            RecoveryState::Interrupted(marker) => marker,
            RecoveryState::Clean | RecoveryState::Committed(_) => {
                return Err(BisyncStoreError::RecoveryRequired);
            }
        };

        let current_generation = self.current_pointer()?.map(|pointer| pointer.generation);
        if current_generation != marker.base_generation {
            return Err(BisyncStoreError::BaseGenerationMismatch);
        }

        remove_if_exists(&self.generation_temp_path(marker.target_generation))?;
        remove_if_exists(&self.generation_path(marker.target_generation))?;
        remove_if_exists(&self.directory.join(CURRENT_TEMP_FILE))?;
        sync_directory(&self.directory)?;
        Ok(marker)
    }

    /// Start a mutation transaction by durably recording base -> target before
    /// any replica change is allowed.
    pub fn begin_run(&self, base_generation: Option<GenerationId>) -> Result<RecoveryMarker> {
        if self.recovery_marker()?.is_some() {
            return Err(BisyncStoreError::RecoveryRequired);
        }

        let current_generation = self.current_pointer()?.map(|pointer| pointer.generation);
        if current_generation != base_generation {
            return Err(BisyncStoreError::BaseGenerationMismatch);
        }

        let target_generation = match base_generation {
            Some(generation) => generation.next()?,
            None => GenerationId::FIRST,
        };
        let marker = RecoveryMarker {
            base_generation,
            target_generation,
        };
        self.install_small_atomic(RECOVERY_TEMP_FILE, RECOVERY_FILE, &marker.encode())?;
        Ok(marker)
    }

    /// Commit a new immutable baseline and switch the trusted pointer.
    ///
    /// Ordering is:
    /// 1. write + strong-sync immutable generation;
    /// 2. rename generation + sync state directory;
    /// 3. write + strong-sync current pointer;
    /// 4. rename pointer + sync directory;
    /// 5. remove recovery marker + sync directory.
    ///
    /// A crash before step 4 leaves the old baseline plus a recovery marker. A
    /// crash after step 4 is recognizable as committed and cleanup-only.
    pub fn commit_generation<I>(
        &self,
        marker: RecoveryMarker,
        policy: PolicyFingerprint,
        records: I,
    ) -> Result<GenerationSummary>
    where
        I: IntoIterator<Item = BaselineRecord>,
    {
        let durable_marker = self
            .recovery_marker()?
            .ok_or(BisyncStoreError::RecoveryRequired)?;
        if durable_marker != marker || durable_marker.target_generation != marker.target_generation
        {
            return Err(BisyncStoreError::TargetGenerationMismatch);
        }

        let current_generation = self.current_pointer()?.map(|pointer| pointer.generation);
        if current_generation != marker.base_generation {
            return Err(BisyncStoreError::BaseGenerationMismatch);
        }

        let summary = self.write_generation(
            GenerationHeader {
                generation: marker.target_generation,
                policy,
            },
            records,
        )?;
        let pointer = CurrentPointer {
            generation: summary.generation,
            generation_digest: summary.digest,
        };
        self.install_small_atomic(CURRENT_TEMP_FILE, CURRENT_FILE, &pointer.encode())?;
        remove_if_exists(&self.directory.join(RECOVERY_FILE))?;
        sync_directory(&self.directory)?;
        Ok(summary)
    }

    pub fn open_current(&self) -> Result<Option<TrustedGenerationReader>> {
        let Some(pointer) = self.current_pointer()? else {
            return Ok(None);
        };
        let path = self.generation_path(pointer.generation);
        let reader = GenerationReader::new(File::open(path)?)?;
        if reader.header().generation != pointer.generation {
            return Err(BisyncStoreError::PointerDigestMismatch {
                generation: pointer.generation.get(),
            });
        }
        Ok(Some(TrustedGenerationReader {
            reader,
            pointer,
            verified: false,
        }))
    }

    fn verify_pointer(&self, pointer: CurrentPointer) -> Result<()> {
        let mut reader = self
            .open_current()?
            .ok_or(BisyncStoreError::PointerDigestMismatch {
                generation: pointer.generation.get(),
            })?;
        while reader.next_record()?.is_some() {}
        if reader.pointer() != pointer {
            return Err(BisyncStoreError::InconsistentRecovery);
        }
        Ok(())
    }

    fn write_generation<I>(&self, header: GenerationHeader, records: I) -> Result<GenerationSummary>
    where
        I: IntoIterator<Item = BaselineRecord>,
    {
        let final_path = self.generation_path(header.generation);
        if fs::exists(&final_path)? {
            return Err(BisyncStoreError::GenerationExists(header.generation.get()));
        }
        let temp_path = self.generation_temp_path(header.generation);
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&temp_path)?;

        let mut writer = GenerationWriter::new(&mut file, header)?;
        for record in records {
            writer.write_record(&record)?;
        }
        let (_, summary) = writer.finish()?;
        strong_sync_file(&file)?;
        drop(file);

        fs::rename(&temp_path, &final_path)?;
        sync_directory(&self.directory)?;
        Ok(summary)
    }

    fn install_small_atomic(&self, temp_name: &str, final_name: &str, bytes: &[u8]) -> Result<()> {
        let temp = self.directory.join(temp_name);
        let final_path = self.directory.join(final_name);
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.flush()?;
        strong_sync_file(&file)?;
        drop(file);
        fs::rename(temp, final_path)?;
        sync_directory(&self.directory)?;
        Ok(())
    }

    fn generation_path(&self, generation: GenerationId) -> PathBuf {
        self.directory
            .join(format!("generation-{:020}.bin", generation.get()))
    }

    fn generation_temp_path(&self, generation: GenerationId) -> PathBuf {
        self.directory
            .join(format!(".generation-{:020}.tmp", generation.get()))
    }
}

pub struct TrustedGenerationReader {
    reader: GenerationReader<File>,
    pointer: CurrentPointer,
    verified: bool,
}

impl TrustedGenerationReader {
    pub const fn header(&self) -> GenerationHeader {
        self.reader.header()
    }

    pub const fn pointer(&self) -> CurrentPointer {
        self.pointer
    }

    pub const fn is_verified(&self) -> bool {
        self.verified
    }

    /// Read the next baseline record. `Ok(None)` means both the generation's
    /// own checksum and the current pointer's digest have been verified.
    pub fn next_record(&mut self) -> Result<Option<BaselineRecord>> {
        let record = self.reader.next_record()?;
        if record.is_none() && !self.verified {
            let summary =
                self.reader
                    .verified_summary()
                    .ok_or(BisyncStoreError::PointerDigestMismatch {
                        generation: self.pointer.generation.get(),
                    })?;
            if summary.generation != self.pointer.generation
                || summary.digest != self.pointer.generation_digest
            {
                return Err(BisyncStoreError::PointerDigestMismatch {
                    generation: self.pointer.generation.get(),
                });
            }
            self.verified = true;
        }
        Ok(record)
    }
}

fn read_optional_exact(path: &Path) -> Result<Option<Vec<u8>>> {
    match File::open(path) {
        Ok(mut file) => {
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)?;
            Ok(Some(bytes))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn remove_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<()> {
    let directory = File::open(path)?;
    directory.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<()> {
    Err(BisyncStoreError::UnsupportedPlatform)
}

#[cfg(target_os = "macos")]
fn strong_sync_file(file: &File) -> Result<()> {
    use std::os::fd::AsRawFd;

    file.sync_all()?;
    // SAFETY: `file` owns a live descriptor for the duration of this call;
    // F_FULLFSYNC takes no pointer arguments and does not retain the fd.
    let result = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_FULLFSYNC) };
    if result == -1 {
        Err(io::Error::last_os_error().into())
    } else {
        Ok(())
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn strong_sync_file(file: &File) -> Result<()> {
    file.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn strong_sync_file(_file: &File) -> Result<()> {
    Err(BisyncStoreError::UnsupportedPlatform)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::bisync::ValueId;
    use crate::engine::bisync_state::NamespaceKey;
    use crate::engine::domain::EntryIdentity;

    const POLICY: PolicyFingerprint = PolicyFingerprint::from_bytes([7; 32]);

    fn record(path: &[u8], value: u8) -> BaselineRecord {
        BaselineRecord {
            key: NamespaceKey::new(path.to_vec()).unwrap(),
            value: ValueId::from_bytes([value; 32]),
            left_identity: Some(EntryIdentity::from_bytes([value + 1; 32])),
            right_identity: Some(EntryIdentity::from_bytes([value + 2; 32])),
        }
    }

    #[test]
    #[cfg(unix)]
    fn lock_contention_fails_fast() {
        let temp = tempfile::TempDir::new().unwrap();
        let _first = BisyncStateStore::open(temp.path()).unwrap();
        assert!(matches!(
            BisyncStateStore::open(temp.path()),
            Err(BisyncStoreError::Busy)
        ));
    }

    #[test]
    #[cfg(unix)]
    fn begin_commit_and_read_verified_generation() {
        let temp = tempfile::TempDir::new().unwrap();
        let store = BisyncStateStore::open(temp.path()).unwrap();
        let marker = store.begin_run(None).unwrap();
        assert_eq!(marker.target_generation, GenerationId::FIRST);
        assert!(matches!(
            store.recovery_state().unwrap(),
            RecoveryState::Interrupted(found) if found == marker
        ));

        let summary = store
            .commit_generation(marker, POLICY, [record(b"a", 1), record(b"nested/b", 2)])
            .unwrap();
        assert_eq!(summary.generation, GenerationId::FIRST);
        assert_eq!(store.recovery_state().unwrap(), RecoveryState::Clean);

        let mut reader = store.open_current().unwrap().unwrap();
        assert_eq!(reader.header().policy, POLICY);
        assert!(!reader.is_verified());
        assert_eq!(reader.next_record().unwrap(), Some(record(b"a", 1)));
        assert_eq!(reader.next_record().unwrap(), Some(record(b"nested/b", 2)));
        assert_eq!(reader.next_record().unwrap(), None);
        assert!(reader.is_verified());
    }

    #[test]
    #[cfg(unix)]
    fn interrupted_run_blocks_new_run() {
        let temp = tempfile::TempDir::new().unwrap();
        {
            let store = BisyncStateStore::open(temp.path()).unwrap();
            let marker = store.begin_run(None).unwrap();
            assert!(matches!(
                store.recovery_state().unwrap(),
                RecoveryState::Interrupted(found) if found == marker
            ));
        }

        let store = BisyncStateStore::open(temp.path()).unwrap();
        assert!(matches!(
            store.begin_run(None),
            Err(BisyncStoreError::RecoveryRequired)
        ));
    }

    #[test]
    #[cfg(unix)]
    fn committed_crash_marker_is_cleanup_only_after_generation_verification() {
        let temp = tempfile::TempDir::new().unwrap();
        let store = BisyncStateStore::open(temp.path()).unwrap();
        let marker = store.begin_run(None).unwrap();
        store
            .commit_generation(marker, POLICY, [record(b"a", 1)])
            .unwrap();

        store
            .install_small_atomic(RECOVERY_TEMP_FILE, RECOVERY_FILE, &marker.encode())
            .unwrap();
        assert!(matches!(
            store.recovery_state().unwrap(),
            RecoveryState::Committed(found) if found == marker
        ));
        assert!(store.clean_committed_recovery().unwrap());
        assert_eq!(store.recovery_state().unwrap(), RecoveryState::Clean);
    }

    #[test]
    #[cfg(unix)]
    fn corrupt_generation_cannot_turn_stale_marker_into_committed_state() {
        let temp = tempfile::TempDir::new().unwrap();
        let store = BisyncStateStore::open(temp.path()).unwrap();
        let marker = store.begin_run(None).unwrap();
        let summary = store
            .commit_generation(marker, POLICY, [record(b"a", 1)])
            .unwrap();
        store
            .install_small_atomic(RECOVERY_TEMP_FILE, RECOVERY_FILE, &marker.encode())
            .unwrap();

        let generation_path = store.generation_path(summary.generation);
        let mut bytes = fs::read(&generation_path).unwrap();
        let index = bytes.len() - 1;
        bytes[index] ^= 1;
        fs::write(generation_path, bytes).unwrap();

        assert!(matches!(
            store.recovery_state(),
            Err(BisyncStoreError::State(BisyncStateError::ChecksumMismatch))
                | Err(BisyncStoreError::PointerDigestMismatch { .. })
        ));
    }

    #[test]
    #[cfg(unix)]
    fn interrupted_resync_discards_untrusted_target_but_keeps_marker() {
        let temp = tempfile::TempDir::new().unwrap();
        let store = BisyncStateStore::open(temp.path()).unwrap();
        let marker = store.begin_run(None).unwrap();

        let target = store.generation_path(marker.target_generation);
        let target_temp = store.generation_temp_path(marker.target_generation);
        let current_temp = store.directory.join(CURRENT_TEMP_FILE);
        fs::write(&target, b"untrusted target").unwrap();
        fs::write(&target_temp, b"partial target").unwrap();
        fs::write(&current_temp, b"partial pointer").unwrap();

        assert_eq!(store.prepare_interrupted_resync().unwrap(), marker);
        assert!(!target.exists());
        assert!(!target_temp.exists());
        assert!(!current_temp.exists());
        assert_eq!(store.recovery_marker().unwrap(), Some(marker));
        assert!(matches!(
            store.recovery_state().unwrap(),
            RecoveryState::Interrupted(found) if found == marker
        ));

        store
            .commit_generation(marker, POLICY, [record(b"a", 1)])
            .unwrap();
        assert_eq!(store.recovery_state().unwrap(), RecoveryState::Clean);
    }

    #[test]
    #[cfg(unix)]
    fn generation_advances_monotonically() {
        let temp = tempfile::TempDir::new().unwrap();
        let store = BisyncStateStore::open(temp.path()).unwrap();

        let first = store.begin_run(None).unwrap();
        store
            .commit_generation(first, POLICY, [record(b"a", 1)])
            .unwrap();
        let second = store.begin_run(Some(GenerationId::FIRST)).unwrap();
        assert_eq!(
            second.target_generation,
            GenerationId::FIRST.next().unwrap()
        );
        store
            .commit_generation(second, POLICY, [record(b"a", 2)])
            .unwrap();

        assert_eq!(
            store.current_pointer().unwrap().unwrap().generation,
            GenerationId::FIRST.next().unwrap()
        );
    }
}
