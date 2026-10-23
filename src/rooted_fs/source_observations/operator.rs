//! Native operator-address provenance. This is binding-only evidence, not
//! SOURCE membership or permission to inspect siblings/ancestor file data.
use super::*;
use crate::rooted_fs::operand::{BoundSource, SourceShape};
use std::collections::VecDeque;

const MAX_STEPS: usize = 4096;
const MAX_RESOLUTION_BYTES: usize = crate::protocol::MAX_WIRE_PATH_BYTES;
const MAX_LINKS: usize = 40;

struct SelectedLink {
    parent: OwnedFd,
    name: OsString,
    named: NativeObservation,
    target: LinkTarget,
    prefix_records: usize,
}

#[derive(Clone, Copy)]
struct OriginalFollowedLeaf {
    observation: [u8; 32],
    // The final native parent/name in the operator tape, not SOURCE membership
    // or a new operational root. Its parent chain remains binding-only evidence.
    terminal: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LeafData {
    CurrentRun,
    NewRun,
}

pub(super) struct OriginalAddress {
    cwd: Option<Arc<OwnedFd>>,
    count: u64,
    start: u64,
    end: u64,
    root: u64,
    followed_leaf: Option<OriginalFollowedLeaf>,
}

fn record_key(ordinal: u64) -> [u8; 32] {
    let mut hash = blake3::Hasher::new();
    hash.update(b"sy/source-operator-address/ordinal/v1\0");
    hash.update(&ordinal.to_le_bytes());
    *hash.finalize().as_bytes()
}

/// Ancestors need search/metadata authority, not directory enumeration. The
/// selected tree's read descriptor is acquired separately through this cursor.
pub(super) fn directory_at(parent: RawFd, name: &OsStr) -> Result<OwnedFd> {
    let name = component_cstring(name)?;
    #[cfg(target_os = "linux")]
    let access = libc::O_PATH;
    #[cfg(target_os = "macos")]
    let access = libc::O_SEARCH;
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    return Err(RootedFsError::UnsupportedPlatform);
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        // SAFETY: parent is held (or AT_FDCWD), name is live; no link is followed.
        let fd = unsafe {
            libc::openat(
                parent,
                name.as_ptr(),
                access | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error().into());
        }
        // SAFETY: successful openat returned a fresh exclusively owned FD.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

fn components(path: &Path, budget: &mut usize) -> Result<VecDeque<OsString>> {
    let bytes = path.as_os_str().as_bytes();
    *budget = budget.checked_add(bytes.len()).ok_or_else(invalid)?;
    if bytes.contains(&0) {
        return Err(RootedFsError::PathContainsNul);
    }
    if *budget > MAX_RESOLUTION_BYTES {
        return Err(invalid().into());
    }
    // Do not use Path::components: '..' and '.' retain native traversal meaning,
    // including after a symlink target, rather than lexical cancellation.
    Ok(bytes
        .split(|byte| *byte == b'/')
        .filter(|name| !name.is_empty())
        .map(|name| OsStr::from_bytes(name).to_os_string())
        .collect())
}

fn push(
    records: &mut Vec<Observation>,
    name: OsString,
    named: NativeObservation,
    effective: NativeObservation,
    target: Option<LinkTarget>,
) -> Result<()> {
    if records.len() == MAX_STEPS {
        return Err(invalid().into());
    }
    let parent = if records.is_empty() {
        [0; 32]
    } else {
        record_key(records.len() as u64 - 1)
    };
    records.push(Observation {
        parent,
        name,
        named,
        effective,
        followed: target.is_some(),
        object_key: None,
        followed_object_key: None,
        target,
        resolution_start: 0,
        resolution: Vec::new(),
    });
    Ok(())
}

pub(in crate::rooted_fs) fn acquire(
    path: PathBuf,
    contents: bool,
    follow: bool,
) -> Result<BoundSource> {
    if path.as_os_str().is_empty() {
        return Err(io::Error::from_raw_os_error(libc::ENOENT).into());
    }
    let absolute = path.is_absolute();
    let start = directory_at(libc::AT_FDCWD, OsStr::new(if absolute { "/" } else { "." }))?;
    let (cwd, mut directory) = if absolute {
        (None, start)
    } else {
        let directory = start.try_clone()?;
        (Some(Arc::new(start)), directory)
    };
    let native = NativeObservation::from_stat(&stat_fd(directory.as_raw_fd())?)?;
    let mut records = Vec::new();
    push(&mut records, OsString::new(), native, native, None)?;
    let mut budget = 0;
    let mut pending = components(&path, &mut budget)?;
    let requires_directory =
        path.as_os_str().as_bytes().ends_with(b"/") || path.as_os_str().as_bytes().ends_with(b"/.");
    let mut links = 0;
    let mut leaf = None;
    let mut selected_link: Option<SelectedLink> = None;
    let mut followed_leaf = None;
    while let Some(name) = pending.pop_front() {
        let named =
            NativeObservation::from_stat(&stat_at_no_follow(directory.as_raw_fd(), &name)?)?;
        let last = pending.is_empty();
        if last
            && named.kind != libc::S_IFDIR as u64
            && !requires_directory
            && !(named.kind == libc::S_IFLNK as u64 && follow)
        {
            if let Some(selected) = selected_link.take() {
                let original = inspect_selected(
                    selected.parent.as_raw_fd(),
                    Path::new(&selected.name),
                    named,
                    object_key(directory.as_raw_fd(), Some(&name))?,
                )?;
                if !original.named.matches(selected.named, true)
                    || original.target.as_ref() != Some(&selected.target)
                    || !original.effective.matches(named, true)
                {
                    return Err(RootedFsError::RootChanged(path));
                }
                // Retain the entire native target resolution, including its
                // terminal parent/name. The operational ROOT remains the
                // ORIGINAL selected link's parent, never its effective parent.
                push(&mut records, name, named, named, None)?;
                followed_leaf = Some((
                    selected.prefix_records as u64 - 1,
                    OriginalFollowedLeaf {
                        observation: key([0; 32], &selected.name),
                        terminal: records.len() as u64 - 1,
                    },
                ));
                directory = selected.parent;
                leaf = Some((selected.name, original));
            } else {
                let original = inspect(
                    directory.as_raw_fd(),
                    directory.as_raw_fd(),
                    Path::new(&name),
                    false,
                )?;
                if !original.named.matches(named, true) {
                    return Err(RootedFsError::RootChanged(path));
                }
                leaf = Some((name, original));
            }
            break;
        }
        if named.kind == libc::S_IFLNK as u64 {
            links += 1;
            if links > MAX_LINKS {
                return Err(io::Error::from_raw_os_error(libc::ELOOP).into());
            }
            let target = target_at(directory.as_raw_fd(), &name)?;
            let after =
                NativeObservation::from_stat(&stat_at_no_follow(directory.as_raw_fd(), &name)?)?;
            if !named.matches(after, true) || target_at(directory.as_raw_fd(), &name)? != target {
                return Err(RootedFsError::RootChanged(path));
            }
            if last && !requires_directory && selected_link.is_none() {
                selected_link = Some(SelectedLink {
                    parent: directory.try_clone()?,
                    name: name.clone(),
                    named,
                    target: target.clone(),
                    prefix_records: records.len(),
                });
            }
            let mut expansion = components(&target, &mut budget)?;
            push(&mut records, name, named, named, Some(target.clone()))?;
            if target.is_absolute() {
                directory = directory_at(libc::AT_FDCWD, OsStr::new("/"))?;
                let root = NativeObservation::from_stat(&stat_fd(directory.as_raw_fd())?)?;
                push(&mut records, OsString::from("/"), root, root, None)?;
            }
            expansion.append(&mut pending);
            pending = expansion;
            continue;
        }
        if named.kind != libc::S_IFDIR as u64 {
            return Err(io::Error::from_raw_os_error(libc::ENOTDIR).into());
        }
        let next = directory_at(directory.as_raw_fd(), &name)?;
        let opened = NativeObservation::from_stat(&stat_fd(next.as_raw_fd())?)?;
        let after =
            NativeObservation::from_stat(&stat_at_no_follow(directory.as_raw_fd(), &name)?)?;
        if !named.same_binding(opened) || !named.same_binding(after) {
            return Err(RootedFsError::RootChanged(path));
        }
        push(&mut records, name, named, opened, None)?;
        directory = next;
    }
    drop(selected_link);
    // Acquire the actual tree/leaf-parent FD through the original resolution,
    // never by reopening the operator pathname after classification.
    let root_fd = open_dir_at(directory.as_raw_fd(), OsStr::new("."))?;
    let held = NativeObservation::from_stat(&stat_fd(root_fd.as_raw_fd())?)?;
    if !held.same_binding(NativeObservation::from_stat(&stat_fd(
        directory.as_raw_fd(),
    )?)?) {
        return Err(RootedFsError::RootChanged(path));
    }
    drop(directory);
    let (root_path, shape) = match &leaf {
        Some((leaf, _)) => {
            let name = RelativePath::new(PathBuf::from(leaf))
                .map_err(|_| RootedFsError::InvalidRelativePath)?;
            let parent = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            (parent.to_path_buf(), SourceShape::Leaf { name })
        }
        None => {
            let basename = if contents {
                None
            } else {
                path.file_name()
                    .map(|name| RelativePath::new(PathBuf::from(name)))
                    .transpose()
                    .map_err(|_| RootedFsError::InvalidRelativePath)?
            };
            (path, SourceShape::Directory { basename })
        }
    };
    // The ephemeral transcript is bounded by operand bytes, 40 link hops and
    // MAX_STEPS, not tree size. Delay ALL scratch writes until its actual held
    // root qualifies the scratch location; then discard the temporary records.
    let guard = File::from(root_fd.try_clone()?);
    crate::engine::scan_sort::validate_scratch_root(&guard, &tempfile::env::temp_dir())?;
    drop(guard);
    let mut catalogue = Catalogue::new(root_fd.as_raw_fd())?;
    let start = catalogue.end;
    let count = records.len() as u64;
    for (ordinal, record) in records.iter().enumerate() {
        catalogue.append(record_key(ordinal as u64), record)?;
    }
    drop(records);
    catalogue.operator = Some(OriginalAddress {
        cwd,
        count,
        start,
        end: catalogue.end,
        root: followed_leaf.map_or(count - 1, |(parent, _)| parent),
        followed_leaf: followed_leaf.map(|(_, leaf)| leaf),
    });
    if let Some((name, original)) = leaf {
        catalogue.insert(key([0; 32], &name), &original)?;
    }
    let mut rooted = RootedFs::from_operand_fd(root_path, root_fd);
    catalogue.begin()?;
    catalogue.validate_operator(&rooted, LeafData::CurrentRun)?;
    catalogue.healthy = true;
    rooted.source_path_continuity = true;
    *rooted
        .source_observations
        .lock()
        .map_err(|_| RootedFsError::SourceCatalogueTerminal)? =
        CatalogueState::Active(Box::new(catalogue));
    Ok(BoundSource { rooted, shape })
}

impl Catalogue {
    pub(super) fn selected_terminal(
        &mut self,
        rooted: &RootedFs,
        path: &Path,
    ) -> Result<Option<TerminalBinding>> {
        let Some(leaf) = self
            .operator
            .as_ref()
            .and_then(|address| address.followed_leaf)
        else {
            return Ok(None);
        };
        if leaf.observation != path_key(path) {
            return Ok(None);
        }
        if self.scope != Some(SourceRootProtection::SelectedLeafAnchor)
            || self.selected != Some(path_key(path))
        {
            return Err(RootedFsError::SourceFollowUnsupported(path.to_path_buf()));
        }
        self.replay_address(rooted, LeafData::CurrentRun)
    }

    fn validate_operator(
        &mut self,
        rooted: &RootedFs,
        data: LeafData,
    ) -> Result<Option<TerminalBinding>> {
        let address = self
            .operator
            .as_ref()
            .ok_or(RootedFsError::SourceCatalogueTerminal)?;
        let (count, start, end, root) = (address.count, address.start, address.end, address.root);
        let cwd = address.cwd.clone();
        let leaf = address.followed_leaf;
        let target = leaf.map(|leaf| leaf.terminal);
        if count == 0
            || count > MAX_STEPS as u64
            || start >= end
            || end > self.end
            || root >= count
            || match target {
                Some(target) => target != count - 1 || root + 1 >= target,
                None => root != count - 1,
            }
        {
            return Err(invalid().into());
        }
        let original_leaf = leaf
            .map(|leaf| -> Result<_> {
                self.get(leaf.observation)?.ok_or_else(|| invalid().into())
            })
            .transpose()?;
        if let Some(original) = &original_leaf {
            if original.parent != [0; 32]
                || key([0; 32], &original.name) != leaf.ok_or_else(invalid)?.observation
                || !original.followed
                || original.named.kind != libc::S_IFLNK as u64
                || original.effective.kind == libc::S_IFDIR as u64
            {
                return Err(invalid().into());
            }
        }
        let mut cursor = None;
        let mut terminal_data = None;
        let mut offset = start;
        let mut previous = [0; 32];
        for ordinal in 0..count {
            let key = record_key(ordinal);
            let locator = self.index.get_bytes(key)?.ok_or_else(invalid)?;
            let record_offset = u64::from_le_bytes(locator[..8].try_into().map_err(|_| invalid())?);
            let length = u64::from_le_bytes(locator[8..16].try_into().map_err(|_| invalid())?);
            offset = offset.checked_add(length).ok_or_else(invalid)?;
            if record_offset != offset - length || offset > end {
                return Err(invalid().into());
            }
            // Authenticate before decoding names/targets or making any syscall
            // selected by scratch contents. Count/exhaustion stay run-owned.
            let record = self.read_locator(locator)?;
            if record.parent != previous
                || record.object_key.is_some()
                || record.followed_object_key.is_some()
            {
                return Err(invalid().into());
            }
            if target.is_some() && ordinal == root + 1 {
                let original = original_leaf.as_ref().ok_or_else(invalid)?;
                if record.name != original.name
                    || record.named != original.named
                    || record.target != original.target
                    || !record.followed
                {
                    return Err(invalid().into());
                }
            }
            // A followed selected leaf leaves this original LINK parent in
            // the middle of the tape. Its FD never becomes the target parent.
            if target.is_some()
                && ordinal == root
                && !record
                    .effective
                    .same_binding(NativeObservation::from_stat(&stat_fd(
                        rooted.root_fd.as_raw_fd(),
                    )?)?)
            {
                return Err(RootedFsError::RootChanged(
                    rooted.root_path.as_ref().clone(),
                ));
            }
            if ordinal == 0 || record.name == OsStr::new("/") {
                if record.target.is_some()
                    || record.followed
                    || (ordinal == 0 && !record.name.is_empty())
                {
                    return Err(invalid().into());
                }
                let next = directory_at(
                    libc::AT_FDCWD,
                    OsStr::new(if ordinal == 0 && cwd.is_some() {
                        "."
                    } else {
                        "/"
                    }),
                )?;
                if !record
                    .effective
                    .same_binding(NativeObservation::from_stat(&stat_fd(next.as_raw_fd())?)?)
                {
                    return Err(RootedFsError::RootChanged(
                        rooted.root_path.as_ref().clone(),
                    ));
                }
                cursor = Some(if ordinal == 0 {
                    match &cwd {
                        Some(cwd) => {
                            if !record.effective.same_binding(NativeObservation::from_stat(
                                &stat_fd(cwd.as_raw_fd())?,
                            )?) {
                                return Err(RootedFsError::RootChanged(
                                    rooted.root_path.as_ref().clone(),
                                ));
                            }
                            cwd.try_clone()?
                        }
                        None => next,
                    }
                } else {
                    next
                });
            } else {
                let directory = cursor.as_ref().ok_or_else(invalid)?;
                let current = NativeObservation::from_stat(&stat_at_no_follow(
                    directory.as_raw_fd(),
                    &record.name,
                )?)?;
                // Only a successor may release the prior FILE data identity.
                // The terminal native name and all original DIR/LINK bindings
                // remain authoritative; FILE object/data is captured per run,
                // just as for an ordinary selected FILE (including replacement).
                let refresh_file = data == LeafData::NewRun
                    && target == Some(ordinal)
                    && record.named.kind == libc::S_IFREG as u64;
                let matches = if refresh_file {
                    current.kind == libc::S_IFREG as u64
                } else {
                    record
                        .named
                        .matches(current, record.followed || target == Some(ordinal))
                };
                if !matches {
                    if data == LeafData::CurrentRun
                        && target == Some(ordinal)
                        && record.named.kind == libc::S_IFREG as u64
                        && current.kind == libc::S_IFREG as u64
                    {
                        return Err(self.changed(
                            original_leaf
                                .as_ref()
                                .ok_or_else(invalid)?
                                .name
                                .clone()
                                .into(),
                        ));
                    }
                    return Err(RootedFsError::RootChanged(
                        rooted.root_path.as_ref().clone(),
                    ));
                }
                if let Some(target) = &record.target {
                    if !record.followed
                        || record.named.kind != libc::S_IFLNK as u64
                        || target_at(directory.as_raw_fd(), &record.name)? != *target
                        || !record.named.matches(
                            NativeObservation::from_stat(&stat_at_no_follow(
                                directory.as_raw_fd(),
                                &record.name,
                            )?)?,
                            true,
                        )
                    {
                        return Err(RootedFsError::RootChanged(
                            rooted.root_path.as_ref().clone(),
                        ));
                    }
                } else if target == Some(ordinal) {
                    let original = original_leaf.as_ref().ok_or_else(invalid)?;
                    if record.followed
                        || record.named.kind == libc::S_IFDIR as u64
                        || record.named.kind == libc::S_IFLNK as u64
                        || record.named != record.effective
                        || !original.effective.matches(record.effective, true)
                    {
                        return Err(invalid().into());
                    }
                    // stat_at_no_follow above reached the final target through
                    // every authenticated ORIGINAL native ancestor. Its FILE
                    // observation can seed a new run, never repair this run.
                    // Retain only this temporary qualified parent/name. No
                    // ROOT, mutation or removal authority is created.
                    terminal_data = Some(TerminalBinding {
                        parent: directory.try_clone()?,
                        name: record.name.clone(),
                        object_key: object_key(directory.as_raw_fd(), Some(&record.name))?,
                        stat: stat_at_no_follow(directory.as_raw_fd(), &record.name)?,
                    });
                    if data == LeafData::CurrentRun
                        && terminal_data.as_ref().ok_or_else(invalid)?.object_key
                            != original.followed_object_key
                    {
                        return Err(self.changed(original.name.clone().into()));
                    }
                    if !current.matches(
                        NativeObservation::from_stat(
                            &terminal_data.as_ref().ok_or_else(invalid)?.stat,
                        )?,
                        true,
                    ) {
                        return Err(self.changed(original.name.clone().into()));
                    }
                } else {
                    if record.followed || record.named.kind != libc::S_IFDIR as u64 {
                        return Err(invalid().into());
                    }
                    // The last directory is already genuinely held. Binding
                    // validation needs named metadata, not fresh search/read
                    // permission on that root (e.g. a later mode-000 census).
                    if ordinal == count - 1 {
                        let after = NativeObservation::from_stat(&stat_at_no_follow(
                            directory.as_raw_fd(),
                            &record.name,
                        )?)?;
                        if !record.named.same_binding(after)
                            || !record.effective.same_binding(NativeObservation::from_stat(
                                &stat_fd(rooted.root_fd.as_raw_fd())?,
                            )?)
                        {
                            return Err(RootedFsError::RootChanged(
                                rooted.root_path.as_ref().clone(),
                            ));
                        }
                        if offset != end {
                            return Err(invalid().into());
                        }
                        return Ok(None);
                    }
                    let next = directory_at(directory.as_raw_fd(), &record.name)?;
                    let after = NativeObservation::from_stat(&stat_at_no_follow(
                        directory.as_raw_fd(),
                        &record.name,
                    )?)?;
                    if !record.named.same_binding(after)
                        || !record
                            .effective
                            .same_binding(NativeObservation::from_stat(&stat_fd(
                                next.as_raw_fd(),
                            )?)?)
                    {
                        return Err(RootedFsError::RootChanged(
                            rooted.root_path.as_ref().clone(),
                        ));
                    }
                    cursor = Some(next);
                }
            }
            previous = key;
        }
        if offset != end
            || (target.is_none()
                && !NativeObservation::from_stat(&stat_fd(
                    cursor.as_ref().ok_or_else(invalid)?.as_raw_fd(),
                )?)?
                .same_binding(NativeObservation::from_stat(&stat_fd(
                    rooted.root_fd.as_raw_fd(),
                )?)?))
        {
            return Err(RootedFsError::RootChanged(
                rooted.root_path.as_ref().clone(),
            ));
        }
        Ok(terminal_data)
    }
}

impl CatalogueState {
    fn validate_operator(
        &mut self,
        rooted: &RootedFs,
        data: LeafData,
    ) -> Result<Option<TerminalBinding>> {
        self.active()?.begin()?;
        let observed = self.replay_operator(rooted, data)?;
        self.active()?.healthy = true;
        Ok(observed)
    }

    /// The inventory batch owns the locked, unhealthy catalogue until both
    /// original-address barriers and all native bindings have succeeded.
    pub(super) fn validate_original_operator(&mut self, rooted: &RootedFs) -> Result<()> {
        self.replay_operator(rooted, LeafData::CurrentRun)
            .map(|_| ())
    }

    fn replay_operator(
        &mut self,
        rooted: &RootedFs,
        data: LeafData,
    ) -> Result<Option<TerminalBinding>> {
        self.active()?.replay_address(rooted, data)
    }
}

impl Catalogue {
    fn operator_changed(&mut self, rooted: &RootedFs) -> RootedFsError {
        let path = rooted.root_path.as_ref().clone();
        let Ok(mut lifetime) = self.lifetime.lock() else {
            return RootedFsError::SourceCatalogueTerminal;
        };
        *lifetime = Some(path.clone());
        RootedFsError::RootChanged(path)
    }

    /// `inspect_selected` observes only the original named LINK, never the
    /// effective FILE's mutable DATA. Its failures cannot authorize CHILD retry.
    pub(super) fn original_link_result(
        &mut self,
        rooted: &RootedFs,
        path: &Path,
        result: Result<Observation>,
    ) -> Result<Observation> {
        let observed = match result {
            Err(RootedFsError::SourceBindingChanged(_)) => {
                return Err(self.operator_changed(rooted));
            }
            result => result?,
        };
        let original = self.require(path)?;
        if !original.named.matches(observed.named, true) || original.target != observed.target {
            return Err(self.operator_changed(rooted));
        }
        Ok(observed)
    }

    fn replay_address(
        &mut self,
        rooted: &RootedFs,
        data: LeafData,
    ) -> Result<Option<TerminalBinding>> {
        // Failed native lookup is a root-change error, not permission to retry
        // acquisition. Scratch/authentication errors keep their typed cause.
        match self.validate_operator(rooted, data) {
            Err(RootedFsError::RootChanged(_)) => Err(self.operator_changed(rooted)),
            Err(RootedFsError::Io(error))
                if matches!(
                    error.raw_os_error(),
                    Some(libc::ENOENT | libc::ENOTDIR | libc::ELOOP)
                ) =>
            {
                Err(self.operator_changed(rooted))
            }
            Err(error) => Err(error),
            Ok(observed) => Ok(observed),
        }
    }
}

#[cfg(test)]
#[path = "operator_tests.rs"]
mod tests;

impl RootedFs {
    /// Read-only operations use the authenticated terminal cursor, never a
    /// lexical reopen. The target parent/name cannot authorize selected unlink.
    fn with_source_follow_binding<T>(
        &self,
        path: &RelativePath,
        follow: bool,
        read: impl FnOnce(RawFd, &OsStr, &libc::stat) -> Result<T>,
    ) -> Result<T> {
        self.verify_source_root_path_blocking()?;
        let mut owner = self
            .source_observations
            .lock()
            .map_err(|_| RootedFsError::SourceCatalogueTerminal)?;
        if matches!(*owner, CatalogueState::Unstarted) {
            return Err(RootedFsError::SourceFollowUnsupported(
                path.as_path().to_path_buf(),
            ));
        }
        let c = owner.active()?;
        c.begin()?;
        let original = c.require(path.as_path())?;
        if follow && original.named.kind == libc::S_IFLNK as u64 && !original.followed {
            return Err(RootedFsError::SourceFollowUnsupported(
                path.as_path().to_path_buf(),
            ));
        }
        let result = c.validate_native_binding(self, path.as_path(), true);
        let binding = c
            .native_result(result)?
            .ok_or(RootedFsError::InvalidRelativePath)?;
        #[cfg(test)]
        self.pause_mutation(PublicationPausePoint::AfterSourceBinding);
        if !original
            .effective
            .matches(NativeObservation::from_stat(&binding.stat)?, true)
        {
            return Err(c.changed(path.as_path().to_path_buf()));
        }
        #[cfg(test)]
        self.pause_mutation(PublicationPausePoint::AfterSourceValidation);
        let result = read(binding.parent.as_raw_fd(), &binding.name, &binding.stat);
        let value = c.native_result(result)?;
        let result = c.validate_native_binding(self, path.as_path(), true);
        c.native_result(result)?;
        if self.source_path_continuity {
            c.replay_address(self, LeafData::CurrentRun)?;
        }
        c.healthy = true;
        Ok(value)
    }

    pub(crate) fn source_following_identity_blocking(
        &self,
        path: &RelativePath,
    ) -> Result<Option<EntryIdentity>> {
        self.with_source_follow_binding(path, true, |_, _, stat| Ok(identity_from_stat(stat)))
    }

    pub(crate) fn source_metadata_following_blocking(
        &self,
        path: &RelativePath,
    ) -> Result<crate::endpoint::FileMetadata> {
        self.source_scan_metadata_blocking(path, true)
    }

    pub(crate) fn source_scan_metadata_blocking(
        &self,
        path: &RelativePath,
        follow: bool,
    ) -> Result<crate::endpoint::FileMetadata> {
        self.with_source_follow_binding(path, follow, |_, _, stat| Self::metadata_from_stat(stat))
    }

    pub(crate) fn open_source_following_blocking(&self, path: &RelativePath) -> Result<File> {
        self.with_source_follow_binding(path, true, |parent, name, stat| {
            if stat.st_mode & libc::S_IFMT != libc::S_IFREG {
                return Err(RootedFsError::NotRegularFile(path.as_path().to_path_buf()));
            }
            let file = open_file_at(parent, name)
                .map_err(|error| source_read_race(error, path.as_path()))?;
            let opened = NativeObservation::from_stat(&stat_fd(file.as_raw_fd())?)?;
            let named = NativeObservation::from_stat(
                &stat_at_no_follow(parent, name)
                    .map_err(|error| source_read_race(error, path.as_path()))?,
            )?;
            let expected = NativeObservation::from_stat(stat)?;
            if !expected.matches(opened, true) || !expected.matches(named, true) {
                return Err(RootedFsError::SourceBindingChanged(
                    path.as_path().to_path_buf(),
                ));
            }
            Ok(file)
        })
    }

    pub(crate) fn source_entry_metadata_blocking(
        &self,
        path: &RelativePath,
        request: crate::engine::scan::ScanRequest,
    ) -> Result<crate::engine::domain::Entry> {
        self.with_source_follow_binding(path, request.follow_symlinks, |parent, name, stat| {
            use super::super::scan::RootedScanError;
            super::super::scan::source_metadata_entry(parent, name, path, request, *stat).map_err(
                |error| match error {
                    RootedScanError::EntryChanged(_) | RootedScanError::UnstableSymlink(_) => {
                        RootedFsError::SourceBindingChanged(path.as_path().to_path_buf())
                    }
                    RootedScanError::Metadata { source, .. }
                        if matches!(
                            source.raw_os_error(),
                            Some(libc::ENOENT | libc::ENOTDIR | libc::ELOOP)
                        ) =>
                    {
                        RootedFsError::SourceBindingChanged(path.as_path().to_path_buf())
                    }
                    error => io::Error::other(error).into(),
                },
            )
        })
    }

    /// A new run may observe fresh FILE data and tree children, but never
    /// reacquire operator ancestry or the original selected LINK. Authenticate
    /// and copy original ROOT/DIR/LINK records through the same held descriptor.
    pub(crate) fn next_source_run_blocking(&self) -> Result<Self> {
        self.source_successor_blocking(false)
    }

    pub(crate) fn retry_source_run_blocking(&self) -> Result<Self> {
        let result = self.source_successor_blocking(true);
        if result.is_err() {
            // Failed authentication/native replay is terminal even if someone
            // later restores scratch bytes or operator spellings.
            if let Ok(mut owner) = self.source_observations.lock() {
                if let CatalogueState::Active(catalogue) = &mut *owner {
                    catalogue.run = RunState::Terminal;
                }
            }
        }
        result
    }

    fn source_successor_blocking(&self, retry: bool) -> Result<Self> {
        let mut owner = self
            .source_observations
            .lock()
            .map_err(|_| RootedFsError::SourceCatalogueTerminal)?;
        let original = if retry {
            let CatalogueState::Active(original) = &mut *owner else {
                return Err(RootedFsError::SourceCatalogueTerminal);
            };
            original.check_lifetime()?;
            if original.cancelled.load(Ordering::Acquire)
                || original.run != RunState::SourceRace
                || original.scanning
            {
                return Err(RootedFsError::SourceCatalogueTerminal);
            }
            original
        } else {
            owner.active()?
        };
        if original.scanning {
            original.begin()?;
            return Err(RootedFsError::SourceCatalogueTerminal);
        }
        let data = if retry || original.completed {
            LeafData::NewRun
        } else {
            LeafData::CurrentRun
        };
        // Only this successor transition releases FILE data evidence. Normal
        // begin rejects failure; retry requires a classified terminal race and
        // authenticated provenance, never reviving the failed run.
        let terminal_data = if retry {
            // Authenticate the full retained catalogue without opening CHILD
            // names or repairing scratch. No failed-run observations go live.
            if original.file.metadata()?.len() != original.end {
                return Err(invalid().into());
            }
            for ordinal in 0..original.count {
                let (key, _) = original.original_at(ordinal)?;
                original.original_path(key)?;
                let mut expected = [0; 48];
                expected[..8].copy_from_slice(&ordinal.to_le_bytes());
                if original
                    .index
                    .get_bytes(inventory::state_key(b"ordinal", key))?
                    != Some(expected)
                {
                    return Err(invalid().into());
                }
                original.disposition(key)?;
                original.child_count(key)?;
            }
            original.replay_address(self, data)?
        } else {
            owner.validate_operator(self, data)?
        };
        let CatalogueState::Active(original) = &mut *owner else {
            return Err(RootedFsError::SourceCatalogueTerminal);
        };
        if !retry {
            original.begin()?;
        }
        if !retry && !original.completed {
            original.healthy = true;
            return Ok(self.clone());
        }
        let address = original
            .operator
            .as_ref()
            .ok_or(RootedFsError::SourceOperatorUncaptured)?;
        let cwd = address.cwd.clone();
        let count = address.count;
        let root_ordinal = address.root;
        let leaf = address.followed_leaf;
        let root = original.require(Path::new(""))?;
        let original_leaf = leaf
            .map(|leaf| -> Result<_> {
                let mut record = original.get(leaf.observation)?.ok_or_else(invalid)?;
                let terminal = terminal_data.as_ref().ok_or_else(invalid)?;
                record.effective = NativeObservation::from_stat(&terminal.stat)?;
                record.followed_object_key = terminal.object_key;
                Ok(record)
            })
            .transpose()?;
        let guard = File::from(self.root_fd.try_clone()?);
        crate::engine::scan_sort::validate_scratch_root(&guard, &tempfile::env::temp_dir())?;
        drop(guard);
        let mut next = Catalogue::empty()?;
        next.lifetime = original.lifetime.clone();
        next.insert([0; 32], &root)?;
        let start = next.end;
        for ordinal in 0..count {
            let key = record_key(ordinal);
            let mut record = original.get(key)?.ok_or_else(invalid)?;
            if leaf.is_some_and(|leaf| leaf.terminal == ordinal) {
                let observed = NativeObservation::from_stat(
                    &terminal_data.as_ref().ok_or_else(invalid)?.stat,
                )?;
                record.named = observed;
                record.effective = observed;
            }
            next.append(key, &record)?;
        }
        next.operator = Some(OriginalAddress {
            cwd,
            count,
            start,
            end: next.end,
            root: root_ordinal,
            followed_leaf: leaf,
        });
        if let Some(original_leaf) = original_leaf {
            next.insert(leaf.ok_or_else(invalid)?.observation, &original_leaf)?;
        }
        next.begin()?;
        next.replay_address(self, LeafData::CurrentRun)?;
        next.healthy = true;
        if !retry {
            original.healthy = true;
        }
        let mut rooted = self.clone();
        rooted.source_observations = Arc::new(std::sync::Mutex::new(CatalogueState::Active(
            Box::new(next),
        )));
        Ok(rooted)
    }

    pub(in crate::rooted_fs) fn validate_operator_address_blocking(&self) -> Result<()> {
        let mut owner = self
            .source_observations
            .lock()
            .map_err(|_| RootedFsError::SourceCatalogueTerminal)?;
        owner
            .validate_operator(self, LeafData::CurrentRun)
            .map(|_| ())
    }
}
