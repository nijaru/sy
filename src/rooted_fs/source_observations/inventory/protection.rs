//! Protection-only metadata shares the original catalogue, never the sync
//! stream. Its ordinal tape is the queue: one active directory reader, no
//! per-depth descriptor stack or resident tree/name set.
use super::*;
use crate::fs_util::directory_stream::DirectoryStream;
#[cfg(test)]
#[path = "protection_tests.rs"]
mod tests;

impl Catalogue {
    pub(in crate::rooted_fs::source_observations) fn census_protection_metadata(
        &mut self,
        rooted: &RootedFs,
        active: &impl Fn() -> bool,
    ) -> Result<bool> {
        if !self.scanning {
            return Err(invalid().into());
        }
        let mut ordinal = 0;
        while ordinal < self.count {
            if !active() {
                return Ok(false);
            }
            self.check_run()?;
            let (parent_key, original, state) = self.original_at(ordinal)?;
            ordinal += 1;
            if original.named.kind != libc::S_IFDIR as u64 {
                continue;
            }
            let disposition = state.directory;
            if disposition == SourceDirectoryDisposition::Traversed {
                continue;
            }
            let path = self.original_path(parent_key, &original)?;
            if disposition == SourceDirectoryDisposition::Opaque {
                if self.empty_directory_stamp(parent_key)?.is_some() {
                    let parent = match path.parent() {
                        Some(parent) => {
                            self.open_original_directory(rooted.root_fd.as_raw_fd(), parent)?
                        }
                        None => rooted.root_fd.try_clone()?,
                    };
                    self.check_empty_directory(
                        parent_key,
                        parent.as_raw_fd(),
                        path.file_name(),
                        &path,
                    )?;
                    continue;
                }
                // Replays cannot repair an originally unknown/unreadable scope.
                if self.completed {
                    continue;
                }
            }
            let directory = match self.open_original_directory(rooted.root_fd.as_raw_fd(), &path) {
                Ok(directory) => directory,
                // Initial read denial is not permission to omit protection.
                // Keep the scope opaque and require refusal at effect admission.
                Err(RootedFsError::Io(error))
                    if !self.completed && error.kind() == io::ErrorKind::PermissionDenied =>
                {
                    continue
                }
                Err(error) => return Err(source_read_race(error, &path)),
            };
            let mut directory = DirectoryStream::from_directory(directory)?;
            let mut seen = 0u64;
            while let Some(name) = directory.next() {
                if !active() {
                    return Ok(false);
                }
                self.check_run()?;
                let name = name?;
                let child_path = path.join(&name);
                validate_original_path(&child_path)?;
                let child_key = key(parent_key, &name);
                let seen_key = state_key(b"protection-seen", child_key);
                if self.epoch_at(seen_key)? == Some(self.epoch) {
                    return Err(self.changed(child_path));
                }
                self.set_index_record(seen_key, IndexRecord::Epoch(self.epoch))?;
                let (observed, stat, _) = inspect_with_stat(
                    ResolutionRoot::Held(rooted.root_fd.as_raw_fd()),
                    directory.as_raw_fd(),
                    &child_path,
                    false,
                )?;
                if let Some(original) = self.get(child_key)? {
                    // Existing followed originals retain their own DATA
                    // transcript; discovery here inspects physical names only.
                    let matches = if original.followed {
                        original.named.matches(observed.named, true)
                            && original.object_key == observed.object_key
                            && original.target == observed.target
                    } else {
                        original.matches(&observed, true)
                    };
                    if !matches {
                        return Err(self.changed(child_path));
                    }
                } else {
                    if self.completed || self.sealed {
                        return Err(self.changed(child_path));
                    }
                    self.insert(child_key, &observed)?;
                    self.protected_count =
                        self.protected_count.checked_add(1).ok_or_else(invalid)?;
                    self.capture_empty_directory(
                        &child_path,
                        directory.as_raw_fd(),
                        Some(&name),
                        &observed,
                        &stat,
                    )?;
                }
                seen = seen.checked_add(1).ok_or_else(invalid)?;
            }
            // Child capture updates this same state record. Reload at EOF so
            // publishing its disposition cannot overwrite the derived count.
            let mut state = self.original_state(parent_key)?;
            if seen != state.children {
                return Err(self.changed(path));
            }
            let result = self.validate_native_binding(rooted, &path, true);
            self.native_result(result)?;
            if !self.completed {
                // Actual census EOF, not scanner traversal, byte authority,
                // publication, completion, or source-removal permission.
                state.directory = SourceDirectoryDisposition::Protected;
                self.set_original_state(parent_key, state)?;
            }
        }
        Ok(active())
    }
}
