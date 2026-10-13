//! Run-owned destination directory bindings. Two scratch FDs, one held anchor,
//! bounded paths, and shared authenticated ancestor references (not one FD/name).
use super::*;
use crate::authenticated_index::AuthenticatedIndex;
use crate::engine::namespace::{
    Folding, NamespaceParent, NamespacePreflightError, NamespaceQualification, NamespaceSemantics,
    Result as NamespaceResult,
};
use crate::fs_util::namespace::{qualify_directory, DirectoryNamespace};
use futures::future::BoxFuture;
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::Mutex;

const MAX_PATH: usize = 64 * 1024;
const MAX_RECORD: usize = MAX_PATH + 128;
type Key = [u8; 32];
pub(super) type Shared = Arc<Mutex<Catalogue>>;

/// The same concrete owner is retained by the destination and qualification RPC.
#[derive(Clone)]
pub struct DestinationNamespace {
    owner: Shared,
    prefix: PathBuf,
}
impl std::fmt::Debug for DestinationNamespace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DestinationNamespace")
            .field("prefix", &self.prefix)
            .finish_non_exhaustive()
    }
}

fn invalid() -> RootedFsError {
    RootedFsError::NamespaceCatalogueTerminal
}
fn key(path: &Path) -> Key {
    let mut h = blake3::Hasher::new();
    h.update(b"sy/destination-namespace/path/v1\0");
    h.update(path.as_os_str().as_bytes());
    *h.finalize().as_bytes()
}
fn ordinal(n: u64) -> Key {
    let mut h = blake3::Hasher::new();
    h.update(b"sy/destination-namespace/record/v1\0");
    h.update(&n.to_le_bytes());
    *h.finalize().as_bytes()
}
fn bounded(path: &Path) -> Result<()> {
    if path.as_os_str().as_bytes().len() > MAX_PATH
        || path
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(RootedFsError::InvalidRelativePath);
    }
    Ok(())
}
fn axes(s: NamespaceSemantics) -> [u8; 2] {
    fn axis(f: Folding) -> u8 {
        match f {
            Folding::Exact => 0,
            Folding::Folded => 1,
            Folding::Unspecified => 2,
        }
    }
    [axis(s.case), axis(s.normalization)]
}
fn decode_axes(b: &[u8]) -> Result<NamespaceSemantics> {
    fn axis(b: u8) -> Result<Folding> {
        match b {
            0 => Ok(Folding::Exact),
            1 => Ok(Folding::Folded),
            2 => Ok(Folding::Unspecified),
            _ => Err(invalid()),
        }
    }
    Ok(NamespaceSemantics {
        case: axis(b[0])?,
        normalization: axis(b[1])?,
    })
}

#[derive(Clone)]
struct Record {
    parent: Key,
    name: OsString,
    observed: Option<(u64, u64)>,
    profile: DirectoryNamespace,
}
impl Record {
    fn encode(&self, k: Key) -> Vec<u8> {
        let mut b = Vec::with_capacity(112 + self.name.as_bytes().len());
        b.extend_from_slice(&k);
        b.extend_from_slice(&self.parent);
        b.push(u8::from(self.observed.is_some()));
        let (dev, ino) = self.observed.unwrap_or((0, 0));
        for n in [dev, ino] {
            b.extend_from_slice(&n.to_le_bytes());
        }
        b.extend_from_slice(&axes(self.profile.semantics));
        b.push(u8::from(self.profile.inherits));
        for n in self.profile.filesystem {
            b.extend_from_slice(&n.to_le_bytes());
        }
        b.extend_from_slice(self.name.as_bytes());
        b
    }
    fn decode(b: &[u8]) -> Result<(Key, Self)> {
        if b.len() < 108 || b[64] > 1 || b[83] > 1 {
            return Err(invalid());
        }
        let u64_at = |i| -> Result<u64> {
            Ok(u64::from_le_bytes(
                b[i..i + 8].try_into().map_err(|_| invalid())?,
            ))
        };
        let k = b[..32].try_into().map_err(|_| invalid())?;
        let parent = b[32..64].try_into().map_err(|_| invalid())?;
        let name = OsStr::from_bytes(&b[108..]).to_owned();
        if name.as_bytes().contains(&0) || name.as_bytes().contains(&b'/') {
            return Err(invalid());
        }
        Ok((
            k,
            Self {
                parent,
                name,
                observed: if b[64] == 1 {
                    Some((u64_at(65)?, u64_at(73)?))
                } else {
                    None
                },
                profile: DirectoryNamespace {
                    semantics: decode_axes(&b[81..83])?,
                    inherits: b[83] != 0,
                    filesystem: [u64_at(84)?, u64_at(92)?, u64_at(100)?],
                },
            },
        ))
    }
    fn observed(fd: RawFd, parent: Key, name: OsString) -> Result<Self> {
        let stat = stat_fd(fd)?;
        if stat.st_mode & libc::S_IFMT != libc::S_IFDIR {
            return Err(invalid());
        }
        // SAFETY: caller retains the live directory for this synchronous query.
        let profile = qualify_directory(unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) })?;
        Ok(Self {
            parent,
            name,
            observed: Some((stat.st_dev as u64, stat.st_ino as u64)),
            profile,
        })
    }
    fn prediction(parent: Key, name: OsString, profile: DirectoryNamespace) -> Self {
        Self {
            parent,
            name,
            observed: None,
            profile: if profile.inherits {
                profile
            } else {
                DirectoryNamespace {
                    semantics: NamespaceSemantics::UNSPECIFIED,
                    filesystem: [0; 3],
                    inherits: false,
                }
            },
        }
    }
    fn verify(&self, fd: RawFd, path: &Path) -> Result<()> {
        let current = Self::observed(fd, self.parent, self.name.clone())?;
        if self.observed != current.observed || self.profile != current.profile {
            return Err(RootedFsError::DestinationChanged(path.to_path_buf()));
        }
        Ok(())
    }
}

pub(super) struct Catalogue {
    // Only mkdir preparation is serialized; byte transfers never hold this.
    creation: Arc<Mutex<()>>,
    anchor: Arc<OwnedFd>,
    index: AuthenticatedIndex<48>,
    tape: File,
    end: u64,
    count: u64,
    healthy: bool,
    sealed: bool,
}
impl Catalogue {
    fn new(anchor: Arc<OwnedFd>) -> Result<Self> {
        let root = Record::observed(anchor.as_raw_fd(), key(Path::new("")), OsString::new())?;
        let mut c = Self {
            creation: Arc::new(Mutex::new(())),
            anchor,
            index: AuthenticatedIndex::new()?,
            tape: tempfile::tempfile()?,
            end: 0,
            count: 0,
            healthy: true,
            sealed: false,
        };
        c.insert(key(Path::new("")), &root)?;
        Ok(c)
    }
    fn operation<T>(&mut self, f: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        if !self.healthy {
            return Err(invalid());
        }
        self.healthy = false;
        if self.tape.metadata()?.len() != self.end {
            return Err(invalid());
        }
        let result = f(self);
        if result.is_ok() {
            self.healthy = true;
        }
        result
    }
    fn read_locator(&mut self, locator: [u8; 48]) -> Result<(Key, Record)> {
        let offset = u64::from_le_bytes(locator[..8].try_into().map_err(|_| invalid())?);
        let length = u64::from_le_bytes(locator[8..16].try_into().map_err(|_| invalid())?);
        if length > MAX_RECORD as u64 || offset.checked_add(length).is_none_or(|end| end > self.end)
        {
            return Err(invalid());
        }
        let mut b = vec![0; length as usize];
        self.tape.seek(SeekFrom::Start(offset))?;
        self.tape.read_exact(&mut b)?;
        // Authentication precedes ALL record decoding, including ancestor refs.
        if blake3::hash(&b).as_bytes() != &locator[16..] {
            return Err(invalid());
        }
        Record::decode(&b)
    }
    fn get(&mut self, k: Key) -> Result<Option<Record>> {
        let Some(locator) = self.index.get_bytes(k)? else {
            return Ok(None);
        };
        let (stored, record) = self.read_locator(locator)?;
        if stored != k {
            return Err(invalid());
        }
        Ok(Some(record))
    }
    fn insert(&mut self, k: Key, r: &Record) -> Result<()> {
        let b = r.encode(k);
        if b.len() > MAX_RECORD {
            return Err(invalid());
        }
        let next = self.end.checked_add(b.len() as u64).ok_or_else(invalid)?;
        let count = self.count.checked_add(1).ok_or_else(invalid)?;
        let mut locator = [0; 48];
        locator[..8].copy_from_slice(&self.end.to_le_bytes());
        locator[8..16].copy_from_slice(&(b.len() as u64).to_le_bytes());
        locator[16..].copy_from_slice(blake3::hash(&b).as_bytes());
        self.tape.seek(SeekFrom::Start(self.end))?;
        self.tape.write_all(&b)?;
        self.index.set_bytes(k, locator)?;
        self.index.set_bytes(ordinal(self.count), locator)?;
        self.end = next;
        self.count = count;
        Ok(())
    }
    fn walk(&mut self, path: &Path, capture: bool) -> Result<(Record, Option<OwnedFd>)> {
        let anchor = Arc::clone(&self.anchor);
        self.walk_from(Path::new(""), &anchor, path, capture)
    }
    /// The base record remains original, but its held FD replaces lookup above
    /// that base after root acquisition. Below it, every original binding and
    /// profile is mandatory. At most two traversal descriptors stay live;
    /// missing suffixes never authorize adopting later-arriving directories.
    fn walk_from(
        &mut self,
        base: &Path,
        held: &OwnedFd,
        path: &Path,
        capture: bool,
    ) -> Result<(Record, Option<OwnedFd>)> {
        self.walk_from_checked(base, held, path, capture, false)
    }
    fn walk_from_checked(
        &mut self,
        base: &Path,
        held: &OwnedFd,
        path: &Path,
        capture: bool,
        literal: bool,
    ) -> Result<(Record, Option<OwnedFd>)> {
        bounded(base)?;
        bounded(path)?;
        bounded(&base.join(path))?;
        let mut prefix = base.to_path_buf();
        let mut record = self.get(key(&prefix))?.ok_or_else(invalid)?;
        record.verify(held.as_raw_fd(), &prefix)?;
        let mut fd = Some(held.try_clone()?);
        for part in path.components() {
            let Component::Normal(name) = part else {
                return Err(RootedFsError::InvalidRelativePath);
            };
            let parent_key = key(&prefix);
            if literal {
                if let Some(parent) = &fd {
                    verify_literal_name(parent.as_raw_fd(), name, &prefix.join(name))?;
                }
            }
            prefix.push(name);
            let k = key(&prefix);
            let original = self.get(k)?;
            let (next, child) = match (&fd, original) {
                (Some(parent), Some(original)) => match original.observed {
                    Some(_) => {
                        let child = open_dir_at(parent.as_raw_fd(), name)?;
                        original.verify(child.as_raw_fd(), &prefix)?;
                        (original, Some(child))
                    }
                    None => {
                        if stat_at_optional(parent.as_raw_fd(), name)?.is_some() {
                            return Err(RootedFsError::DestinationChanged(prefix));
                        }
                        (original, None)
                    }
                },
                (None, Some(original)) if original.observed.is_none() => (original, None),
                (_, Some(_)) => return Err(invalid()),
                (Some(parent), None) if capture && !self.sealed => {
                    match open_dir_at(parent.as_raw_fd(), name) {
                        Ok(child) => (
                            Record::observed(child.as_raw_fd(), parent_key, name.to_owned())?,
                            Some(child),
                        ),
                        Err(RootedFsError::Io(e)) if e.raw_os_error() == Some(libc::ENOENT) => (
                            Record::prediction(parent_key, name.to_owned(), record.profile),
                            None,
                        ),
                        Err(e) => return Err(e),
                    }
                }
                (None, None) if capture && !self.sealed => (
                    Record::prediction(parent_key, name.to_owned(), record.profile),
                    None,
                ),
                _ => return Err(RootedFsError::NamespaceUnqualified(prefix)),
            };
            if next.parent != parent_key || next.name != name {
                return Err(invalid());
            }
            if self.get(k)?.is_none() {
                self.insert(k, &next)?;
            }
            record = next;
            fd = child;
        }
        Ok((record, fd))
    }
    /// Native lookup determines equivalence; enumeration proves literal spelling.
    /// Parent lookups consume only this catalogue's original observations or
    /// qualified absences. They never capture/adopt a newly arriving directory.
    fn validate_backup_address(&mut self, base: &Path, held: &OwnedFd, path: &Path) -> Result<()> {
        bounded(path)?;
        let parent_path = path.parent().ok_or(RootedFsError::InvalidRelativePath)?;
        let name = path.file_name().ok_or(RootedFsError::InvalidRelativePath)?;
        let (_, parent) = self.walk_from_checked(base, held, parent_path, false, true)?;
        if let Some(parent) = parent {
            verify_literal_name(parent.as_raw_fd(), name, &base.join(path))?;
        }
        Ok(())
    }

    fn record_path(&mut self, mut k: Key, mut r: Record) -> Result<PathBuf> {
        let root = key(Path::new(""));
        let mut names = Vec::new();
        let mut bytes = 0usize;
        while k != root {
            bytes = bytes
                .checked_add(r.name.as_bytes().len() + 1)
                .filter(|n| *n <= MAX_PATH)
                .ok_or_else(invalid)?;
            if r.name.is_empty() {
                return Err(invalid());
            }
            names.push(r.name);
            k = r.parent;
            r = self.get(k)?.ok_or_else(invalid)?;
        }
        let mut path = PathBuf::new();
        for name in names.into_iter().rev() {
            path.push(name);
        }
        Ok(path)
    }
    fn seal(&mut self) -> Result<()> {
        self.operation(|c| {
            // The native count and ordinal index establish exact exhaustion.
            for n in 0..c.count {
                let locator = c.index.get_bytes(ordinal(n))?.ok_or_else(invalid)?;
                let (k, record) = c.read_locator(locator)?;
                let path = c.record_path(k, record)?;
                if key(&path) != k {
                    return Err(invalid());
                }
                c.walk(&path, false)?;
            }
            c.sealed = true;
            Ok(())
        })
    }
}

impl NamespaceQualification for DestinationNamespace {
    fn qualify<'a>(
        &'a self,
        parent: NamespaceParent,
    ) -> BoxFuture<'a, NamespaceResult<NamespaceSemantics>> {
        let this = self.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                let path = match parent {
                    NamespaceParent::Root => this.prefix,
                    NamespaceParent::Directory(path) => this.prefix.join(path.as_path()),
                };
                let mut c = this.owner.lock().map_err(|_| invalid())?;
                c.operation(|c| Ok(c.walk(&path, true)?.0.profile.semantics))
            })
            .await
            .map_err(|e| NamespacePreflightError::Worker(e.to_string()))?
            .map_err(namespace_error)
        })
    }
    fn validate_backup_address(&self, path: RelativePath) -> BoxFuture<'_, NamespaceResult<()>> {
        let this = self.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                let mut c = this.owner.lock().map_err(|_| invalid())?;
                c.operation(|c| {
                    let anchor = Arc::clone(&c.anchor);
                    c.validate_backup_address(
                        Path::new(""),
                        &anchor,
                        &this.prefix.join(path.as_path()),
                    )
                })
            })
            .await
            .map_err(|e| NamespacePreflightError::Worker(e.to_string()))?
            .map_err(namespace_error)
        })
    }

    fn seal(&self) -> BoxFuture<'_, NamespaceResult<()>> {
        let this = self.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || this.owner.lock().map_err(|_| invalid())?.seal())
                .await
                .map_err(|e| NamespacePreflightError::Worker(e.to_string()))?
                .map_err(namespace_error)
        })
    }
}
fn namespace_error(e: RootedFsError) -> NamespacePreflightError {
    NamespacePreflightError::Io(std::io::Error::other(e))
}

/// Always drain the bounded stream: an I/O error after the matching name
/// remains a failed mandatory native read, not a successful spelling proof.
fn verify_literal_name(parent: RawFd, name: &OsStr, path: &Path) -> Result<()> {
    if stat_at_optional(parent, name)?.is_none() {
        return Ok(());
    }
    let mut exact = false;
    for entry in super::scan::DirectoryStream::open(parent)? {
        exact |= entry?.as_bytes() == name.as_bytes();
    }
    if !exact {
        return Err(RootedFsError::BackupAddressAlias(path.to_path_buf()));
    }
    Ok(())
}

impl RootedFs {
    pub(super) fn verify_backup_address_blocking(&self, path: &Path) -> Result<()> {
        self.require_namespace_sealed()?;
        let owner = self
            .destination_namespace
            .as_ref()
            .ok_or_else(|| RootedFsError::NamespaceUnqualified(path.to_path_buf()))?;
        let mut c = owner.lock().map_err(|_| invalid())?;
        c.operation(|c| c.validate_backup_address(&self.namespace_prefix, &self.root_fd, path))
    }

    /// Bind only the returned authority to a fresh run. The controller/server
    /// must join previous execution before using it; retained clones and proofs
    /// are not execution ownership and continue to carry their original run.
    pub async fn with_namespace_qualification(self) -> Result<(Self, DestinationNamespace)> {
        self.with_namespace_prefix(PathBuf::new()).await
    }

    pub(super) async fn with_namespace_prefix(
        mut self,
        prefix: PathBuf,
    ) -> Result<(Self, DestinationNamespace)> {
        tokio::task::spawn_blocking(move || {
            bounded(&prefix)?;
            // A fresh owner starts at the same held root, never its configured
            // spelling. Pending preflight still predicts from its held ancestor;
            // acquisition retains that owner's records and prefix. Old clones
            // keep their old mapping, while local callers enforce path continuity.
            let mut c = Catalogue::new(Arc::clone(&self.root_fd))?;
            c.operation(|c| c.walk(&prefix, true).map(|_| ()))?;
            self.namespace_prefix = PathBuf::new();
            let owner = Arc::new(Mutex::new(c));
            self.destination_namespace = Some(Arc::clone(&owner));
            self.namespace_qualification_required = true;
            Ok((self, DestinationNamespace { owner, prefix }))
        })
        .await
        .map_err(|e| RootedFsError::Worker(e.to_string()))?
    }

    pub(super) fn namespace_creation_gate(&self) -> Result<Option<Arc<Mutex<()>>>> {
        self.destination_namespace
            .as_ref()
            .map(|owner| {
                owner
                    .lock()
                    .map(|catalogue| Arc::clone(&catalogue.creation))
                    .map_err(|_| invalid())
            })
            .transpose()
    }

    pub(super) fn require_namespace_sealed(&self) -> Result<()> {
        if let Some(owner) = &self.destination_namespace {
            let c = owner.lock().map_err(|_| invalid())?;
            if !c.healthy {
                return Err(invalid());
            }
            if !c.sealed {
                return Err(RootedFsError::NamespaceUnsealed);
            }
        } else if self.namespace_qualification_required {
            return Err(RootedFsError::NamespaceUnsealed);
        }
        Ok(())
    }
    /// Check the exact FD used by the effect against original ancestry AND
    /// its own native profile, not just a freshly reopened parent inode.
    pub(super) fn verify_namespace_directory(&self, path: &Path, held: RawFd) -> Result<()> {
        if let Some(owner) = &self.destination_namespace {
            let mut c = owner.lock().map_err(|_| invalid())?;
            if !c.healthy {
                return Err(invalid());
            }
            if !c.sealed {
                return Ok(());
            } // Read-only scan before qualification.
            c.operation(|c| {
                let (record, _) =
                    c.walk_from(&self.namespace_prefix, &self.root_fd, path, false)?;
                record.verify(held, &self.namespace_prefix.join(path))
            })?;
        }
        Ok(())
    }
    pub(super) fn namespace_predicts_absent(&self, path: &Path, parent: RawFd) -> Result<bool> {
        self.require_namespace_sealed()?;
        let Some(owner) = &self.destination_namespace else {
            return Ok(false);
        };
        let mut c = owner.lock().map_err(|_| invalid())?;
        c.operation(|c| {
            let (record, _) = c.walk_from(&self.namespace_prefix, &self.root_fd, path, false)?;
            let parent_path = path.parent().ok_or_else(invalid)?;
            let (original, _) =
                c.walk_from(&self.namespace_prefix, &self.root_fd, parent_path, false)?;
            original.verify(parent, &self.namespace_prefix.join(parent_path))?;
            Ok(record.observed.is_none())
        })
    }
    pub(super) fn advance_namespace_directory(
        &self,
        path: &Path,
        parent: RawFd,
        child: RawFd,
    ) -> Result<()> {
        let Some(owner) = &self.destination_namespace else {
            return Ok(());
        };
        let parent_path = path.parent().ok_or_else(invalid)?;
        let path = self.namespace_prefix.join(path);
        let mut c = owner.lock().map_err(|_| invalid())?;
        c.operation(|c| {
            let original = c.get(key(&path))?.ok_or_else(invalid)?;
            if original.observed.is_some() {
                return Err(RootedFsError::DestinationChanged(path.clone()));
            }
            let (parent_record, _) =
                c.walk_from(&self.namespace_prefix, &self.root_fd, parent_path, false)?;
            parent_record.verify(parent, &self.namespace_prefix.join(parent_path))?;
            let observed = Record::observed(child, original.parent, original.name.clone())?;
            let named = stat_at_optional(parent, &original.name)?.ok_or_else(invalid)?;
            if observed.observed != Some((named.st_dev as u64, named.st_ino as u64))
                || named.st_mode & libc::S_IFMT != libc::S_IFDIR
            {
                return Err(RootedFsError::DestinationChanged(path.clone()));
            }
            // Unknown prediction was collision-checked conservatively. Qualified
            // inheritance must match both filesystem facts and the native axes.
            if original.profile.inherits && original.profile != observed.profile {
                return Err(RootedFsError::DestinationChanged(path.clone()));
            }
            c.insert(key(&path), &observed)
        })
    }
}

#[cfg(test)]
mod backup_address_tests;
#[cfg(test)]
mod tests;
