//! Original SOURCE bindings outlive scanner continuations. Names and link
//! literals live on disk; an authenticated radix shares ancestor records by key.
//! Only scanner capture inserts records. Readers can only validate them.
use super::*;
use crate::authenticated_index::AuthenticatedIndex;
use std::io::{self, Read, Seek, SeekFrom, Write};

const MAX_TARGET: usize = 64 * 1024;
const MAX_RECORD: usize = 32 + 4 + crate::protocol::MAX_WIRE_PATH_BYTES + 112 + 1 + 4 + MAX_TARGET;

#[derive(Clone, Copy, PartialEq, Eq)]
struct NativeObservation {
    device: u64,
    inode: u64,
    kind: u64,
    identity: [u8; 32],
}
impl NativeObservation {
    #[allow(clippy::unnecessary_cast)] // native libc widths differ.
    fn from_stat(stat: &libc::stat) -> Result<Self> {
        let kind = match stat.st_mode & libc::S_IFMT {
            libc::S_IFDIR => Some(EntryKind::Directory),
            libc::S_IFREG => Some(EntryKind::File),
            libc::S_IFLNK => Some(EntryKind::Symlink),
            _ => None,
        };
        let identity = if let Some(kind) = kind {
            *crate::endpoint::local_identity::stat_identity(stat, kind)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid source stat"))?
                .as_bytes()
        } else {
            // Omitted special names still have real bindings. Recording them
            // grants no transfer capability: semantic scanning rejects them
            // only if selection policy actually includes them.
            let mut hash = blake3::Hasher::new();
            hash.update(b"sy/source-observation/special/v1\0");
            for field in [
                stat.st_dev as u64,
                stat.st_ino as u64,
                stat.st_mode as u64,
                stat.st_nlink as u64,
                stat.st_uid as u64,
                stat.st_gid as u64,
                stat.st_rdev as u64,
            ] {
                hash.update(&field.to_le_bytes());
            }
            for field in [
                stat.st_size as i64,
                stat.st_mtime as i64,
                stat.st_mtime_nsec as i64,
                stat.st_ctime as i64,
                stat.st_ctime_nsec as i64,
            ] {
                hash.update(&field.to_le_bytes());
            }
            *hash.finalize().as_bytes()
        };
        Ok(Self {
            device: stat.st_dev as u64,
            inode: stat.st_ino as u64,
            kind: (stat.st_mode & libc::S_IFMT) as u64,
            identity,
        })
    }
    fn same_binding(self, other: Self) -> bool {
        (self.device, self.inode, self.kind) == (other.device, other.inode, other.kind)
    }
    fn matches(self, other: Self, strict: bool) -> bool {
        self.same_binding(other)
            && (!strict || self.kind == libc::S_IFDIR as u64 || self.identity == other.identity)
    }
    fn encode(self, bytes: &mut Vec<u8>) {
        for value in [self.device, self.inode, self.kind] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        bytes.extend_from_slice(&self.identity);
    }
    fn decode(bytes: &mut &[u8]) -> io::Result<Self> {
        Ok(Self {
            device: u64::from_le_bytes(take(bytes)?),
            inode: u64::from_le_bytes(take(bytes)?),
            kind: u64::from_le_bytes(take(bytes)?),
            identity: take(bytes)?,
        })
    }
}

struct Observation {
    parent: [u8; 32],
    name: OsString,
    named: NativeObservation,
    effective: NativeObservation,
    followed: bool,
    target: Option<PathBuf>,
}
impl Observation {
    fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&self.parent);
        let name = self.name.as_bytes();
        bytes.extend_from_slice(&(name.len() as u32).to_le_bytes());
        bytes.extend_from_slice(name);
        self.named.encode(&mut bytes);
        self.effective.encode(&mut bytes);
        bytes.push(u8::from(self.followed));
        match &self.target {
            None => bytes.extend_from_slice(&u32::MAX.to_le_bytes()),
            Some(target) => {
                let target = target.as_os_str().as_bytes();
                bytes.extend_from_slice(&(target.len() as u32).to_le_bytes());
                bytes.extend_from_slice(target);
            }
        }
        bytes
    }
    fn decode(mut bytes: &[u8]) -> io::Result<Self> {
        let parent = take(&mut bytes)?;
        let length = u32::from_le_bytes(take(&mut bytes)?) as usize;
        let name = OsStr::from_bytes(field(&mut bytes, length)?).to_os_string();
        let named = NativeObservation::decode(&mut bytes)?;
        let effective = NativeObservation::decode(&mut bytes)?;
        let followed = match take::<1>(&mut bytes)? {
            [0] => false,
            [1] => true,
            _ => return Err(invalid()),
        };
        let length = u32::from_le_bytes(take(&mut bytes)?);
        let target = if length == u32::MAX {
            None
        } else {
            Some(PathBuf::from(OsStr::from_bytes(field(
                &mut bytes,
                length as usize,
            )?)))
        };
        if !bytes.is_empty() {
            return Err(invalid());
        }
        Ok(Self {
            parent,
            name,
            named,
            effective,
            followed,
            target,
        })
    }
    fn matches(&self, current: &Self, strict: bool) -> bool {
        self.parent == current.parent
            && self.name == current.name
            && self.named.matches(current.named, strict)
            && self.effective.matches(current.effective, strict)
            && self.followed == current.followed
            && self.target == current.target
    }
}
fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "invalid source observation catalogue",
    )
}
fn field<'a>(bytes: &mut &'a [u8], length: usize) -> io::Result<&'a [u8]> {
    if length > bytes.len() {
        return Err(invalid());
    }
    let (value, rest) = bytes.split_at(length);
    *bytes = rest;
    Ok(value)
}
fn take<const N: usize>(bytes: &mut &[u8]) -> io::Result<[u8; N]> {
    field(bytes, N)?.try_into().map_err(|_| invalid())
}
fn key(parent: [u8; 32], name: &OsStr) -> [u8; 32] {
    let mut hash = blake3::Hasher::new();
    hash.update(b"sy/source-observation/name/v1\0");
    hash.update(&parent);
    hash.update(name.as_bytes());
    *hash.finalize().as_bytes()
}
fn path_key(path: &Path) -> [u8; 32] {
    path.components().fold([0; 32], |parent, component| {
        key(parent, component.as_os_str())
    })
}

#[derive(Default)]
pub(super) enum CatalogueState {
    #[default]
    Unstarted,
    Active(Catalogue),
    Terminal,
}

impl CatalogueState {
    fn active(&mut self) -> Result<&mut Catalogue> {
        match self {
            Self::Active(catalogue) => Ok(catalogue),
            Self::Unstarted | Self::Terminal => Err(RootedFsError::SourceCatalogueTerminal),
        }
    }
}

/// A failed/panicking scanner cannot leave apparently reusable authority.
/// Early close is cancellation, not exhaustion. Its recorded names cannot
/// establish a reusable completed attempt; retries need a new SOURCE owner.
pub(crate) struct SourceScanAttempt {
    rooted: RootedFs,
    successful: bool,
}
impl SourceScanAttempt {
    pub(crate) fn finish(mut self, successful: bool) {
        self.successful = successful;
    }
}
impl Drop for SourceScanAttempt {
    fn drop(&mut self) {
        if !self.successful {
            if let Ok(mut owner) = self.rooted.source_observations.lock() {
                *owner = CatalogueState::Terminal;
            }
        }
    }
}

pub(super) struct Catalogue {
    index: AuthenticatedIndex<48>,
    file: File,
    end: u64,
    healthy: bool,
}
impl Catalogue {
    fn new(root: RawFd) -> Result<Self> {
        let mut catalogue = Self {
            index: AuthenticatedIndex::new()?,
            file: tempfile::tempfile()?,
            end: 0,
            healthy: true,
        };
        let native = NativeObservation::from_stat(&stat_fd(root)?)?;
        catalogue.insert(
            [0; 32],
            &Observation {
                parent: [0; 32],
                name: OsString::new(),
                named: native,
                effective: native,
                followed: false,
                target: None,
            },
        )?;
        Ok(catalogue)
    }
    fn begin(&mut self) -> Result<()> {
        if !self.healthy {
            return Err(RootedFsError::SourceCatalogueTerminal);
        }
        self.healthy = false;
        if self.file.metadata()?.len() != self.end {
            return Err(invalid().into());
        }
        Ok(())
    }
    fn get(&mut self, key: [u8; 32]) -> Result<Option<Observation>> {
        let Some(locator) = self.index.get_bytes(key)? else {
            return Ok(None);
        };
        let offset = u64::from_le_bytes(locator[..8].try_into().map_err(|_| invalid())?);
        let length = u64::from_le_bytes(locator[8..16].try_into().map_err(|_| invalid())?);
        if length > MAX_RECORD as u64 || offset.checked_add(length).is_none_or(|end| end > self.end)
        {
            return Err(invalid().into());
        }
        let mut bytes = vec![0; length as usize];
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.read_exact(&mut bytes)?;
        if blake3::hash(&bytes).as_bytes() != &locator[16..] {
            return Err(invalid().into());
        }
        Ok(Some(Observation::decode(&bytes)?))
    }
    fn insert(&mut self, key: [u8; 32], observation: &Observation) -> Result<()> {
        let bytes = observation.encode();
        if bytes.len() > MAX_RECORD {
            return Err(invalid().into());
        }
        let mut locator = [0; 48];
        locator[..8].copy_from_slice(&self.end.to_le_bytes());
        locator[8..16].copy_from_slice(&(bytes.len() as u64).to_le_bytes());
        locator[16..].copy_from_slice(blake3::hash(&bytes).as_bytes());
        let end = self
            .end
            .checked_add(bytes.len() as u64)
            .ok_or_else(invalid)?;
        self.file.seek(SeekFrom::Start(self.end))?;
        self.file.write_all(&bytes)?;
        self.index.set_bytes(key, locator)?;
        self.end = end;
        Ok(())
    }
    fn require(&mut self, path: &Path) -> Result<Observation> {
        self.get(path_key(path))?
            .ok_or_else(|| RootedFsError::SourceUnobserved(path.to_path_buf()))
    }
}

fn target_at(parent: RawFd, name: &OsStr) -> Result<PathBuf> {
    let name = component_cstring(name)?;
    let mut bytes = vec![0; MAX_TARGET + 1];
    // SAFETY: parent is live, name is a component and bytes is writable for its stated length.
    let count = unsafe {
        libc::readlinkat(
            parent,
            name.as_ptr(),
            bytes.as_mut_ptr().cast(),
            bytes.len(),
        )
    };
    if count < 0 {
        return Err(io::Error::last_os_error().into());
    }
    let count = usize::try_from(count).map_err(|_| invalid())?;
    if count > MAX_TARGET {
        return Err(invalid().into());
    }
    bytes.truncate(count);
    Ok(PathBuf::from(OsStr::from_bytes(&bytes)))
}
fn inspect(parent: RawFd, path: &Path, followed: bool) -> Result<Observation> {
    let name = path.file_name().ok_or(RootedFsError::InvalidRelativePath)?;
    let named = NativeObservation::from_stat(&stat_at_no_follow(parent, name)?)?;
    let target = if named.kind == libc::S_IFLNK as u64 {
        Some(target_at(parent, name)?)
    } else {
        None
    };
    let effective = if followed {
        NativeObservation::from_stat(&stat_at(parent, name, 0)?)?
    } else {
        named
    };
    let after = NativeObservation::from_stat(&stat_at_no_follow(parent, name)?)?;
    if !named.matches(after, true) {
        return Err(RootedFsError::SourceBindingChanged(path.to_path_buf()));
    }
    if let Some(original) = &target {
        if &target_at(parent, name)? != original {
            return Err(RootedFsError::SourceBindingChanged(path.to_path_buf()));
        }
    }
    Ok(Observation {
        parent: path_key(path.parent().unwrap_or(Path::new(""))),
        name: name.to_os_string(),
        named,
        effective,
        followed,
        target,
    })
}

impl RootedFs {
    pub(super) fn is_source_authority(&self) -> bool {
        self.source_path_continuity
            || matches!(self.mutation_admission, RootedMutationAdmission::ReadOnly)
            || self
                .source_observations
                .lock()
                .map_or(true, |owner| !matches!(*owner, CatalogueState::Unstarted))
    }
    pub(crate) fn start_source_scan_blocking(&self) -> Result<SourceScanAttempt> {
        let attempt = SourceScanAttempt {
            rooted: self.clone(),
            successful: false,
        };
        self.begin_source_observations_blocking()?;
        Ok(attempt)
    }
    pub(crate) fn begin_source_observations_blocking(&self) -> Result<()> {
        self.verify_source_root_path_blocking()?;
        let mut owner = self
            .source_observations
            .lock()
            .map_err(|_| RootedFsError::SourceCatalogueTerminal)?;
        if matches!(*owner, CatalogueState::Unstarted) {
            // Failure during initial scratch acquisition is terminal too.
            *owner = CatalogueState::Terminal;
            let root = File::from(self.root_fd.try_clone()?);
            crate::engine::scan_sort::validate_scratch_root(&root, &tempfile::env::temp_dir())?;
            *owner = CatalogueState::Active(Catalogue::new(self.root_fd.as_raw_fd())?);
        }
        let catalogue = owner.active()?;
        catalogue.begin()?;
        let root = catalogue.require(Path::new(""))?;
        if !root
            .effective
            .same_binding(NativeObservation::from_stat(&stat_fd(
                self.root_fd.as_raw_fd(),
            )?)?)
        {
            return Err(RootedFsError::RootChanged(self.root_path.as_ref().clone()));
        }
        catalogue.healthy = true;
        Ok(())
    }
    /// Capture BEFORE omission, using the scanner's actual active parent. A
    /// second scan validates an existing record; it cannot overwrite originals.
    pub(crate) fn record_source_name_blocking(
        &self,
        parent: RawFd,
        path: &Path,
        followed: bool,
    ) -> Result<()> {
        self.verify_source_root_path_blocking()?;
        if path.as_os_str().as_bytes().len() > crate::protocol::MAX_WIRE_PATH_BYTES
            || path.components().count() > crate::protocol::MAX_WIRE_COMPONENTS
        {
            return Err(invalid().into());
        }
        let mut owner = self
            .source_observations
            .lock()
            .map_err(|_| RootedFsError::SourceCatalogueTerminal)?;
        let catalogue = owner.active()?;
        catalogue.begin()?;
        let ancestor = catalogue.require(path.parent().unwrap_or(Path::new("")))?;
        if !ancestor
            .effective
            .same_binding(NativeObservation::from_stat(&stat_fd(parent)?)?)
        {
            return Err(RootedFsError::SourceBindingChanged(path.to_path_buf()));
        }
        let observed = inspect(parent, path, followed)?;
        if let Some(original) = catalogue.get(path_key(path))? {
            if !original.matches(&observed, true) {
                return Err(RootedFsError::SourceBindingChanged(path.to_path_buf()));
            }
        } else {
            catalogue.insert(path_key(path), &observed)?;
        }
        catalogue.healthy = true;
        Ok(())
    }
    pub(crate) fn validate_source_reader_blocking(
        &self,
        path: &RelativePath,
        file: &File,
    ) -> Result<()> {
        self.validate_source_observation_blocking(path)?;
        let mut owner = self
            .source_observations
            .lock()
            .map_err(|_| RootedFsError::SourceCatalogueTerminal)?;
        if matches!(*owner, CatalogueState::Unstarted) {
            return Ok(());
        }
        let catalogue = owner.active()?;
        catalogue.begin()?;
        let original = catalogue.require(path.as_path())?;
        if !original.effective.matches(
            NativeObservation::from_stat(&stat_fd(file.as_raw_fd())?)?,
            true,
        ) {
            return Err(RootedFsError::SourceBindingChanged(
                path.as_path().to_path_buf(),
            ));
        }
        catalogue.healthy = true;
        drop(owner);
        self.validate_source_observation_blocking(path)
    }
    /// Validate every original ancestor and parent/name, with at most two
    /// active directory descriptors. SOURCE never consults retirement lineage.
    pub(crate) fn validate_source_observation_blocking(&self, path: &RelativePath) -> Result<()> {
        self.validate_source_binding_blocking(path.as_path(), true)
    }
    pub(super) fn validate_source_binding_blocking(&self, path: &Path, strict: bool) -> Result<()> {
        self.verify_source_root_path_blocking()?;
        let source = self.is_source_authority();
        let mut owner = self
            .source_observations
            .lock()
            .map_err(|_| RootedFsError::SourceCatalogueTerminal)?;
        if matches!(*owner, CatalogueState::Unstarted) {
            return if source {
                Err(RootedFsError::SourceUnobserved(path.to_path_buf()))
            } else {
                Ok(())
            };
        }
        let catalogue = owner.active()?;
        catalogue.begin()?;
        let mut directory = self.root_fd.try_clone()?;
        let mut relative = PathBuf::new();
        let mut components = path.components().peekable();
        while let Some(component) = components.next() {
            let Component::Normal(name) = component else {
                return Err(RootedFsError::InvalidRelativePath);
            };
            let ancestor = catalogue.require(&relative)?;
            if !ancestor
                .effective
                .same_binding(NativeObservation::from_stat(&stat_fd(
                    directory.as_raw_fd(),
                )?)?)
            {
                return Err(RootedFsError::SourceBindingChanged(path.to_path_buf()));
            }
            relative.push(name);
            let original = catalogue.require(&relative)?;
            let observed = inspect(directory.as_raw_fd(), &relative, original.followed)?;
            // Own unlink continuation may change a file's ctime/nlink, never
            // its original binding or any link literal/ancestor observation.
            let exact = strict || original.named.kind == libc::S_IFLNK as u64;
            if !original.matches(&observed, exact) {
                return Err(RootedFsError::SourceBindingChanged(path.to_path_buf()));
            }
            if components.peek().is_some() {
                if original.followed && original.named.kind == libc::S_IFLNK as u64 {
                    let name = component_cstring(name)?;
                    // SAFETY: directory is live and name is one component. This
                    // is existing explicit LOCAL follow behavior, not remote follow.
                    let fd = unsafe {
                        libc::openat(
                            directory.as_raw_fd(),
                            name.as_ptr(),
                            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
                        )
                    };
                    if fd < 0 {
                        return Err(io::Error::last_os_error().into());
                    }
                    // SAFETY: successful openat returned a fresh descriptor.
                    directory = unsafe { OwnedFd::from_raw_fd(fd) };
                } else {
                    directory = open_dir_at(directory.as_raw_fd(), name)?;
                }
                if !original
                    .effective
                    .same_binding(NativeObservation::from_stat(&stat_fd(
                        directory.as_raw_fd(),
                    )?)?)
                {
                    return Err(RootedFsError::SourceBindingChanged(path.to_path_buf()));
                }
            }
        }
        catalogue.healthy = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
