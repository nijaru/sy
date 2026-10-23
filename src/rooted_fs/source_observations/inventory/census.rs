//! Exact late membership, not a new scan/adoption or a subtree snapshot.
//! One directory cursor and bounded ancestry descriptors; visited names spill
//! into an authenticated radix so duplicate native records cannot hide removals.
use super::*;
use crate::fs_util::directory_stream::DirectoryStream;

impl Catalogue {
    fn open_original_directory(&mut self, root: RawFd, path: &Path) -> Result<OwnedFd> {
        let original = self.require(Path::new(""))?;
        let mut directory = open_dir_at(root, OsStr::new("."))?;
        self.check_directory(&original, directory.as_raw_fd(), Path::new(""))?;
        let mut relative = PathBuf::new();
        for component in path.components() {
            let Component::Normal(name) = component else {
                return Err(RootedFsError::InvalidRelativePath);
            };
            relative.push(name);
            // Authenticate the original before interpreting the name or opening.
            let original = self.require(&relative)?;
            if original.followed || original.named.kind != libc::S_IFDIR as u64 {
                return Err(invalid().into());
            }
            if !original.matches(
                &inspect(root, directory.as_raw_fd(), &relative, false)?,
                true,
            ) {
                return Err(RootedFsError::SourceBindingChanged(relative));
            }
            directory = open_dir_at(directory.as_raw_fd(), name)?;
            self.check_directory(&original, directory.as_raw_fd(), &relative)?;
        }
        Ok(directory)
    }

    /// Discovery records never stand in for independently enumerated members.
    /// Every resolution DIR/LINK/FILE must match a physical original, including
    /// observations independently captured after target discovery.
    fn qualifies_follow_resolution(&mut self, original: &Observation) -> Result<bool> {
        if original.named.kind != libc::S_IFLNK as u64
            || original.effective.kind != libc::S_IFREG as u64
            || original.resolution.is_empty()
        {
            return Ok(false);
        }
        let mut parent = [0; 32];
        for (index, step) in original.resolution.iter().enumerate() {
            if step.name == OsStr::new(".") || step.name == OsStr::new("..") {
                // Native parent results must corroborate the original graph;
                // neither dot creates membership, traversal EOF, or a ROOT lease.
                if step.name == OsStr::new("..") {
                    if parent == [0; 32] {
                        return Err(invalid().into());
                    }
                    let current = self.get(parent)?.ok_or_else(invalid)?;
                    if current.followed || current.named.kind != libc::S_IFDIR as u64 {
                        return Ok(false);
                    }
                    parent = current.parent;
                }
                let member = self.get(parent)?.ok_or_else(invalid)?;
                self.original_path(parent)?;
                if step.named.kind != libc::S_IFDIR as u64
                    || member.followed
                    || member.named != member.effective
                    || !member.named.matches(step.named, true)
                    || member.object_key != step.object_key
                    || step.target.is_some()
                {
                    return Ok(false);
                }
                continue;
            }
            let member_key = key(parent, &step.name);
            let Some(member) = self.get(member_key)? else {
                return Ok(false);
            };
            self.original_path(member_key)?;
            if member.parent != parent
                || member.name != step.name
                || !member.named.matches(step.named, true)
                || member.object_key != step.object_key
                || member.target != step.target
            {
                return Ok(false);
            }
            if step.named.kind == libc::S_IFLNK as u64 {
                if !member.followed
                    || member.effective != original.effective
                    || member.followed_object_key != original.followed_object_key
                    || !same_resolution(
                        &member.resolution[member.resolution_start..],
                        &original.resolution[index + 1..],
                    )
                {
                    return Ok(false);
                }
            } else {
                if member.followed || member.named != member.effective {
                    return Ok(false);
                }
                if step.named.kind == libc::S_IFDIR as u64 {
                    parent = member_key;
                } else if member.named != original.effective
                    || member.object_key != original.followed_object_key
                {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }

    fn check_directory(&self, original: &Observation, fd: RawFd, path: &Path) -> Result<()> {
        if !original
            .named
            .matches(NativeObservation::from_stat(&stat_fd(fd)?)?, true)
            || original.object_key != object_key(fd, None)?
        {
            return Err(RootedFsError::SourceBindingChanged(path.to_path_buf()));
        }
        Ok(())
    }
}

impl SourceInventory {
    /// No DATA reads, no outside-root candidate mount opens. Object absence is
    /// meaningful only in this complete original, single native APFS view.
    /// It certifies object absence, not directory retirement or independence
    /// from an ancestor's access/inheritance policy.
    pub(in crate::rooted_fs) fn excludes_objects_blocking(
        &self,
        candidates: &[NativeObjectKey],
    ) -> Result<bool> {
        if let Some(selected) = self.selected_leaf_keys_blocking()? {
            return Ok(selected.iter().all(|source| {
                candidates
                    .iter()
                    .all(|candidate| source.independent(*candidate))
            }));
        }
        self.rooted.verify_source_root_path_blocking()?;
        let mut owner = self
            .rooted
            .source_observations
            .lock()
            .map_err(|_| RootedFsError::SourceCatalogueTerminal)?;
        let c = owner.active()?;
        c.begin()?;
        let result = (|| {
            if !c.sealed || c.scanning || c.count != self.count {
                return Err(RootedFsError::SourceInventoryIncomplete);
            }
            if c.scope != Some(SourceRootProtection::Tree) {
                c.healthy = true;
                return Ok(false);
            }
            let (_, root) = c.original_at(0)?;
            let Some(view) = root.object_key else {
                c.healthy = true;
                return Ok(false);
            };
            // Qualify ALL originals before any census. A followed FILE chain
            // must agree with every independently enumerated physical member;
            // discovery is not enumeration or closure of an opaque subtree.
            let mut qualified = true;
            for ordinal in 0..c.count {
                let (original_key, original) = c.original_at(ordinal)?;
                c.original_path(original_key)?;
                let physical = if original.followed {
                    c.qualifies_follow_resolution(&original)?
                        && original.followed_object_key.is_some_and(|object| {
                            view.0[..24] == object.0[..24]
                                && candidates
                                    .iter()
                                    .all(|candidate| object.independent(*candidate))
                        })
                } else {
                    original.named == original.effective
                };
                qualified &= physical
                    && original.object_key.is_some_and(|object| {
                        view.0[..24] == object.0[..24]
                            && candidates
                                .iter()
                                .all(|candidate| object.independent(*candidate))
                    });
                if original.named.kind == libc::S_IFDIR as u64 {
                    qualified &=
                        c.disposition(original_key)? == SourceDirectoryDisposition::Traversed;
                    c.child_count(original_key)?;
                }
            }
            if !qualified {
                c.healthy = true;
                return Ok(false);
            }

            let mut seen = AuthenticatedIndex::<8>::new()?;
            let mut total = 0u64;
            for ordinal in 0..c.count {
                let (parent_key, parent) = c.original_at(ordinal)?;
                if parent.named.kind != libc::S_IFDIR as u64 {
                    continue;
                }
                let path = c.original_path(parent_key)?;
                let expected = c.child_count(parent_key)?;
                let directory =
                    c.open_original_directory(self.rooted.root_fd.as_raw_fd(), &path)?;
                let mut directory = DirectoryStream::from_directory(directory)?;
                let mut count = 0u64;
                while let Some(name) = directory.next() {
                    let name = name?;
                    let child_path = path.join(&name);
                    validate_original_path(&child_path)?;
                    let child_key = key(parent_key, &name);
                    let original = c
                        .get(child_key)?
                        .ok_or_else(|| RootedFsError::SourceBindingChanged(child_path.clone()))?;
                    if original.parent != parent_key
                        || original.name != name
                        || seen.get_bytes(child_key)?.is_some()
                    {
                        return Err(RootedFsError::SourceBindingChanged(child_path));
                    }
                    // Authenticate ordinal membership too; a parent/name lookup
                    // cannot introduce a record outside the original exact set.
                    let position = c
                        .index
                        .get_bytes(state_key(b"ordinal", child_key))?
                        .ok_or_else(invalid)?;
                    if position[8..] != [0; 40] {
                        return Err(invalid().into());
                    }
                    let ordinal =
                        u64::from_le_bytes(position[..8].try_into().map_err(|_| invalid())?);
                    if c.original_at(ordinal)?.0 != child_key {
                        return Err(invalid().into());
                    }
                    let current = inspect(
                        self.rooted.root_fd.as_raw_fd(),
                        directory.as_raw_fd(),
                        &child_path,
                        original.followed,
                    )
                    .map_err(|error| match error {
                        RootedFsError::SourceFollowUnsupported(_) if original.followed => {
                            RootedFsError::SourceBindingChanged(child_path.clone())
                        }
                        error => error,
                    })?;
                    if !original.matches(&current, true) {
                        return Err(RootedFsError::SourceBindingChanged(child_path));
                    }
                    seen.set_bytes(child_key, ordinal.to_le_bytes())?;
                    count = count.checked_add(1).ok_or_else(invalid)?;
                    if count > expected {
                        return Err(RootedFsError::SourceBindingChanged(path));
                    }
                }
                if count != expected {
                    return Err(RootedFsError::SourceBindingChanged(path));
                }
                c.check_directory(&parent, directory.as_raw_fd(), &path)?;
                total = total.checked_add(count).ok_or_else(invalid)?;
            }
            if total != c.count - 1 {
                return Err(invalid().into());
            }
            // Exact exhaustion of every ORIGINAL, not merely an enumeration count.
            for ordinal in 1..c.count {
                let (key, _) = c.original_at(ordinal)?;
                if seen.get_bytes(key)? != Some(ordinal.to_le_bytes()) {
                    return Err(invalid().into());
                }
            }
            c.healthy = true;
            Ok(true)
        })();
        c.native_result(result)
    }
}
