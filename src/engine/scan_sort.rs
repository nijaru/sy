//! Bounded native-name sorting. Every spilled record is authenticated before
//! decoding; sealed descriptors, not scratch EOF, authorize run exhaustion.
use super::native_path;
use std::cell::Cell;
use std::ffi::OsString;
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use tempfile::TempDir;

#[path = "scan_stack.rs"]
pub(crate) mod stack;

const MAX_NAME_BYTES: usize = crate::protocol::MAX_WIRE_COMPONENT_BYTES;
const HEADER_BYTES: u64 = 72;

#[cfg(test)]
thread_local! {
    // Scoped to the native scanner thread; no production corruption interface.
    static TRUNCATE_SORTED_RUN: Cell<bool> = const { Cell::new(false) };
}
#[cfg(test)]
pub(crate) fn truncate_next_sorted_run() {
    TRUNCATE_SORTED_RUN.set(true);
}

pub(crate) fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "scan scratch authentication/state failure",
    )
}

pub(crate) fn unsafe_scratch() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "temporary directory is inside scanned root; configure temporary storage outside the source",
    )
}

/// Compare the scratch parent's actual ancestry to the held scan root, not its
/// pathname (which may have been renamed or replaced). This is a write-location
/// guard, not a substitute for descriptor-rooted scan confinement. Concurrent
/// external moves of the temporary directory remain outside this guarantee.
#[cfg(unix)]
pub(crate) fn validate_scratch_root(root: &File, scratch: &Path) -> io::Result<()> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let identity = |file: &File| -> io::Result<_> {
        let metadata = file.metadata()?;
        Ok((metadata.dev(), metadata.ino()))
    };
    let root_identity = identity(root)?;
    let mut directory = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
        .open(scratch)?;
    loop {
        let current = identity(&directory)?;
        if current == root_identity {
            return Err(unsafe_scratch());
        }
        // SAFETY: directory owns a live FD; '..' is a NUL-terminated component.
        // Successful openat returns a fresh owned directory descriptor.
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                c"..".as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful openat returned a fresh exclusively owned FD.
        let parent = unsafe { File::from_raw_fd(fd) };
        if identity(&parent)? == current {
            return Ok(());
        }
        directory = parent;
    }
}

#[derive(Clone)]
pub(crate) struct Health(Rc<Cell<bool>>);
impl Health {
    fn new() -> Self {
        Self(Rc::new(Cell::new(true)))
    }
    pub fn check(&self) -> io::Result<()> {
        if self.0.get() {
            Ok(())
        } else {
            Err(invalid())
        }
    }
    pub fn finish<T>(&self, result: io::Result<T>) -> io::Result<T> {
        if result.is_err() {
            self.0.set(false);
        }
        result
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RunKey {
    id: u64,
    pass: u64,
    index: u64,
}
impl RunKey {
    fn bytes(self) -> [u8; 24] {
        let mut bytes = [0; 24];
        for (slot, value) in bytes
            .as_chunks_mut::<8>()
            .0
            .iter_mut()
            .zip([self.id, self.pass, self.index])
        {
            *slot = value.to_le_bytes();
        }
        bytes
    }
}

/// Only constructed from native writes or an authenticated continuation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RunCursor {
    key: RunKey,
    count: u64,
    end: u64,
    offset: u64,
    ordinal: u64,
}
impl RunCursor {
    pub(super) fn values(self) -> [u64; 7] {
        [
            self.key.id,
            self.key.pass,
            self.key.index,
            self.count,
            self.end,
            self.offset,
            self.ordinal,
        ]
    }
    pub(super) fn from_values(v: [u64; 7]) -> Self {
        Self {
            key: RunKey {
                id: v[0],
                pass: v[1],
                index: v[2],
            },
            count: v[3],
            end: v[4],
            offset: v[5],
            ordinal: v[6],
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct SortBudget {
    pub bytes: usize,
    pub records: usize,
    pub fan_in: usize,
}
impl Default for SortBudget {
    fn default() -> Self {
        Self {
            bytes: 1024 * 1024,
            records: 4096,
            fan_in: 16,
        }
    }
}

pub(crate) struct NameSpool {
    scratch: TempDir,
    secret: [u8; 32],
    health: Health,
    next_id: u64,
    budget: SortBudget,
    #[cfg(test)]
    pub peak_records: usize,
    #[cfg(test)]
    pub peak_bytes: usize,
    #[cfg(test)]
    pub peak_readers: usize,
}

impl NameSpool {
    #[cfg(test)]
    pub fn new(budget: SortBudget) -> io::Result<Self> {
        Self::new_in(budget, &tempfile::env::temp_dir())
    }
    pub fn new_in(budget: SortBudget, parent: &Path) -> io::Result<Self> {
        if budget.bytes == 0 || budget.records == 0 || budget.fan_in < 2 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid scan sort budget",
            ));
        }
        let secret = scan_secret()?;
        Ok(Self {
            scratch: tempfile::Builder::new()
                .prefix("sy-scan-")
                .tempdir_in(parent)?,
            secret,
            health: Health::new(),
            next_id: 0,
            budget,
            #[cfg(test)]
            peak_records: 0,
            #[cfg(test)]
            peak_bytes: 0,
            #[cfg(test)]
            peak_readers: 0,
        })
    }
    pub fn health(&self) -> Health {
        self.health.clone()
    }
    pub fn sort(
        &mut self,
        names: impl IntoIterator<Item = io::Result<OsString>>,
        cancelled: impl Fn() -> bool,
    ) -> io::Result<RunCursor> {
        self.health.check()?;
        let result = self.sort_inner(names, &cancelled);
        self.health.finish(result)
    }
    fn sort_inner(
        &mut self,
        names: impl IntoIterator<Item = io::Result<OsString>>,
        cancelled: &impl Fn() -> bool,
    ) -> io::Result<RunCursor> {
        let id = self.next_id;
        self.next_id = id.checked_add(1).ok_or_else(invalid)?;
        let mut buffer = Vec::new();
        let mut bytes = 0;
        let mut count = 0_u64;
        let mut last = None;
        for name in names {
            check_cancelled(cancelled)?;
            let name = name?;
            let raw = native_path::encode(&name);
            validate_name(&raw)?;
            let charge = raw.len() + std::mem::size_of::<OsString>();
            if !buffer.is_empty()
                && (buffer.len() >= self.budget.records || bytes + charge > self.budget.bytes)
            {
                last = Some(self.flush_run(
                    RunKey {
                        id,
                        pass: 0,
                        index: count,
                    },
                    &mut buffer,
                )?);
                count = count.checked_add(1).ok_or_else(invalid)?;
                bytes = 0;
            }
            bytes += charge;
            buffer.push(name);
            #[cfg(test)]
            {
                self.peak_records = self.peak_records.max(buffer.len());
                self.peak_bytes = self.peak_bytes.max(bytes);
            }
        }
        check_cancelled(cancelled)?;
        if !buffer.is_empty() || count == 0 {
            last = Some(self.flush_run(
                RunKey {
                    id,
                    pass: 0,
                    index: count,
                },
                &mut buffer,
            )?);
            count = count.checked_add(1).ok_or_else(invalid)?;
        }
        let mut pass = 0_u64;
        while count > 1 {
            check_cancelled(cancelled)?;
            let next_pass = pass.checked_add(1).ok_or_else(invalid)?;
            let mut start = 0;
            let mut output = 0;
            while start < count {
                let end = count.min(start.saturating_add(self.budget.fan_in as u64));
                last = Some(self.merge(id, pass, start..end, next_pass, output, cancelled)?);
                start = end;
                output += 1;
            }
            count = output;
            pass = next_pass;
        }
        let run = last.ok_or_else(invalid)?;
        std::fs::rename(self.run_path(run.key), self.names_path(run.key.id))?;
        #[cfg(test)]
        if TRUNCATE_SORTED_RUN.replace(false) {
            File::options()
                .write(true)
                .open(self.names_path(run.key.id))?
                .set_len(0)?;
        }
        Ok(run)
    }
    fn writer(&self, key: RunKey) -> io::Result<RunWriter> {
        RunWriter::new(self.run_path(key), key, self.secret)
    }
    fn flush_run(&self, key: RunKey, buffer: &mut Vec<OsString>) -> io::Result<RunCursor> {
        buffer.sort_unstable();
        let mut writer = self.writer(key)?;
        for name in buffer.drain(..) {
            writer.write(&name)?;
        }
        writer.seal()
    }
    fn merge(
        &mut self,
        id: u64,
        pass: u64,
        inputs: std::ops::Range<u64>,
        next_pass: u64,
        output: u64,
        cancelled: &impl Fn() -> bool,
    ) -> io::Result<RunCursor> {
        let mut readers = inputs
            .clone()
            .map(|index| {
                let key = RunKey { id, pass, index };
                NameReader::open(
                    self.run_path(key),
                    key,
                    None,
                    self.secret,
                    self.health.clone(),
                )
            })
            .collect::<io::Result<Vec<_>>>()?;
        #[cfg(test)]
        {
            self.peak_readers = self.peak_readers.max(readers.len());
        }
        let mut heads = readers
            .iter_mut()
            .map(NameReader::next)
            .collect::<io::Result<Vec<_>>>()?;
        let mut writer = self.writer(RunKey {
            id,
            pass: next_pass,
            index: output,
        })?;
        loop {
            check_cancelled(cancelled)?;
            let smallest = heads
                .iter()
                .enumerate()
                .filter_map(|(i, h)| h.as_ref().map(|name| (i, name)))
                .min_by(|(_, a), (_, b)| a.cmp(b));
            let Some((index, name)) = smallest else { break };
            writer.write(name)?;
            heads[index] = readers[index].next()?;
        }
        let result = writer.seal()?;
        drop(readers);
        for index in inputs {
            std::fs::remove_file(self.run_path(RunKey { id, pass, index }))?;
        }
        Ok(result)
    }
    pub fn open(&self, run: RunCursor) -> io::Result<NameReader> {
        self.health.check()?;
        self.health.finish(NameReader::open(
            self.names_path(run.key.id),
            run.key,
            Some(run),
            self.secret,
            self.health.clone(),
        ))
    }
    pub fn remove(&self, run: RunCursor) -> io::Result<()> {
        self.health.check()?;
        self.health
            .finish(std::fs::remove_file(self.names_path(run.key.id)))
    }
    pub fn close(self) -> io::Result<()> {
        self.scratch.close()
    }
    pub fn scratch_path(&self) -> &Path {
        self.scratch.path()
    }
    fn run_path(&self, key: RunKey) -> PathBuf {
        self.scratch
            .path()
            .join(format!("{}-{}-{}", key.id, key.pass, key.index))
    }
    fn names_path(&self, id: u64) -> PathBuf {
        self.scratch.path().join(format!("{id}-names"))
    }
}

fn mac(
    secret: &[u8; 32],
    domain: &[u8],
    key: RunKey,
    ordinal: u64,
    offset: u64,
    bytes: &[u8],
) -> [u8; 32] {
    let mut hash = blake3::Hasher::new_keyed(secret);
    hash.update(domain);
    hash.update(&key.bytes());
    hash.update(&ordinal.to_le_bytes());
    hash.update(&offset.to_le_bytes());
    hash.update(&(bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
    *hash.finalize().as_bytes()
}

struct RunWriter {
    writer: BufWriter<File>,
    cursor: RunCursor,
    secret: [u8; 32],
}
impl RunWriter {
    fn new(path: PathBuf, key: RunKey, secret: [u8; 32]) -> io::Result<Self> {
        let mut writer = BufWriter::new(File::create_new(path)?);
        writer.write_all(&[0; HEADER_BYTES as usize])?;
        Ok(Self {
            writer,
            cursor: RunCursor {
                key,
                count: 0,
                end: HEADER_BYTES,
                offset: HEADER_BYTES,
                ordinal: 0,
            },
            secret,
        })
    }
    fn write(&mut self, name: &OsString) -> io::Result<()> {
        let bytes = native_path::encode(name);
        validate_name(&bytes)?;
        let tag = mac(
            &self.secret,
            b"sy/scan-name/v1",
            self.cursor.key,
            self.cursor.count,
            self.cursor.end,
            &bytes,
        );
        self.writer.write_all(&(bytes.len() as u32).to_le_bytes())?;
        self.writer.write_all(&bytes)?;
        self.writer.write_all(&tag)?;
        self.cursor.count = self.cursor.count.checked_add(1).ok_or_else(invalid)?;
        self.cursor.end = self
            .cursor
            .end
            .checked_add(36 + bytes.len() as u64)
            .ok_or_else(invalid)?;
        Ok(())
    }
    fn seal(mut self) -> io::Result<RunCursor> {
        let mut header = [0; HEADER_BYTES as usize];
        header[..24].copy_from_slice(&self.cursor.key.bytes());
        header[24..32].copy_from_slice(&self.cursor.count.to_le_bytes());
        header[32..40].copy_from_slice(&self.cursor.end.to_le_bytes());
        let tag = mac(
            &self.secret,
            b"sy/scan-run/v1",
            self.cursor.key,
            0,
            0,
            &header[..40],
        );
        header[40..].copy_from_slice(&tag);
        self.writer.seek(SeekFrom::Start(0))?;
        self.writer.write_all(&header)?;
        self.writer.flush()?;
        if self.writer.get_ref().metadata()?.len() != self.cursor.end {
            return Err(invalid());
        }
        Ok(self.cursor)
    }
}

pub(crate) struct NameReader {
    reader: BufReader<File>,
    cursor: RunCursor,
    secret: [u8; 32],
    health: Health,
}
impl NameReader {
    fn open(
        path: PathBuf,
        key: RunKey,
        expected: Option<RunCursor>,
        secret: [u8; 32],
        health: Health,
    ) -> io::Result<Self> {
        let mut reader = BufReader::new(File::open(path)?);
        let mut header = [0; HEADER_BYTES as usize];
        reader.read_exact(&mut header)?;
        if header[..24] != key.bytes()
            || header[40..] != mac(&secret, b"sy/scan-run/v1", key, 0, 0, &header[..40])
        {
            return Err(invalid());
        }
        let count = u64::from_le_bytes(header[24..32].try_into().map_err(|_| invalid())?);
        let end = u64::from_le_bytes(header[32..40].try_into().map_err(|_| invalid())?);
        let cursor = expected.unwrap_or(RunCursor {
            key,
            count,
            end,
            offset: HEADER_BYTES,
            ordinal: 0,
        });
        if cursor.count != count
            || cursor.end != end
            || end < HEADER_BYTES
            || reader.get_ref().metadata()?.len() != end
            || cursor.ordinal > count
            || cursor.offset < HEADER_BYTES
            || cursor.offset > end
            || (cursor.ordinal == count) != (cursor.offset == end)
            || (cursor.ordinal == 0 && cursor.offset != HEADER_BYTES)
        {
            return Err(invalid());
        }
        reader.seek(SeekFrom::Start(cursor.offset))?;
        Ok(Self {
            reader,
            cursor,
            secret,
            health,
        })
    }
    pub fn cursor(&self) -> RunCursor {
        self.cursor
    }
    pub fn next(&mut self) -> io::Result<Option<OsString>> {
        self.health.check()?;
        let result = self.next_inner();
        self.health.finish(result)
    }
    fn next_inner(&mut self) -> io::Result<Option<OsString>> {
        if self.reader.get_ref().metadata()?.len() != self.cursor.end {
            return Err(invalid());
        }
        if self.cursor.ordinal == self.cursor.count {
            if self.cursor.offset != self.cursor.end {
                return Err(invalid());
            }
            return Ok(None);
        }
        let mut length = [0; 4];
        self.reader.read_exact(&mut length)?;
        let len = u32::from_le_bytes(length) as usize;
        let next = self
            .cursor
            .offset
            .checked_add(36 + len as u64)
            .ok_or_else(invalid)?;
        if len == 0 || len > MAX_NAME_BYTES || next > self.cursor.end {
            return Err(invalid());
        }
        let mut bytes = vec![0; len];
        self.reader.read_exact(&mut bytes)?;
        let mut tag = [0; 32];
        self.reader.read_exact(&mut tag)?;
        if tag
            != mac(
                &self.secret,
                b"sy/scan-name/v1",
                self.cursor.key,
                self.cursor.ordinal,
                self.cursor.offset,
                &bytes,
            )
        {
            return Err(invalid());
        }
        let name = validate_name(&bytes)?;
        self.cursor.offset = next;
        self.cursor.ordinal += 1;
        if (self.cursor.ordinal == self.cursor.count) != (self.cursor.offset == self.cursor.end) {
            return Err(invalid());
        }
        Ok(Some(name))
    }
}

fn validate_name(bytes: &[u8]) -> io::Result<OsString> {
    if bytes.is_empty() || bytes.len() > MAX_NAME_BYTES || bytes.contains(&0) {
        return Err(invalid());
    }
    let name = native_path::decode(bytes)?;
    let mut components = name.components();
    if !matches!(components.next(), Some(std::path::Component::Normal(component)) if component == name.as_os_str())
        || components.next().is_some()
    {
        return Err(invalid());
    }
    Ok(name.into_os_string())
}
// Kernel randomness, never public scratch names/IDs. Functional Windows
// synchronization is outside the rooted engine's supported contract.
fn scan_secret() -> io::Result<[u8; 32]> {
    #[cfg(unix)]
    {
        let mut secret = [0; 32];
        File::open("/dev/urandom")?.read_exact(&mut secret)?;
        Ok(secret)
    }
    #[cfg(not(unix))]
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "authenticated scan randomness unsupported",
    ))
}

fn check_cancelled(cancelled: &impl Fn() -> bool) -> io::Result<()> {
    if cancelled() {
        Err(io::Error::new(io::ErrorKind::Interrupted, "scan cancelled"))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn scratch_guard_uses_held_root_after_path_replacement() {
        let parent = tempfile::TempDir::new().unwrap();
        let root = parent.path().join("root");
        std::fs::create_dir(&root).unwrap();
        let held = File::open(&root).unwrap();
        let moved = parent.path().join("moved");
        std::fs::rename(&root, &moved).unwrap();
        std::fs::create_dir(&root).unwrap();
        let scratch = moved.join("scratch");
        std::fs::create_dir(&scratch).unwrap();
        let before = std::fs::metadata(&scratch).unwrap().modified().unwrap();
        assert_eq!(
            validate_scratch_root(&held, &scratch).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            std::fs::metadata(&scratch).unwrap().modified().unwrap(),
            before
        );
        assert_eq!(std::fs::read_dir(&scratch).unwrap().count(), 0);
        // The replacement pathname is not the held scan root, and a temporary
        // ancestor is safe: scratch children are siblings, not source children.
        validate_scratch_root(&held, &root).unwrap();
        validate_scratch_root(&held, parent.path()).unwrap();
    }

    #[test]
    fn many_closed_runs_merge_with_tiny_budgets_in_native_order() {
        let budget = SortBudget {
            bytes: 90,
            records: 3,
            fan_in: 2,
        };
        let mut spool = NameSpool::new(budget).unwrap();
        let names = (0..511)
            .rev()
            .map(|i| Ok(OsString::from(format!("n{i:04}"))));
        let id = spool.sort(names, || false).unwrap();
        assert!(spool.peak_records <= 3);
        assert!(spool.peak_bytes <= 90);
        assert!(spool.peak_readers <= 2);
        assert_eq!(std::fs::read_dir(spool.scratch.path()).unwrap().count(), 1);
        let mut reader = spool.open(id).unwrap();
        for i in 0..511 {
            assert_eq!(
                reader.next().unwrap().unwrap(),
                OsString::from(format!("n{i:04}"))
            );
        }
        assert!(reader.next().unwrap().is_none());
        drop(reader);
        let path = spool.scratch.path().to_path_buf();
        drop(spool);
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn native_names_round_trip_and_sort_like_path_components() {
        use std::os::unix::ffi::OsStringExt;
        let mut expected = [
            b"a.ext".to_vec(),
            vec![0xff],
            b"a".to_vec(),
            vec![b'a', 0xfe],
        ]
        .map(|bytes| PathBuf::from(OsString::from_vec(bytes)));
        let mut spool = NameSpool::new(SortBudget {
            bytes: 1,
            records: 1,
            fan_in: 2,
        })
        .unwrap();
        let id = spool
            .sort(
                expected.iter().map(|path| Ok(path.as_os_str().to_owned())),
                || false,
            )
            .unwrap();
        expected.sort();
        let mut reader = spool.open(id).unwrap();
        for name in expected {
            assert_eq!(reader.next().unwrap().unwrap(), name.into_os_string());
        }
        assert!(reader.next().unwrap().is_none());
    }

    #[test]
    fn final_runs_reject_truncation_append_replacement_and_payload_changes() {
        for damage in 0..9 {
            let mut spool = NameSpool::new(SortBudget::default()).unwrap();
            let run = spool
                .sort([Ok("aa".into()), Ok("bb".into())], || false)
                .unwrap();
            let path = spool.names_path(run.key.id);
            let mut bytes = std::fs::read(&path).unwrap();
            match damage {
                0 => bytes.clear(),
                1 => bytes.truncate(HEADER_BYTES as usize - 1),
                2 => bytes.truncate(HEADER_BYTES as usize),
                3 => {
                    bytes.pop();
                }
                4 => bytes.push(0),
                5 => bytes[HEADER_BYTES as usize + 4] = b'z',
                6 => bytes[HEADER_BYTES as usize + 4..HEADER_BYTES as usize + 6]
                    .copy_from_slice(b".."),
                7 => bytes[HEADER_BYTES as usize + 4..HEADER_BYTES as usize + 6]
                    .copy_from_slice(b"/x"),
                8 => bytes[HEADER_BYTES as usize..HEADER_BYTES as usize + 4]
                    .copy_from_slice(&u32::MAX.to_le_bytes()),
                _ => unreachable!(),
            }
            // Replacement, not just a write through the original descriptor.
            std::fs::remove_file(&path).unwrap();
            std::fs::write(&path, bytes).unwrap();
            let result = spool.open(run).and_then(|mut reader| {
                while reader.next()?.is_some() {}
                Ok(())
            });
            assert!(result.is_err(), "damage {damage} authorized exhaustion");
            assert!(spool.open(run).is_err(), "failed owner recovered");
            let scratch = spool.scratch_path().to_owned();
            spool.close().unwrap();
            assert!(!scratch.exists());
        }
    }

    #[test]
    fn replay_is_bound_to_original_run_ordinal_and_resume_cursor() {
        let mut spool = NameSpool::new(SortBudget::default()).unwrap();
        let run = spool
            .sort([Ok("aa".into()), Ok("bb".into())], || false)
            .unwrap();
        let mut reader = spool.open(run).unwrap();
        assert_eq!(reader.next().unwrap().unwrap(), "aa");
        let resume = reader.cursor();
        drop(reader);
        let path = spool.names_path(run.key.id);
        let mut bytes = std::fs::read(&path).unwrap();
        let record = bytes[HEADER_BYTES as usize..HEADER_BYTES as usize + 38].to_owned();
        bytes[HEADER_BYTES as usize + 38..].copy_from_slice(&record);
        std::fs::write(&path, bytes).unwrap();
        let mut reader = spool.open(resume).unwrap();
        assert!(reader.next().is_err());

        let mut spool = NameSpool::new(SortBudget::default()).unwrap();
        let first = spool.sort([Ok("aa".into())], || false).unwrap();
        let second = spool.sort([Ok("bb".into())], || false).unwrap();
        std::fs::copy(
            spool.names_path(first.key.id),
            spool.names_path(second.key.id),
        )
        .unwrap();
        assert!(spool.open(second).is_err());

        let mut spool = NameSpool::new(SortBudget::default()).unwrap();
        let run = spool
            .sort([Ok("aa".into()), Ok("bb".into())], || false)
            .unwrap();
        let mut reader = spool.open(run).unwrap();
        reader.next().unwrap();
        let mut resume = reader.cursor();
        drop(reader);
        resume.offset = HEADER_BYTES; // ordinal remains one: replay must fail.
        assert!(spool
            .open(resume)
            .and_then(|mut reader| reader.next())
            .is_err());
    }

    #[test]
    fn intermediate_merge_inputs_are_authenticated_and_exactly_exhausted() {
        for truncate in [true, false] {
            let mut spool = NameSpool::new(SortBudget {
                bytes: 1,
                records: 1,
                fan_in: 2,
            })
            .unwrap();
            let scratch = spool.scratch_path().to_owned();
            let damaged = Cell::new(false);
            let result = spool.sort([Ok("aa".into()), Ok("bb".into()), Ok("cc".into())], || {
                let path = scratch.join("0-0-0");
                // The cancellation boundary is reached before merging. Mutate
                // a real sealed input without a production corruption hook.
                if path.exists() && !damaged.replace(true) {
                    let mut bytes = std::fs::read(&path).unwrap();
                    if truncate {
                        bytes.clear();
                    } else {
                        bytes[HEADER_BYTES as usize + 4] = b'z';
                    }
                    std::fs::write(path, bytes).unwrap();
                }
                false
            });
            assert!(damaged.get());
            assert!(result.is_err());
            assert!(spool.sort(std::iter::empty(), || false).is_err());
            spool.close().unwrap();
            assert!(!scratch.exists());
        }
    }

    #[test]
    fn truncating_an_open_run_is_terminal_even_with_buffered_names() {
        let mut spool = NameSpool::new(SortBudget::default()).unwrap();
        let run = spool
            .sort([Ok("aa".into()), Ok("bb".into())], || false)
            .unwrap();
        let mut reader = spool.open(run).unwrap();
        reader.next().unwrap();
        File::options()
            .write(true)
            .open(spool.names_path(run.key.id))
            .unwrap()
            .set_len(0)
            .unwrap();
        assert!(reader.next().is_err());
        assert!(spool.open(run).is_err());
        drop(reader);
        spool.close().unwrap();
    }

    #[test]
    fn scratch_write_failure_is_not_a_successful_empty_scan() {
        let mut spool = NameSpool::new(SortBudget::default()).unwrap();
        std::fs::remove_dir(spool.scratch.path()).unwrap();
        let error = spool
            .sort([Ok(OsString::from("name"))], || false)
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn cancellation_and_corrupt_records_fail_and_cleanup() {
        let mut spool = NameSpool::new(SortBudget::default()).unwrap();
        let path = spool.scratch.path().to_path_buf();
        assert_eq!(
            spool
                .sort([Ok(OsString::from("a"))], || true)
                .unwrap_err()
                .kind(),
            io::ErrorKind::Interrupted
        );
        drop(spool);
        assert!(!path.exists());
        for bytes in [b"".as_slice(), b"..", b"a/b", b"/", b"a\0"] {
            assert!(validate_name(bytes).is_err());
        }
        assert!(validate_name(&vec![b'a'; MAX_NAME_BYTES + 1]).is_err());
    }
}
