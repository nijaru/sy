//! Bounded native-name sorting for ordered directory scans.
//!
//! Runs have deterministic on-disk names: neither the run catalogue nor inactive
//! runs occupy RAM/descriptors. Only one directory is sorted at a time; its
//! finished run can be closed while traversal descends into a child.
use super::native_path;
use std::ffi::OsString;
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use tempfile::TempDir;

const MAX_NAME_BYTES: usize = crate::protocol::MAX_WIRE_COMPONENT_BYTES;

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
        Ok(Self {
            scratch: tempfile::Builder::new()
                .prefix("sy-scan-")
                .tempdir_in(parent)?,
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

    pub fn sort(
        &mut self,
        names: impl IntoIterator<Item = io::Result<OsString>>,
        cancelled: impl Fn() -> bool,
    ) -> io::Result<u64> {
        let id = self.next_id;
        self.next_id = id
            .checked_add(1)
            .ok_or_else(|| io::Error::other("scan run ID overflow"))?;
        let mut buffer = Vec::new();
        let mut bytes = 0;
        let mut count = 0_u64;
        for name in names {
            check_cancelled(&cancelled)?;
            let name = name?;
            let len = native_path::encode(&name).len();
            if len > MAX_NAME_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "scan name exceeds native/wire component limit",
                ));
            }
            let charge = len + std::mem::size_of::<OsString>();
            if !buffer.is_empty()
                && (buffer.len() >= self.budget.records || bytes + charge > self.budget.bytes)
            {
                self.flush_run(id, count, &mut buffer)?;
                count = count
                    .checked_add(1)
                    .ok_or_else(|| io::Error::other("scan run count overflow"))?;
                bytes = 0;
            }
            // One valid name may exceed a tiny test/configured byte budget.
            // It remains bounded by MAX_NAME_BYTES, not an arbitrary refusal.
            bytes += charge;
            buffer.push(name);
            #[cfg(test)]
            {
                self.peak_records = self.peak_records.max(buffer.len());
                self.peak_bytes = self.peak_bytes.max(bytes);
            }
        }
        if !buffer.is_empty() || count == 0 {
            self.flush_run(id, count, &mut buffer)?;
            count = count
                .checked_add(1)
                .ok_or_else(|| io::Error::other("scan run count overflow"))?;
        }
        let mut pass = 0_u64;
        while count > 1 {
            check_cancelled(&cancelled)?;
            let next_pass = pass
                .checked_add(1)
                .ok_or_else(|| io::Error::other("scan merge pass overflow"))?;
            let mut start = 0;
            let mut output = 0;
            while start < count {
                let end = count.min(start.saturating_add(self.budget.fan_in as u64));
                self.merge(id, pass, start..end, next_pass, output, &cancelled)?;
                start = end;
                output += 1;
            }
            count = output;
            pass = next_pass;
        }
        std::fs::rename(self.run_path(id, pass, 0), self.names_path(id))?;
        Ok(id)
    }

    fn flush_run(&self, id: u64, index: u64, buffer: &mut Vec<OsString>) -> io::Result<()> {
        buffer.sort_unstable();
        let mut writer = BufWriter::new(File::create(self.run_path(id, 0, index))?);
        for name in buffer.drain(..) {
            write_name(&mut writer, &name)?;
        }
        writer.flush()
    }

    fn merge(
        &mut self,
        id: u64,
        pass: u64,
        inputs: std::ops::Range<u64>,
        next_pass: u64,
        output: u64,
        cancelled: &impl Fn() -> bool,
    ) -> io::Result<()> {
        let mut readers = inputs
            .clone()
            .map(|index| File::open(self.run_path(id, pass, index)).map(BufReader::new))
            .collect::<io::Result<Vec<_>>>()?;
        #[cfg(test)]
        {
            self.peak_readers = self.peak_readers.max(readers.len());
        }
        let mut heads = readers
            .iter_mut()
            .map(read_name)
            .collect::<io::Result<Vec<_>>>()?;
        let mut writer = BufWriter::new(File::create(self.run_path(id, next_pass, output))?);
        loop {
            check_cancelled(cancelled)?;
            let smallest = heads
                .iter()
                .enumerate()
                .filter_map(|(index, head)| head.as_ref().map(|name| (index, name)))
                .min_by(|(_, left), (_, right)| left.cmp(right));
            let Some((index, name)) = smallest else { break };
            write_name(&mut writer, name)?;
            heads[index] = read_name(&mut readers[index])?;
        }
        writer.flush()?;
        drop(readers);
        for index in inputs {
            std::fs::remove_file(self.run_path(id, pass, index))?;
        }
        Ok(())
    }

    pub fn open(&self, id: u64, offset: u64) -> io::Result<File> {
        let mut file = File::open(self.names_path(id))?;
        file.seek(SeekFrom::Start(offset))?;
        Ok(file)
    }

    pub fn remove(&self, id: u64) -> io::Result<()> {
        std::fs::remove_file(self.names_path(id))
    }

    pub fn close(self) -> io::Result<()> {
        self.scratch.close()
    }

    pub fn scratch_path(&self) -> &std::path::Path {
        self.scratch.path()
    }

    fn run_path(&self, id: u64, pass: u64, index: u64) -> PathBuf {
        self.scratch.path().join(format!("{id}-{pass}-{index}"))
    }

    fn names_path(&self, id: u64) -> PathBuf {
        self.scratch.path().join(format!("{id}-names"))
    }
}

fn check_cancelled(cancelled: &impl Fn() -> bool) -> io::Result<()> {
    if cancelled() {
        Err(io::Error::new(io::ErrorKind::Interrupted, "scan cancelled"))
    } else {
        Ok(())
    }
}

fn write_name(writer: &mut impl Write, name: &OsString) -> io::Result<()> {
    let bytes = native_path::encode(name);
    let len =
        u32::try_from(bytes.len()).map_err(|_| io::Error::other("scan name length overflow"))?;
    writer.write_all(&len.to_le_bytes())?;
    writer.write_all(&bytes)
}

pub(crate) fn read_name(reader: &mut impl Read) -> io::Result<Option<OsString>> {
    let mut len = [0; 4];
    if reader.read(&mut len[..1])? == 0 {
        return Ok(None);
    }
    reader.read_exact(&mut len[1..])?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_NAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "oversized scan name record",
        ));
    }
    let mut bytes = vec![0; len];
    reader.read_exact(&mut bytes)?;
    let name = native_path::decode(&bytes)?.into_os_string();
    let mut components = std::path::Path::new(&name).components();
    if !matches!(components.next(), Some(std::path::Component::Normal(component)) if component == name)
        || components.next().is_some()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "scan record is not one native name",
        ));
    }
    Ok(Some(name))
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
        let mut reader = spool.open(id, 0).unwrap();
        for i in 0..511 {
            assert_eq!(
                read_name(&mut reader).unwrap().unwrap(),
                OsString::from(format!("n{i:04}"))
            );
        }
        assert!(read_name(&mut reader).unwrap().is_none());
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
        let mut reader = spool.open(id, 0).unwrap();
        for name in expected {
            assert_eq!(
                read_name(&mut reader).unwrap().unwrap(),
                name.into_os_string()
            );
        }
        assert!(read_name(&mut reader).unwrap().is_none());
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
        assert!(read_name(&mut &b"\x01\x00"[..]).is_err());
        assert!(read_name(&mut &b"\x02\x00\x00\x00.."[..]).is_err());
        assert!(read_name(&mut &b"\x03\x00\x00\x00a/b"[..]).is_err());
        assert!(read_name(&mut &(MAX_NAME_BYTES as u32 + 1).to_le_bytes()[..]).is_err());
    }
}
