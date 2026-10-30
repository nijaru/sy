//! Local original protection evidence, not byte, mutation or completion authority.
//! The ordinal mapping shares the catalogue's authenticated index; no second
//! observation owner or per-entry resident state is introduced.
use super::*;
mod census;
mod empty;
mod protection;
mod state;
pub(super) use state::OriginalState;
#[cfg(test)]
#[path = "inventory_tests.rs"]
mod tests;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SourceRootProtection {
    Tree,
    SelectedLeafAnchor,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SourceDirectoryDisposition {
    Traversed,
    /// Metadata-only census reached EOF; these entries are not sync candidates.
    Protected,
    Opaque,
}

/// Additional namespace footprint; its object must also occur in the census
/// candidates. These are requests for proof, never reusable effect permits.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::rooted_fs) enum SourceExclusion {
    Objects,
    OperatorLinks(NativeObjectKey),
    DirectoryAncestors(NativeObjectKey),
}

#[derive(Clone)]
pub(crate) struct SourceInventory {
    rooted: RootedFs,
    count: u64,
}

pub(super) fn ordinal_key(ordinal: u64) -> [u8; 32] {
    let mut hash = blake3::Hasher::new();
    hash.update(b"sy/source-observation/ordinal/v1\0");
    hash.update(&ordinal.to_le_bytes());
    *hash.finalize().as_bytes()
}
pub(super) fn state_key(domain: &[u8], key: [u8; 32]) -> [u8; 32] {
    let mut hash = blake3::Hasher::new();
    hash.update(b"sy/source-observation/state/v1\0");
    hash.update(domain);
    hash.update(&key);
    *hash.finalize().as_bytes()
}

pub(super) fn validate_original_path(path: &Path) -> Result<()> {
    let mut bytes = 2usize;
    let mut count = 0usize;
    for component in path.components() {
        let Component::Normal(name) = component else {
            return Err(RootedFsError::InvalidRelativePath);
        };
        bytes = bytes
            .checked_add(2 + name.as_bytes().len())
            .ok_or_else(invalid)?;
        count += 1;
        if bytes > crate::protocol::MAX_WIRE_PATH_BYTES
            || count > crate::protocol::MAX_WIRE_COMPONENTS
        {
            return Err(invalid().into());
        }
    }
    if count == 0 {
        return Err(RootedFsError::InvalidRelativePath);
    }
    Ok(())
}

impl Catalogue {
    pub(super) fn check_completed_traversal(&mut self) -> Result<()> {
        for ordinal in 0..self.count {
            let (key, original, state) = self.original_at(ordinal)?;
            if original.effective.kind == libc::S_IFDIR as u64
                && state.directory == SourceDirectoryDisposition::Traversed
                && self.epoch_at(state_key(b"traversal", key))? != Some(self.epoch)
            {
                return Err(invalid().into());
            }
        }
        Ok(())
    }
    fn original_key(&mut self, ordinal: u64) -> Result<[u8; 32]> {
        if ordinal >= self.count {
            return Err(invalid().into());
        }
        match self.index_record(ordinal_key(ordinal))? {
            Some(IndexRecord::Ordinal(key)) => Ok(key),
            _ => Err(invalid().into()),
        }
    }

    fn decode_original(
        &self,
        key: [u8; 32],
        ordinal: u64,
        entry: CatalogueEntry,
    ) -> Result<([u8; 32], Observation, OriginalState)> {
        let state = entry.original.ok_or_else(invalid)?;
        if state.ordinal != ordinal {
            return Err(invalid().into());
        }
        let observation = self.read_locator(entry.locator)?;
        let actual = if ordinal == 0 {
            [0; 32]
        } else {
            super::key(observation.parent, &observation.name)
        };
        if actual != key {
            return Err(invalid().into());
        }
        Ok((key, observation, state))
    }

    pub(super) fn original_at(
        &mut self,
        ordinal: u64,
    ) -> Result<([u8; 32], Observation, OriginalState)> {
        let key = self.original_key(ordinal)?;
        let entry = self.entry(key)?.ok_or_else(invalid)?;
        self.decode_original(key, ordinal, entry)
    }

    // In a native census the name is known first. Authenticate its canonical
    // entry once, then the inverse ordinal association, before decoding names.
    // The receipt lives only in this call; no observation/proof is cached.
    fn original_named(&mut self, key: [u8; 32]) -> Result<Option<(Observation, OriginalState)>> {
        let Some(entry) = self.entry(key)? else {
            return Ok(None);
        };
        let state = entry.original.ok_or_else(invalid)?;
        if self.original_key(state.ordinal)? != key {
            return Err(invalid().into());
        }
        let (_, observation, state) = self.decode_original(key, state.ordinal, entry)?;
        Ok(Some((observation, state)))
    }
    /// The caller has just authenticated the first record. Replay its parents
    /// freshly, without loading and decoding that same record a second time.
    pub(super) fn original_path(
        &mut self,
        mut key: [u8; 32],
        original: &Observation,
    ) -> Result<PathBuf> {
        let mut names = Vec::new();
        let mut bytes = 2usize;
        let mut parent = original.parent;
        let mut name = std::borrow::Cow::Borrowed(original.name.as_os_str());
        while key != [0; 32] {
            if name.is_empty()
                || name.as_bytes().contains(&0)
                || name.as_bytes().contains(&b'/')
                || name == OsStr::new(".")
                || name == OsStr::new("..")
                || super::key(parent, &name) != key
            {
                return Err(invalid().into());
            }
            bytes = bytes
                .checked_add(2 + name.as_bytes().len())
                .ok_or_else(invalid)?;
            if bytes > crate::protocol::MAX_WIRE_PATH_BYTES
                || names.len() >= crate::protocol::MAX_WIRE_COMPONENTS
            {
                return Err(invalid().into());
            }
            names.push(name);
            key = parent;
            let ancestor = self.get(key)?.ok_or_else(invalid)?;
            parent = ancestor.parent;
            name = std::borrow::Cow::Owned(ancestor.name);
        }
        if parent != [0; 32] || !name.is_empty() {
            return Err(invalid().into());
        }
        Ok(names.into_iter().rev().collect())
    }

    fn check_inventory_root(&mut self, rooted: &RootedFs) -> Result<()> {
        let (_, root, _) = self.original_at(0)?;
        if !root
            .effective
            .same_binding(NativeObservation::from_stat(&stat_fd(
                rooted.root_fd.as_raw_fd(),
            )?)?)
        {
            return Err(RootedFsError::RootChanged(
                rooted.root_path.as_ref().clone(),
            ));
        }
        Ok(())
    }

    pub(super) fn check_original_ordinal(
        &mut self,
        ordinal: u64,
        key: [u8; 32],
    ) -> Result<OriginalState> {
        let state = self.original_state(key)?;
        if state.ordinal != ordinal {
            return Err(invalid().into());
        }
        Ok(state)
    }

    fn validate_original_records(&mut self, rooted: &RootedFs) -> Result<()> {
        for ordinal in 0..self.count {
            let (key, original, _) = self.original_at(ordinal)?;
            let path = self.original_path(key, &original)?;
            let result = self.validate_native_binding(rooted, &path, true);
            self.native_result(result)?;
            #[cfg(test)]
            rooted.pause_mutation(PublicationPausePoint::AfterSourceValidation);
        }
        Ok(())
    }
}

impl RootedFs {
    /// Call only after draining/joining the successful owned SOURCE producer,
    /// and before admitting any destination effects. Sealing never repairs data.
    pub(crate) fn seal_source_inventory_blocking(&self) -> Result<SourceInventory> {
        self.verify_source_root_path_blocking()?;
        let mut owner = self
            .source_observations
            .lock()
            .map_err(|_| RootedFsError::SourceCatalogueTerminal)?;
        let catalogue = owner.active()?;
        catalogue.begin()?;
        if catalogue.scanning || !catalogue.completed {
            catalogue.healthy = true;
            return Err(RootedFsError::SourceInventoryIncomplete);
        }
        // Authenticate the exact original set before making a sealed view.
        // This is bounded replay, not checksum adoption from scratch.
        for ordinal in 0..catalogue.count {
            let (key, original, _) = catalogue.original_at(ordinal)?;
            catalogue.original_path(key, &original)?;
        }
        catalogue.sealed = true;
        let count = catalogue.count;
        catalogue.healthy = true;
        Ok(SourceInventory {
            rooted: self.clone(),
            count,
        })
    }

    /// Called at actual traversal EOF, never at the decision to descend. A
    /// captured directory defaults to opaque, including depth-zero TREE roots.
    pub(crate) fn complete_source_directory_blocking(&self, path: &Path) -> Result<()> {
        self.validate_source_binding_blocking(path, true)?;
        let mut owner = self
            .source_observations
            .lock()
            .map_err(|_| RootedFsError::SourceCatalogueTerminal)?;
        let catalogue = owner.active()?;
        catalogue.begin()?;
        let key = path_key(path);
        let original = catalogue.require(path)?;
        if !catalogue.scanning || original.effective.kind != libc::S_IFDIR as u64 {
            return Err(invalid().into());
        }
        let mut state = catalogue.original_state(key)?;
        if catalogue.completed {
            if state.directory != SourceDirectoryDisposition::Traversed {
                return Err(invalid().into());
            }
        } else {
            state.directory = SourceDirectoryDisposition::Traversed;
            catalogue.set_original_state(key, state)?;
        }
        catalogue.set_index_record(
            state_key(b"traversal", key),
            IndexRecord::Epoch(catalogue.epoch),
        )?;
        catalogue.healthy = true;
        Ok(())
    }
}

impl SourceInventory {
    /// Selected anchors are binding-only. Protect both a followed name's LINK
    /// and its actual DATA object; neither key alone describes that selection.
    pub(in crate::rooted_fs) fn selected_leaf_keys_blocking(
        &self,
    ) -> Result<Option<[NativeObjectKey; 2]>> {
        let mut owner = self
            .rooted
            .source_observations
            .lock()
            .map_err(|_| RootedFsError::SourceCatalogueTerminal)?;
        let catalogue = owner.active()?;
        catalogue.begin()?;
        if !catalogue.sealed
            || !catalogue.completed
            || catalogue.scanning
            || catalogue.count != self.count
        {
            return Err(RootedFsError::SourceInventoryIncomplete);
        }
        let proof = if catalogue.scope == Some(SourceRootProtection::SelectedLeafAnchor)
            && self.count == 2
        {
            let (_, root, _) = catalogue.original_at(0)?;
            let (key, leaf, _) = catalogue.original_at(1)?;
            if root.object_key != object_key(self.rooted.root_fd.as_raw_fd(), None)? {
                return Err(catalogue.changed(PathBuf::new()));
            }
            let objects = if leaf.parent != [0; 32] || catalogue.selected != Some(key) {
                None
            } else if leaf.followed
                && leaf.named.kind == libc::S_IFLNK as u64
                && leaf.effective.kind == libc::S_IFREG as u64
            {
                // Replay the genuine original operator selection, not a scope
                // flag or a target spelling. This cannot authorize descendants.
                let path = catalogue.original_path(key, &leaf)?;
                let terminal = catalogue
                    .selected_terminal(&self.rooted, &path)?
                    .ok_or_else(|| RootedFsError::SourceFollowUnsupported(path.clone()))?;
                if terminal.object_key != leaf.followed_object_key {
                    return Err(catalogue.changed(path));
                }
                leaf.object_key.zip(terminal.object_key)
            } else if !leaf.followed
                && (leaf.named.kind == libc::S_IFREG as u64
                    || leaf.named.kind == libc::S_IFLNK as u64)
            {
                leaf.object_key.map(|object| (object, object))
            } else {
                None
            };
            match (root.object_key, objects) {
                (Some(root), Some((named, effective)))
                    if root.independent(named) && root.independent(effective) =>
                {
                    Some([named, effective])
                }
                _ => None,
            }
        } else {
            None
        };
        catalogue.healthy = true;
        Ok(proof)
    }

    /// Exact count includes the root (TREE subtree or selected binding anchor).
    #[cfg(test)]
    pub(crate) fn record_count(&self) -> u64 {
        self.count
    }

    /// One locked native-binding batch, not a reusable lease or snapshot.
    pub(in crate::rooted_fs) fn validate_originals_blocking(&self) -> Result<()> {
        self.with_original_batch(|c| c.validate_original_records(&self.rooted))
    }

    pub(in crate::rooted_fs) fn validate_preflight_scratch_blocking(&self) -> Result<()> {
        self.validate_lifetime_blocking()?;
        let root = File::from(self.rooted.root_fd.try_clone()?);
        crate::engine::scan_sort::validate_scratch_root(&root, &tempfile::env::temp_dir())?;
        Ok(())
    }

    /// Allocation can record the current sealed owner, never an effect permit.
    pub(in crate::rooted_fs) fn validate_lifetime_blocking(&self) -> Result<()> {
        self.with_original_batch(|c| c.check_original_ordinal(0, [0; 32]).map(|_| ()))
    }

    /// Used only with independent evidence of exclusive FILE allocation after
    /// this owner was sealed. Certify the creator's current physical prefix;
    /// birth alone does not keep a relocated destination outside SOURCE.
    pub(in crate::rooted_fs) fn excludes_private_creator_blocking(
        &self,
        creator: RawFd,
    ) -> Result<bool> {
        self.with_original_batch(|c| {
            c.check_original_ordinal(0, [0; 32])?;
            if c.scope != Some(SourceRootProtection::Tree) {
                return Ok(false);
            }
            let (_, root, _) = c.original_at(0)?;
            let Some(source) = root.object_key else {
                return Ok(false);
            };
            if object_key(self.rooted.root_fd.as_raw_fd(), None)? != Some(source) {
                return Ok(false);
            }
            let Some(creator_key) = object_key(creator, None)? else {
                return Ok(false);
            };
            super::super::local_source_effects::ancestry::excludes(creator, creator_key, source)
        })
    }

    /// Both ordinary replay and native exclusion own the same original-address,
    /// ROOT, extent and run-state barriers. No partially checked catalogue is
    /// exposed as healthy, and the result cannot be reused as an effect lease.
    fn with_original_batch<T>(
        &self,
        validate: impl FnOnce(&mut Catalogue) -> Result<T>,
    ) -> Result<T> {
        let mut owner = self
            .rooted
            .source_observations
            .lock()
            .map_err(|_| RootedFsError::SourceCatalogueTerminal)?;
        owner.active()?.begin()?;
        if self.rooted.source_path_continuity {
            owner.validate_original_operator(&self.rooted)?;
        }
        let c = owner.active()?;
        if !c.sealed || c.scanning || !c.completed || c.count != self.count || self.count == 0 {
            return Err(RootedFsError::SourceInventoryIncomplete);
        }
        c.scope.ok_or_else(invalid)?;
        c.check_inventory_root(&self.rooted)?;
        let result = validate(c);
        let value = c.native_result(result)?;
        if c.file.metadata()?.len() != c.end {
            return Err(invalid().into());
        }
        c.check_inventory_root(&self.rooted)?;
        if self.rooted.source_path_continuity {
            owner.validate_original_operator(&self.rooted)?;
        }
        let c = owner.active()?;
        c.check_run()?;
        c.healthy = true;
        Ok(value)
    }
}
