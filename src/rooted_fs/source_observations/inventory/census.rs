//! Exact late membership, not a new scan/adoption or a subtree snapshot.
//! One directory cursor and bounded ancestry descriptors; exact visited ordinals
//! use a bounded bitmap or spill into an authenticated radix.
use super::*;
use crate::fs_util::directory_stream::DirectoryStream;
mod seen;
#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
mod tests;
use crate::rooted_fs::local_source_effects::preflight::Declarations;
use seen::SeenOriginals;

enum Candidates<'a> {
    Immediate(&'a [NativeObjectKey], SourceExclusion),
    Declared(&'a mut Declarations),
}

impl Candidates<'_> {
    fn independent(&mut self, source: NativeObjectKey) -> Result<bool> {
        match self {
            Self::Immediate(objects, _) => {
                Ok(objects.iter().all(|object| source.independent(*object)))
            }
            Self::Declared(declarations) => declarations.independent(source),
        }
    }

    fn excludes_additional(
        &mut self,
        catalogue: &mut Catalogue,
        rooted: &RootedFs,
    ) -> Result<bool> {
        match self {
            Self::Immediate(_, exclusion) => catalogue.excludes_additional(rooted, *exclusion),
            Self::Declared(declarations) => {
                for ordinal in 0..declarations.count {
                    catalogue.check_run()?;
                    if !catalogue.excludes_additional(rooted, declarations.exclusion(ordinal)?)? {
                        declarations.refused = Some(ordinal);
                        return Ok(false);
                    }
                }
                Ok(true)
            }
        }
    }
}

impl Catalogue {
    pub(super) fn open_original_directory(&mut self, root: RawFd, path: &Path) -> Result<OwnedFd> {
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
                &inspect(
                    ResolutionRoot::Held(root),
                    directory.as_raw_fd(),
                    &relative,
                    false,
                )?,
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
            let step = match step {
                ResolutionStep::Root(reset) => {
                    let root = self.require(Path::new(""))?;
                    if !root.named.matches(reset.named, true)
                        || root.object_key != Some(reset.object_key)
                    {
                        return Ok(false);
                    }
                    parent = [0; 32];
                    continue;
                }
                ResolutionStep::Entry(step) => step,
            };
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
                self.original_path(parent, &member)?;
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
            self.original_path(member_key, &member)?;
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

    fn excludes_additional(
        &mut self,
        rooted: &RootedFs,
        exclusion: SourceExclusion,
    ) -> Result<bool> {
        match exclusion {
            SourceExclusion::Objects => Ok(true),
            SourceExclusion::OperatorLinks(candidate) => {
                self.excludes_operator_links(rooted, candidate)
            }
            SourceExclusion::DirectoryAncestors(candidate) => {
                // Object absence cannot prove access-policy independence from
                // an ancestor. Selected anchors also have possible outside DATA.
                if self.scope != Some(SourceRootProtection::Tree)
                    || !self.excludes_operator_ancestors(rooted, candidate)?
                {
                    return Ok(false);
                }
                let root = self.require(Path::new(""))?;
                let Some(expected) = root.object_key else {
                    return Ok(false);
                };
                crate::rooted_fs::local_source_effects::ancestry::excludes(
                    rooted.root_fd.as_raw_fd(),
                    expected,
                    candidate,
                )
            }
        }
    }
}

impl SourceInventory {
    /// Fresh exact native membership and the requested namespace exclusion,
    /// within one original-address/ROOT batch and one qualified APFS view.
    /// No DATA reads, outside-root candidate mount opens, or recursive retirement
    /// authority. Object absence alone never proves access-policy independence.
    pub(in crate::rooted_fs) fn excludes_objects_blocking(
        &self,
        candidates: &[NativeObjectKey],
        exclusion: SourceExclusion,
    ) -> Result<bool> {
        match exclusion {
            SourceExclusion::OperatorLinks(object)
            | SourceExclusion::DirectoryAncestors(object)
                if !candidates.contains(&object) =>
            {
                return Err(invalid().into())
            }
            _ => {}
        }
        self.excludes_candidates_blocking(Candidates::Immediate(candidates, exclusion))
    }

    pub(in crate::rooted_fs) fn excludes_declarations_blocking(
        &self,
        declarations: &mut Declarations,
    ) -> Result<bool> {
        self.excludes_candidates_blocking(Candidates::Declared(declarations))
    }

    fn excludes_candidates_blocking(&self, mut candidates: Candidates<'_>) -> Result<bool> {
        let selected = self.selected_leaf_keys_blocking()?;
        self.with_original_batch(|c| {
            if let Some(selected) = selected {
                c.validate_original_records(&self.rooted)?;
                let mut qualified = true;
                for source in selected {
                    qualified &= candidates.independent(source)?;
                }
                return Ok(qualified && candidates.excludes_additional(c, &self.rooted)?);
            }
            if c.scope != Some(SourceRootProtection::Tree) {
                c.validate_original_records(&self.rooted)?;
                return Ok(false);
            }
            let (_, root, _) = c.original_at(0)?;
            let Some(view) = root.object_key else {
                c.validate_original_records(&self.rooted)?;
                return Ok(false);
            };
            // Qualify ALL originals before any census. A followed FILE chain
            // must agree with every independently enumerated physical member;
            // discovery is not enumeration or closure of an opaque subtree.
            let mut qualified = true;
            for ordinal in 0..c.count {
                let (original_key, original, state) = c.original_at(ordinal)?;
                c.original_path(original_key, &original)?;
                let physical = if original.followed {
                    c.qualifies_follow_resolution(&original)?
                        && match original.followed_object_key {
                            Some(object) => {
                                view.0[..24] == object.0[..24] && candidates.independent(object)?
                            }
                            None => false,
                        }
                } else {
                    original.named == original.effective
                };
                qualified &= physical
                    && match original.object_key {
                        Some(object) => {
                            view.0[..24] == object.0[..24] && candidates.independent(object)?
                        }
                        None => false,
                    };
                if original.named.kind == libc::S_IFDIR as u64 {
                    qualified &= state.directory != SourceDirectoryDisposition::Opaque
                        || (state.children == 0
                            && c.empty_directory_stamp(original_key)?.is_some());
                }
            }
            if !qualified {
                c.validate_original_records(&self.rooted)?;
                return Ok(false);
            }

            let mut seen = SeenOriginals::new(c.count)?;
            let mut total = 0u64;
            for ordinal in 0..c.count {
                let (parent_key, parent, state) = c.original_at(ordinal)?;
                if parent.named.kind != libc::S_IFDIR as u64 {
                    continue;
                }
                let path = c.original_path(parent_key, &parent)?;
                let expected = state.children;
                if state.directory == SourceDirectoryDisposition::Opaque {
                    // Native attributes do not require opening the omitted
                    // directory for search/read access, or listing its names.
                    let directory = match path.parent() {
                        Some(parent) => {
                            c.open_original_directory(self.rooted.root_fd.as_raw_fd(), parent)?
                        }
                        None => self.rooted.root_fd.try_clone()?,
                    };
                    c.check_empty_directory(
                        parent_key,
                        directory.as_raw_fd(),
                        path.file_name(),
                        &path,
                    )?;
                    continue;
                }
                let directory =
                    c.open_original_directory(self.rooted.root_fd.as_raw_fd(), &path)?;
                let mut directory = DirectoryStream::from_directory(directory)?;
                let mut count = 0u64;
                while let Some(name) = directory.next() {
                    let name = name?;
                    let child_path = path.join(&name);
                    validate_original_path(&child_path)?;
                    let child_key = key(parent_key, &name);
                    // The canonical entry freshly proves the original ordinal;
                    // its candidate must point back to this independently known key.
                    let Some((original, state)) = c.original_named(child_key)? else {
                        return Err(RootedFsError::SourceBindingChanged(child_path));
                    };
                    let ordinal = state.ordinal;
                    if original.parent != parent_key || original.name != name {
                        return Err(RootedFsError::SourceBindingChanged(child_path));
                    }
                    if !seen.insert(child_key, ordinal)? {
                        return Err(RootedFsError::SourceBindingChanged(child_path));
                    }
                    let current = inspect(
                        ResolutionRoot::Catalogued {
                            rooted: &self.rooted,
                            catalogue: c,
                        },
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
            #[cfg(test)]
            self.rooted
                .pause_mutation(PublicationPausePoint::AfterSourceCensus);
            // Exact exhaustion of every ORIGINAL, not merely an enumeration count.
            // A held cursor must not hide a directory moved out during census:
            // close its original parent/name binding after all enumeration too.
            for ordinal in 1..c.count {
                let (key, original, _) = c.original_at(ordinal)?;
                if !seen.contains(key, ordinal)? {
                    return Err(invalid().into());
                }
                if original.named.kind == libc::S_IFDIR as u64 {
                    let path = c.original_path(key, &original)?;
                    c.validate_native_binding(&self.rooted, &path, true)?;
                }
            }
            candidates.excludes_additional(c, &self.rooted)
        })
    }
}
