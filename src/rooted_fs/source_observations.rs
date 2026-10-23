//! Original SOURCE bindings outlive scanner continuations. Names and link
//! literals live on disk; an authenticated radix shares ancestor records by key.
//! Only scanner capture inserts records. Readers can only validate them.
use super::*;
use crate::authenticated_index::AuthenticatedIndex;
use std::io::{self, Read, Seek, SeekFrom, Write};

const MAX_TARGET: usize = 64 * 1024;
const MAX_FOLLOW_LINKS: usize = 40;
const MAX_FOLLOW_STEPS: usize = 4096;
// One bounded transcript per observation, never one resident entry per tree.
const MAX_RECORD: usize = 32
    + 4
    + crate::protocol::MAX_WIRE_PATH_BYTES
    + 128
    + 32
    + 1
    + 32
    + 4
    + MAX_TARGET
    + 4
    + MAX_FOLLOW_STEPS * (1 + 4 + 64 + 32 + 4)
    + MAX_TARGET
    + 4;
use super::local_source_effects::native_object::{object_key, NativeObjectKey};

mod following;
use following::{inspect_file, same_resolution, ResolutionRoot, ResolutionStep};
pub(super) mod inventory;
pub(super) mod operator;
use inventory::SourceRootProtection;

#[derive(Clone, Copy, PartialEq, Eq)]
struct NativeObservation {
    device: u64,
    inode: u64,
    kind: u64,
    nlink: u64,
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
            nlink: stat.st_nlink as u64,
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
        for value in [self.device, self.inode, self.kind, self.nlink] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        bytes.extend_from_slice(&self.identity);
    }
    fn decode(bytes: &mut &[u8]) -> io::Result<Self> {
        Ok(Self {
            device: u64::from_le_bytes(take(bytes)?),
            inode: u64::from_le_bytes(take(bytes)?),
            kind: u64::from_le_bytes(take(bytes)?),
            nlink: u64::from_le_bytes(take(bytes)?),
            identity: take(bytes)?,
        })
    }
}

/// A link literal is not a normalized filesystem path. In particular, PathBuf
/// equality elides `.` and repeated separators, which cannot authenticate it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct LinkTarget(OsString);
impl LinkTarget {
    fn new(target: &OsStr) -> Self {
        Self(target.to_os_string())
    }
}
impl std::ops::Deref for LinkTarget {
    type Target = Path;
    fn deref(&self) -> &Path {
        Path::new(&self.0)
    }
}

/// Ephemeral read-only cursor reached through authenticated originals.
/// This is not a root, effect lease, or selected-name removal authority.
struct TerminalBinding {
    parent: OwnedFd,
    name: OsString,
    stat: libc::stat,
    object_key: Option<NativeObjectKey>,
}

struct Observation {
    parent: [u8; 32],
    name: OsString,
    named: NativeObservation,
    effective: NativeObservation,
    followed: bool,
    object_key: Option<NativeObjectKey>,
    // A qualified followed FILE has a distinct protected DATA object.
    followed_object_key: Option<NativeObjectKey>,
    target: Option<LinkTarget>,
    // Followed DIR/LINK/FILE observations are provenance, not inventory members.
    resolution_start: usize,
    resolution: Vec<ResolutionStep>,
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
        bytes.extend_from_slice(&self.object_key.map_or([0; 32], |key| key.0));
        bytes.push(u8::from(self.followed));
        if self.followed {
            bytes.extend_from_slice(&self.followed_object_key.map_or([0; 32], |key| key.0));
        }
        match &self.target {
            None => bytes.extend_from_slice(&u32::MAX.to_le_bytes()),
            Some(target) => {
                let target = target.as_os_str().as_bytes();
                bytes.extend_from_slice(&(target.len() as u32).to_le_bytes());
                bytes.extend_from_slice(target);
            }
        }
        bytes.extend_from_slice(&(self.resolution_start as u32).to_le_bytes());
        bytes.extend_from_slice(&(self.resolution.len() as u32).to_le_bytes());
        for step in &self.resolution {
            step.encode(&mut bytes);
        }
        bytes
    }
    fn decode(mut bytes: &[u8]) -> io::Result<Self> {
        let parent = take(&mut bytes)?;
        let length = u32::from_le_bytes(take(&mut bytes)?) as usize;
        if length > crate::protocol::MAX_WIRE_PATH_BYTES {
            return Err(invalid());
        }
        let name = OsStr::from_bytes(field(&mut bytes, length)?).to_os_string();
        let named = NativeObservation::decode(&mut bytes)?;
        let effective = NativeObservation::decode(&mut bytes)?;
        let object = take::<32>(&mut bytes)?;
        let object_key = (object != [0; 32]).then_some(NativeObjectKey(object));
        let followed = match take::<1>(&mut bytes)? {
            [0] => false,
            [1] => true,
            _ => return Err(invalid()),
        };
        let followed_object_key = if followed {
            let object = take::<32>(&mut bytes)?;
            (object != [0; 32]).then_some(NativeObjectKey(object))
        } else {
            None
        };
        let length = u32::from_le_bytes(take(&mut bytes)?);
        let target = if length == u32::MAX {
            None
        } else {
            if length as usize > MAX_TARGET {
                return Err(invalid());
            }
            Some(LinkTarget::new(OsStr::from_bytes(field(
                &mut bytes,
                length as usize,
            )?)))
        };
        let resolution_start = u32::from_le_bytes(take(&mut bytes)?) as usize;
        let count = u32::from_le_bytes(take(&mut bytes)?) as usize;
        if count > MAX_FOLLOW_STEPS || resolution_start > count {
            return Err(invalid());
        }
        let mut resolution = Vec::with_capacity(count);
        for _ in 0..count {
            resolution.push(ResolutionStep::decode(&mut bytes)?);
        }
        if !bytes.is_empty() {
            return Err(invalid());
        }
        let observation = Self {
            parent,
            name,
            named,
            effective,
            followed,
            object_key,
            followed_object_key,
            target,
            resolution_start,
            resolution,
        };
        observation.validate_resolution()?;
        Ok(observation)
    }
    fn matches(&self, current: &Self, strict: bool) -> bool {
        self.parent == current.parent
            && self.name == current.name
            && self.named.matches(current.named, strict)
            && self.effective.matches(current.effective, strict)
            && self.object_key == current.object_key
            && self.followed_object_key == current.followed_object_key
            && self.followed == current.followed
            && self.target == current.target
            && self.resolution_start == current.resolution_start
            && same_resolution(&self.resolution, &current.resolution)
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
    Active(Box<Catalogue>),
    Terminal,
}

impl CatalogueState {
    fn active(&mut self) -> Result<&mut Catalogue> {
        match self {
            Self::Active(catalogue) => {
                catalogue.check_run()?;
                Ok(catalogue)
            }
            Self::Unstarted | Self::Terminal => Err(RootedFsError::SourceCatalogueTerminal),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RunState {
    Usable,
    SourceRace,
    Terminal,
}

/// A failed/panicking scanner cannot leave apparently reusable authority.
/// Early close is cancellation, not exhaustion. Its recorded names cannot
/// establish a reusable completed attempt; retries need a new SOURCE owner.
pub(crate) struct SourceScanAttempt {
    rooted: RootedFs,
    successful: bool,
}
impl SourceScanAttempt {
    pub(crate) fn finish(mut self, successful: bool) -> Result<()> {
        if !successful {
            // Preserve the producer's original error (or ordinary close result).
            // Drop still makes cancelled/failed authority terminal.
            return Ok(());
        }
        let mut owner = self
            .rooted
            .source_observations
            .lock()
            .map_err(|_| RootedFsError::SourceCatalogueTerminal)?;
        let catalogue = owner.active()?;
        catalogue.begin()?;
        let expected = match catalogue.selected {
            Some(key) => u64::from(catalogue.index.get_bytes(key)?.is_some()),
            None => catalogue.count - 1,
        };
        if !catalogue.scanning || catalogue.seen != expected {
            return Err(invalid().into());
        }
        if catalogue.selected.is_none() {
            catalogue.check_completed_traversal()?;
        }
        catalogue.scanning = false;
        catalogue.completed = true;
        catalogue.healthy = true;
        self.successful = true;
        Ok(())
    }
}
impl Drop for SourceScanAttempt {
    fn drop(&mut self) {
        if !self.successful {
            if let Ok(mut owner) = self.rooted.source_observations.lock() {
                if let CatalogueState::Active(catalogue) = &mut *owner {
                    if catalogue.run != RunState::SourceRace {
                        catalogue.run = RunState::Terminal;
                    }
                    catalogue.scanning = false;
                }
            }
        }
    }
}

pub(super) struct Catalogue {
    index: AuthenticatedIndex<48>,
    file: File,
    end: u64,
    healthy: bool,
    run: RunState,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
    // Shared by successors and earlier clones; original binding failure is sticky.
    lifetime: Arc<std::sync::Mutex<Option<PathBuf>>>,
    count: u64,
    seen: u64,
    epoch: u64,
    scanning: bool,
    completed: bool,
    sealed: bool,
    scope: Option<SourceRootProtection>,
    selected: Option<[u8; 32]>,
    operator: Option<operator::OriginalAddress>,
}
impl Catalogue {
    fn empty() -> Result<Self> {
        Ok(Self {
            index: AuthenticatedIndex::new()?,
            file: tempfile::tempfile()?,
            end: 0,
            healthy: true,
            run: RunState::Usable,
            cancelled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            lifetime: Arc::new(std::sync::Mutex::new(None)),
            count: 0,
            seen: 0,
            epoch: 0,
            scanning: false,
            completed: false,
            sealed: false,
            scope: None,
            selected: None,
            operator: None,
        })
    }
    fn new(root: RawFd) -> Result<Self> {
        let mut catalogue = Self::empty()?;
        let native = NativeObservation::from_stat(&stat_fd(root)?)?;
        catalogue.insert(
            [0; 32],
            &Observation {
                parent: [0; 32],
                name: OsString::new(),
                named: native,
                effective: native,
                followed: false,
                object_key: object_key(root, None)?,
                followed_object_key: None,
                target: None,
                resolution_start: 0,
                resolution: Vec::new(),
            },
        )?;
        Ok(catalogue)
    }
    fn check_lifetime(&self) -> Result<()> {
        match &*self
            .lifetime
            .lock()
            .map_err(|_| RootedFsError::SourceCatalogueTerminal)?
        {
            Some(path) => Err(RootedFsError::RootChanged(path.clone())),
            None => Ok(()),
        }
    }
    fn changed(&mut self, path: PathBuf) -> RootedFsError {
        // A known native mismatch ends this transaction. Old readers/proofs
        // remain terminal and storage is never repaired.
        self.run = RunState::SourceRace;
        RootedFsError::SourceBindingChanged(path)
    }
    fn native_result<T>(&mut self, result: Result<T>) -> Result<T> {
        if let Err(RootedFsError::SourceBindingChanged(path)) = &result {
            self.run = RunState::SourceRace;
            return Err(RootedFsError::SourceBindingChanged(path.clone()));
        }
        result
    }
    fn check_run(&self) -> Result<()> {
        self.check_lifetime()?;
        if self.cancelled.load(Ordering::Acquire) {
            return Err(RootedFsError::SourceCatalogueTerminal);
        }
        match self.run {
            RunState::Usable => Ok(()),
            RunState::SourceRace => Err(RootedFsError::SourceRaceCutoff),
            RunState::Terminal => Err(RootedFsError::SourceCatalogueTerminal),
        }
    }
    fn begin(&mut self) -> Result<()> {
        self.check_run()?;
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
        self.read_locator(locator).map(Some)
    }
    fn read_locator(&mut self, locator: [u8; 48]) -> Result<Observation> {
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
        Ok(Observation::decode(&bytes)?)
    }
    fn append(&mut self, key: [u8; 32], observation: &Observation) -> Result<[u8; 48]> {
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
        if self.index.get_bytes(key)?.is_some() {
            return Err(invalid().into());
        }
        self.index.set_bytes(key, locator)?;
        self.end = end;
        Ok(locator)
    }
    fn insert(&mut self, key: [u8; 32], observation: &Observation) -> Result<()> {
        let locator = self.append(key, observation)?;
        self.index
            .set_bytes(inventory::ordinal_key(self.count), locator)?;
        let mut ordinal = [0; 48];
        ordinal[..8].copy_from_slice(&self.count.to_le_bytes());
        self.index
            .set_bytes(inventory::state_key(b"ordinal", key), ordinal)?;
        // Derived solely at original capture, before selection/omission. Late
        // enumeration never adopts a count or name into this authority.
        if self.count != 0 {
            let count = self
                .child_count(observation.parent)?
                .checked_add(1)
                .ok_or_else(invalid)?;
            let mut state = [0; 48];
            state[..8].copy_from_slice(&count.to_le_bytes());
            self.index
                .set_bytes(inventory::state_key(b"children", observation.parent), state)?;
        }
        self.count = self.count.checked_add(1).ok_or_else(invalid)?;
        Ok(())
    }
    fn require(&mut self, path: &Path) -> Result<Observation> {
        self.get(path_key(path))?
            .ok_or_else(|| RootedFsError::SourceUnobserved(path.to_path_buf()))
    }
}

fn target_at(parent: RawFd, name: &OsStr) -> Result<LinkTarget> {
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
    Ok(LinkTarget::new(OsStr::from_bytes(&bytes)))
}
/// Only for a name just observed as a LINK. EINVAL from readlinkat then
/// means native retyping, not an arbitrary I/O failure eligible for retry.
fn observed_target_at(parent: RawFd, name: &OsStr, path: &Path) -> Result<LinkTarget> {
    target_at(parent, name).map_err(|error| match error {
        RootedFsError::Io(error) if error.raw_os_error() == Some(libc::EINVAL) => {
            RootedFsError::SourceBindingChanged(path.to_path_buf())
        }
        error => source_read_race(error, path),
    })
}

fn inspect(
    root: ResolutionRoot<'_>,
    parent: RawFd,
    path: &Path,
    follow: bool,
) -> Result<Observation> {
    inspect_with_stat(root, parent, path, follow).map(|(observation, _, _)| observation)
}

fn inspect_with_stat(
    root: ResolutionRoot<'_>,
    parent: RawFd,
    path: &Path,
    follow: bool,
) -> Result<(Observation, libc::stat, Option<TerminalBinding>)> {
    let name = path.file_name().ok_or(RootedFsError::InvalidRelativePath)?;
    let stat = stat_at_no_follow(parent, name).map_err(|error| match error {
        RootedFsError::Io(io) if io.raw_os_error() == Some(libc::ENOENT) => {
            RootedFsError::SourceBindingChanged(path.to_path_buf())
        }
        other => other,
    })?;
    let named = NativeObservation::from_stat(&stat)?;
    // The observation records actual link following, not the caller's policy.
    // Ordinary names retain the same binding under either scan policy.
    let followed = follow && named.kind == libc::S_IFLNK as u64;
    let target = if named.kind == libc::S_IFLNK as u64 {
        Some(observed_target_at(parent, name, path)?)
    } else {
        None
    };
    let (effective, followed_object_key, stat, terminal, resolution_start, resolution) = if followed
    {
        let target = target.as_deref().ok_or_else(invalid)?;
        let file = inspect_file(root, parent, path, named, target)?;
        (
            NativeObservation::from_stat(&file.binding.stat)?,
            file.binding.object_key,
            file.binding.stat,
            Some(file.binding),
            file.resolution_start,
            file.resolution,
        )
    } else {
        (named, None, stat, None, 0, Vec::new())
    };
    let object_key = finish_named_observation(parent, path, named)?;
    if let Some(original) = &target {
        if &observed_target_at(parent, name, path)? != original {
            return Err(RootedFsError::SourceBindingChanged(path.to_path_buf()));
        }
    }
    Ok((
        Observation {
            parent: path_key(path.parent().unwrap_or(Path::new(""))),
            name: name.to_os_string(),
            named,
            effective,
            followed,
            object_key,
            followed_object_key,
            target,
            resolution_start,
            resolution,
        },
        stat,
        terminal,
    ))
}

/// Close the named observation after any link/target work. Either native query
/// can detect the named change; the original selected-LINK owner must promote
/// that result to a sticky operator failure instead of a CHILD race.
fn finish_named_observation(
    parent: RawFd,
    path: &Path,
    named: NativeObservation,
) -> Result<Option<NativeObjectKey>> {
    let name = path.file_name().ok_or(RootedFsError::InvalidRelativePath)?;
    let object_key =
        object_key(parent, Some(name)).map_err(|error| source_read_race(error, path))?;
    let after = stat_at_no_follow(parent, name).map_err(|error| source_read_race(error, path))?;
    if !named.matches(NativeObservation::from_stat(&after)?, true) {
        return Err(RootedFsError::SourceBindingChanged(path.to_path_buf()));
    }
    Ok(object_key)
}

/// A raw filename, not a normalized path or a virtual-root interpretation.
fn sibling_name(target: &Path) -> Option<&OsStr> {
    let bytes = target.as_os_str().as_bytes();
    (!bytes.is_empty()
        && bytes != b"."
        && bytes != b".."
        && !bytes.contains(&b'/')
        && !bytes.contains(&0))
    .then_some(target.as_os_str())
}

fn source_read_race(error: RootedFsError, path: &Path) -> RootedFsError {
    match error {
        RootedFsError::Io(error)
            if matches!(
                error.raw_os_error(),
                Some(libc::ENOENT | libc::ENOTDIR | libc::ELOOP)
            ) =>
        {
            RootedFsError::SourceBindingChanged(path.to_path_buf())
        }
        error => error,
    }
}

// Only acquisition or authenticated operator replay supplies this effective
// observation. A selected scan flag or lexical target never qualifies a link.
fn inspect_selected(
    parent: RawFd,
    path: &Path,
    effective: NativeObservation,
    followed_object_key: Option<NativeObjectKey>,
) -> Result<Observation> {
    let mut observed = inspect(ResolutionRoot::Held(parent), parent, path, false)?;
    if observed.named.kind != libc::S_IFLNK as u64 {
        return Err(RootedFsError::SourceBindingChanged(path.to_path_buf()));
    }
    observed.followed = true;
    observed.effective = effective;
    observed.followed_object_key = followed_object_key;
    Ok(observed)
}

impl RootedFs {
    pub(crate) fn source_run_cancellation(
        &self,
        source_race: bool,
    ) -> Result<Arc<std::sync::atomic::AtomicBool>> {
        let owner = self
            .source_observations
            .lock()
            .map_err(|_| RootedFsError::SourceCatalogueTerminal)?;
        let CatalogueState::Active(catalogue) = &*owner else {
            return Err(RootedFsError::SourceCatalogueTerminal);
        };
        catalogue.check_lifetime()?;
        let expected = if source_race {
            RunState::SourceRace
        } else {
            RunState::Usable
        };
        if catalogue.cancelled.load(Ordering::Acquire) || catalogue.run != expected {
            return Err(RootedFsError::SourceCatalogueTerminal);
        }
        Ok(catalogue.cancelled.clone())
    }

    /// Called only by the joined LOCAL attempt boundary. Classification is a
    /// separate positive allowlist; unknown transactions cannot become races.
    pub(crate) fn retire_failed_source_run_blocking(&self, source_race: bool) -> Result<()> {
        let mut owner = self
            .source_observations
            .lock()
            .map_err(|_| RootedFsError::SourceCatalogueTerminal)?;
        let CatalogueState::Active(catalogue) = &mut *owner else {
            return Err(RootedFsError::SourceCatalogueTerminal);
        };
        catalogue.check_lifetime()?;
        if source_race
            && !catalogue.cancelled.load(Ordering::Acquire)
            && !catalogue.scanning
            && (catalogue.run == RunState::SourceRace
                || (catalogue.run == RunState::Usable && catalogue.healthy && catalogue.completed))
        {
            catalogue.run = RunState::SourceRace;
            Ok(())
        } else {
            catalogue.run = RunState::Terminal;
            Err(RootedFsError::SourceCatalogueTerminal)
        }
    }

    pub(super) fn is_source_authority(&self) -> bool {
        self.source_path_continuity
            || matches!(self.mutation_admission, RootedMutationAdmission::ReadOnly)
            || self
                .source_observations
                .lock()
                .map_or(true, |owner| !matches!(*owner, CatalogueState::Unstarted))
    }
    pub(crate) fn start_source_scan_blocking(&self) -> Result<SourceScanAttempt> {
        self.start_source_scan_with_scope(SourceRootProtection::Tree, None)
    }
    pub(crate) fn start_selected_source_scan_blocking(
        &self,
        path: &RelativePath,
    ) -> Result<SourceScanAttempt> {
        inventory::validate_original_path(path.as_path())?;
        self.start_source_scan_with_scope(
            SourceRootProtection::SelectedLeafAnchor,
            Some(path_key(path.as_path())),
        )
    }
    fn start_source_scan_with_scope(
        &self,
        scope: SourceRootProtection,
        selected: Option<[u8; 32]>,
    ) -> Result<SourceScanAttempt> {
        let attempt = SourceScanAttempt {
            rooted: self.clone(),
            successful: false,
        };
        self.begin_source_observations_blocking()?;
        let mut owner = self
            .source_observations
            .lock()
            .map_err(|_| RootedFsError::SourceCatalogueTerminal)?;
        let catalogue = owner.active()?;
        catalogue.begin()?;
        // An existing TREE authority may answer a narrower selected query;
        // that does not reclassify its root as a sibling-neutral anchor.
        let narrower = catalogue.scope == Some(SourceRootProtection::Tree)
            && scope == SourceRootProtection::SelectedLeafAnchor
            && selected.is_some();
        if catalogue.scanning
            || (catalogue.scope.is_some_and(|original| original != scope) && !narrower)
        {
            return Err(invalid().into());
        }
        if catalogue.scope.is_none() {
            catalogue.scope = Some(scope);
        }
        catalogue.selected = selected;
        catalogue.scanning = true;
        catalogue.seen = 0;
        catalogue.epoch = catalogue.epoch.checked_add(1).ok_or_else(invalid)?;
        catalogue.healthy = true;
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
            *owner = CatalogueState::Active(Box::new(Catalogue::new(self.root_fd.as_raw_fd())?));
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
        follow: bool,
    ) -> Result<()> {
        self.verify_source_root_path_blocking()?;
        inventory::validate_original_path(path)?;
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
            return Err(catalogue.changed(path.to_path_buf()));
        }
        if catalogue
            .selected
            .is_some_and(|selected| selected != path_key(path))
        {
            return Err(RootedFsError::SourceReferenceMismatch);
        }
        let original = catalogue.get(path_key(path))?;
        let terminal = catalogue.selected_terminal(self, path)?;
        let result = match terminal {
            Some(terminal) => {
                let result = if follow {
                    inspect_selected(
                        parent,
                        path,
                        NativeObservation::from_stat(&terminal.stat)?,
                        terminal.object_key,
                    )
                } else {
                    inspect(
                        ResolutionRoot::Held(self.root_fd.as_raw_fd()),
                        parent,
                        path,
                        false,
                    )
                };
                catalogue.original_link_result(self, path, result)
            }
            None => inspect(
                ResolutionRoot::Catalogued {
                    rooted: self,
                    catalogue,
                },
                parent,
                path,
                follow,
            ),
        };
        let result = result.map_err(|error| match error {
            RootedFsError::SourceFollowUnsupported(_) if original.is_some() => {
                RootedFsError::SourceBindingChanged(path.to_path_buf())
            }
            error => error,
        });
        let observed = catalogue.native_result(result)?;
        if let Some(original) = original {
            if !original.matches(&observed, true) {
                return Err(catalogue.changed(path.to_path_buf()));
            }
        } else {
            if catalogue.sealed
                || (catalogue.completed && catalogue.scope == Some(SourceRootProtection::Tree))
            {
                return Err(catalogue.changed(path.to_path_buf()));
            }
            catalogue.insert(path_key(path), &observed)?;
        }
        if !catalogue.scanning {
            return Err(invalid().into());
        }
        let seen_key = inventory::state_key(b"seen", path_key(path));
        let mut epoch = [0; 48];
        epoch[..8].copy_from_slice(&catalogue.epoch.to_le_bytes());
        if catalogue.index.get_bytes(seen_key)? == Some(epoch) {
            return Err(invalid().into());
        }
        catalogue.index.set_bytes(seen_key, epoch)?;
        catalogue.seen = catalogue.seen.checked_add(1).ok_or_else(invalid)?;
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
            return Err(catalogue.changed(path.as_path().to_path_buf()));
        }
        catalogue.healthy = true;
        drop(owner);
        self.validate_source_observation_blocking(path)
    }
    /// A DATA observation does not authorize unlinking its followed name. Refuse
    /// that unsupported combination during completed preflight, not after DEST
    /// publication. This check grants neither completion nor unlink authority.
    pub(crate) fn check_source_removal_support_blocking(&self, path: &RelativePath) -> Result<()> {
        self.validate_catalogued_source_blocking(path)?;
        let mut owner = self
            .source_observations
            .lock()
            .map_err(|_| RootedFsError::SourceCatalogueTerminal)?;
        let catalogue = owner.active()?;
        catalogue.begin()?;
        if catalogue.scanning || !catalogue.completed {
            return Err(RootedFsError::SourceInventoryIncomplete);
        }
        let original = catalogue.require(path.as_path())?;
        catalogue.healthy = true;
        if original.followed {
            Err(RootedFsError::SourceFollowRemovalUnsupported(
                path.as_path().to_path_buf(),
            ))
        } else {
            Ok(())
        }
    }

    /// Observation-only consumers must reference an existing original scan,
    /// even when this root has not otherwise been classified as SOURCE.
    pub(crate) fn validate_catalogued_source_blocking(&self, path: &RelativePath) -> Result<()> {
        if matches!(
            *self
                .source_observations
                .lock()
                .map_err(|_| RootedFsError::SourceCatalogueTerminal)?,
            CatalogueState::Unstarted
        ) {
            return Err(RootedFsError::SourceUnobserved(
                path.as_path().to_path_buf(),
            ));
        }
        self.validate_source_observation_blocking(path)
    }

    /// Validate every original ancestor and parent/name with bounded active
    /// directory descriptors. SOURCE never consults retirement lineage.
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
        let result = catalogue.validate_native_binding(self, path, strict);
        catalogue.native_result(result)?;
        catalogue.healthy = true;
        Ok(())
    }
}

impl Catalogue {
    /// The caller owns catalogue health and original operator-address checks.
    /// The returned cursor is the actual terminal reached through authenticated
    /// native ancestry, not a fresh reopening after validation. It grants only
    /// the caller's immediate read; originals are never refreshed.
    fn validate_native_binding(
        &mut self,
        rooted: &RootedFs,
        path: &Path,
        strict: bool,
    ) -> Result<Option<TerminalBinding>> {
        let mut directory = rooted.root_fd.try_clone()?;
        let mut relative = PathBuf::new();
        let mut components = path.components().peekable();
        while let Some(component) = components.next() {
            let Component::Normal(name) = component else {
                return Err(RootedFsError::InvalidRelativePath);
            };
            let ancestor = self.require(&relative)?;
            if !ancestor
                .effective
                .same_binding(NativeObservation::from_stat(&stat_fd(
                    directory.as_raw_fd(),
                )?)?)
            {
                return Err(RootedFsError::SourceBindingChanged(path.to_path_buf()));
            }
            relative.push(name);
            let original = self.require(&relative)?;
            let terminal = if original.followed {
                if components.peek().is_some() {
                    return Err(RootedFsError::SourceFollowUnsupported(relative));
                }
                self.selected_terminal(rooted, &relative)?
            } else {
                None
            };
            let (observed, stat, inspected_terminal) = if let Some(terminal) = &terminal {
                let result = inspect_selected(
                    directory.as_raw_fd(),
                    &relative,
                    NativeObservation::from_stat(&terminal.stat)?,
                    terminal.object_key,
                );
                (
                    self.original_link_result(rooted, &relative, result)?,
                    terminal.stat,
                    None,
                )
            } else {
                inspect_with_stat(
                    ResolutionRoot::Catalogued {
                        rooted,
                        catalogue: self,
                    },
                    directory.as_raw_fd(),
                    &relative,
                    original.followed,
                )
                .map_err(|error| match error {
                    RootedFsError::SourceFollowUnsupported(_) if original.followed => {
                        RootedFsError::SourceBindingChanged(relative.clone())
                    }
                    error => error,
                })?
            };
            // Own unlink continuation may change a file's ctime/nlink, never
            // its original binding or any link literal/ancestor observation.
            let exact = strict || original.named.kind == libc::S_IFLNK as u64;
            if !original.matches(&observed, exact) {
                return Err(RootedFsError::SourceBindingChanged(path.to_path_buf()));
            }
            if components.peek().is_none() {
                return Ok(Some(terminal.or(inspected_terminal).unwrap_or_else(|| {
                    TerminalBinding {
                        parent: directory,
                        name: name.to_os_string(),
                        stat,
                        object_key: observed.object_key,
                    }
                })));
            } else {
                directory = open_dir_at(directory.as_raw_fd(), name)?;
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
        Ok(None)
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod following_tests;

#[cfg(test)]
mod forward_tests;

#[cfg(all(test, target_os = "macos"))]
mod sibling_tests;
