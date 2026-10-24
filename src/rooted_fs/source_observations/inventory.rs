//! Local original protection evidence, not byte, mutation or completion authority.
//! The ordinal mapping shares the catalogue's authenticated index; no second
//! observation owner or per-entry resident state is introduced.
use super::*;
mod census;
mod empty;
mod protection;
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
    pub(super) fn child_count(&mut self, key: [u8; 32]) -> Result<u64> {
        match self.index.get_bytes(state_key(b"children", key))? {
            None => Ok(0),
            Some(bytes) if bytes[8..] == [0; 40] => Ok(u64::from_le_bytes(
                bytes[..8].try_into().map_err(|_| invalid())?,
            )),
            Some(_) => Err(invalid().into()),
        }
    }
    pub(super) fn check_completed_traversal(&mut self) -> Result<()> {
        let mut epoch = [0; 48];
        epoch[..8].copy_from_slice(&self.epoch.to_le_bytes());
        for ordinal in 0..self.count {
            let (key, original) = self.original_at(ordinal)?;
            if original.effective.kind == libc::S_IFDIR as u64
                && self.disposition(key)? == SourceDirectoryDisposition::Traversed
                && self.index.get_bytes(state_key(b"traversal", key))? != Some(epoch)
            {
                return Err(invalid().into());
            }
        }
        Ok(())
    }
    pub(super) fn disposition(&mut self, key: [u8; 32]) -> Result<SourceDirectoryDisposition> {
        match self.index.get_bytes(state_key(b"directory", key))? {
            None => Ok(SourceDirectoryDisposition::Opaque),
            Some(bytes) if bytes == [1; 48] => Ok(SourceDirectoryDisposition::Traversed),
            Some(bytes) if bytes == [2; 48] => Ok(SourceDirectoryDisposition::Protected),
            Some(_) => Err(invalid().into()),
        }
    }
    pub(super) fn original_at(&mut self, ordinal: u64) -> Result<([u8; 32], Observation)> {
        if ordinal >= self.count {
            return Err(invalid().into());
        }
        let locator = self
            .index
            .get_bytes(ordinal_key(ordinal))?
            .ok_or_else(invalid)?;
        let observation = self.read_locator(locator)?;
        let key = if ordinal == 0 {
            [0; 32]
        } else {
            key(observation.parent, &observation.name)
        };
        if self.index.get_bytes(key)? != Some(locator) {
            return Err(invalid().into());
        }
        Ok((key, observation))
    }
    pub(super) fn original_path(&mut self, mut key: [u8; 32]) -> Result<PathBuf> {
        let mut names = Vec::new();
        let mut bytes = 2usize;
        while key != [0; 32] {
            let observation = self.get(key)?.ok_or_else(invalid)?;
            if observation.name.is_empty()
                || observation.name.as_bytes().contains(&0)
                || observation.name.as_bytes().contains(&b'/')
                || observation.name == "."
                || observation.name == ".."
                || super::key(observation.parent, &observation.name) != key
            {
                return Err(invalid().into());
            }
            bytes = bytes
                .checked_add(2 + observation.name.as_bytes().len())
                .ok_or_else(invalid)?;
            if bytes > crate::protocol::MAX_WIRE_PATH_BYTES
                || names.len() >= crate::protocol::MAX_WIRE_COMPONENTS
            {
                return Err(invalid().into());
            }
            key = observation.parent;
            names.push(observation.name);
        }
        let root = self.get([0; 32])?.ok_or_else(invalid)?;
        if root.parent != [0; 32] || !root.name.is_empty() {
            return Err(invalid().into());
        }
        Ok(names.into_iter().rev().collect())
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
            let (key, _) = catalogue.original_at(ordinal)?;
            catalogue.original_path(key)?;
            catalogue.disposition(key)?;
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
        if catalogue.completed {
            if catalogue.disposition(key)? != SourceDirectoryDisposition::Traversed {
                return Err(invalid().into());
            }
        } else {
            catalogue
                .index
                .set_bytes(state_key(b"directory", key), [1; 48])?;
        }
        let mut epoch = [0; 48];
        epoch[..8].copy_from_slice(&catalogue.epoch.to_le_bytes());
        catalogue
            .index
            .set_bytes(state_key(b"traversal", key), epoch)?;
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
            let (_, root) = catalogue.original_at(0)?;
            let (key, leaf) = catalogue.original_at(1)?;
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
                let path = catalogue.original_path(key)?;
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

    /// One locked native-binding batch, not a reusable lease or snapshot. No
    /// SOURCE or destination effect occurs here. Operator changes persistent
    /// through the batch are rejected by its end barrier before admission;
    /// native relative checks still use every authenticated original ancestor.
    pub(in crate::rooted_fs) fn validate_originals_blocking(&self) -> Result<()> {
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
        for ordinal in 0..self.count {
            let (key, original) = c.original_at(ordinal)?;
            let path = c.original_path(key)?;
            let mut expected = [0; 48];
            expected[..8].copy_from_slice(&ordinal.to_le_bytes());
            if c.index.get_bytes(state_key(b"ordinal", key))? != Some(expected) {
                return Err(invalid().into());
            }
            c.disposition(key)?;
            if ordinal == 0
                && !original
                    .effective
                    .same_binding(NativeObservation::from_stat(&stat_fd(
                        self.rooted.root_fd.as_raw_fd(),
                    )?)?)
            {
                return Err(RootedFsError::RootChanged(
                    self.rooted.root_path.as_ref().clone(),
                ));
            }
            let result = c.validate_native_binding(&self.rooted, &path, true);
            c.native_result(result)?;
            #[cfg(test)]
            self.rooted
                .pause_mutation(PublicationPausePoint::AfterSourceValidation);
        }
        // Exact trusted-count exhaustion and tape/index health remain mandatory,
        // including repeated batches. Never rehash or adopt altered scratch.
        if c.file.metadata()?.len() != c.end {
            return Err(invalid().into());
        }
        let (_, root) = c.original_at(0)?;
        if !root
            .effective
            .same_binding(NativeObservation::from_stat(&stat_fd(
                self.rooted.root_fd.as_raw_fd(),
            )?)?)
        {
            return Err(RootedFsError::RootChanged(
                self.rooted.root_path.as_ref().clone(),
            ));
        }
        if self.rooted.source_path_continuity {
            owner.validate_original_operator(&self.rooted)?;
        }
        // Until the final original-address check succeeds, no partially checked
        // catalogue is exposed as healthy, even on native/authentication errors.
        owner.active()?.healthy = true;
        Ok(())
    }
}
