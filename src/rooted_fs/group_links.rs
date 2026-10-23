//! Aggregate link provenance, independent of SOURCE health and exact nlink.
//! One authenticated tape, sharing the anchor's pins; no descriptor per member.
use super::*;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::ffi::OsStringExt;

pub(super) enum PrivateLink {
    Anchor,
    Alias,
}

enum LinkProvenance {
    Certified,
    Uncertain,
}

struct AnchorBinding {
    parent: Arc<OwnedFd>,
    directory: Arc<OwnedFd>,
    name: OsString,
    contents: OsString,
    path: PathBuf,
    creation: local_source_effects::PrivateCreationAdmission,
}

pub(super) struct GroupLinks {
    rooted: RootedFs,
    anchor: Option<AnchorBinding>,
    provenance: LinkProvenance,
    tape: File,
    end: u64,
    members: u64,
    digest: blake3::Hash,
}

const MAX_MEMBER_PATH: usize = 64 * 1024;

impl GroupLinks {
    pub(super) fn new(anchor: &RootedNamespaceTransaction) -> Result<Self> {
        Ok(Self {
            rooted: anchor.rooted.clone(),
            anchor: Some(AnchorBinding {
                parent: Arc::clone(&anchor.parent_fd),
                directory: Arc::clone(&anchor.staging_dir_fd),
                name: anchor.staging_dir_name.clone(),
                contents: anchor.temp_name.clone(),
                path: anchor.destination_path.clone(),
                creation: anchor.private_creation,
            }),
            provenance: LinkProvenance::Certified,
            tape: tempfile::tempfile()?,
            end: 0,
            members: 0,
            digest: blake3::hash(b"sy/source-guarded-group-members/v1\0"),
        })
    }

    fn extend(digest: blake3::Hash, bytes: &[u8]) -> blake3::Hash {
        let mut hash = blake3::Hasher::new();
        hash.update(digest.as_bytes());
        hash.update(&(bytes.len() as u32).to_le_bytes());
        hash.update(bytes);
        hash.finalize()
    }

    /// Only a successful native publication adds a name. Failed append is
    /// terminal, never authority to rediscover/adopt current inode aliases.
    pub(super) fn record_publication(&mut self, path: &Path) -> Result<()> {
        if matches!(self.provenance, LinkProvenance::Uncertain) {
            return Err(RootedFsError::GroupLinkOwnershipUncertain);
        }
        self.provenance = LinkProvenance::Uncertain;
        let bytes = path.as_os_str().as_bytes();
        if bytes.is_empty()
            || bytes.len() > MAX_MEMBER_PATH
            || self.tape.metadata()?.len() != self.end
        {
            return Err(RootedFsError::IncompleteStagedGroup);
        }
        let end = self
            .end
            .checked_add(40 + bytes.len() as u64)
            .ok_or(RootedFsError::IncompleteStagedGroup)?;
        let members = self
            .members
            .checked_add(1)
            .ok_or(RootedFsError::StagedGroupCountExhausted)?;
        self.tape.seek(SeekFrom::Start(self.end))?;
        self.tape.write_all(self.digest.as_bytes())?;
        self.tape.write_all(&(bytes.len() as u32).to_le_bytes())?;
        self.tape.write_all(bytes)?;
        self.tape.write_all(&(bytes.len() as u32).to_le_bytes())?;
        self.digest = Self::extend(self.digest, bytes);
        self.end = end;
        self.members = members;
        self.provenance = LinkProvenance::Certified;
        Ok(())
    }

    pub(super) fn retain_uncertain_alias(&mut self) {
        self.provenance = LinkProvenance::Uncertain;
    }

    pub(super) fn retire_anchor(&mut self) {
        // Only the confirmed owned unlink retires this proof and its pins.
        self.anchor = None;
    }

    /// All known links, not just a representative, must still have their
    /// original DST ancestry. A relocated directory leaves FILE stat unchanged.
    /// This proof uses original namespace records, never a fresh SOURCE census.
    pub(super) fn verify(&mut self, file: &File, observed: &libc::stat) -> Result<()> {
        if matches!(self.provenance, LinkProvenance::Uncertain) {
            return Err(RootedFsError::GroupLinkOwnershipUncertain);
        }
        self.provenance = LinkProvenance::Uncertain;
        self.rooted.verify_root_path_blocking()?;
        if self.tape.metadata()?.len() != self.end {
            return Err(RootedFsError::IncompleteStagedGroup);
        }
        // Reverse hash chaining authenticates each bounded path BEFORE decoding
        // or native lookup. The final digest/count/end remain in trusted state.
        let mut digest = self.digest;
        let mut end = self.end;
        for _ in 0..self.members {
            let footer = end
                .checked_sub(4)
                .ok_or(RootedFsError::IncompleteStagedGroup)?;
            self.tape.seek(SeekFrom::Start(footer))?;
            let mut length = [0; 4];
            self.tape.read_exact(&mut length)?;
            let length = u32::from_le_bytes(length) as usize;
            if length == 0 || length > MAX_MEMBER_PATH {
                return Err(RootedFsError::IncompleteStagedGroup);
            }
            end = end
                .checked_sub(40 + length as u64)
                .ok_or(RootedFsError::IncompleteStagedGroup)?;
            self.tape.seek(SeekFrom::Start(end))?;
            let mut previous = [0; 32];
            self.tape.read_exact(&mut previous)?;
            let mut header = [0; 4];
            self.tape.read_exact(&mut header)?;
            let mut bytes = vec![0; length];
            self.tape.read_exact(&mut bytes)?;
            let previous = blake3::Hash::from_bytes(previous);
            if u32::from_le_bytes(header) as usize != length
                || Self::extend(previous, &bytes) != digest
            {
                return Err(RootedFsError::IncompleteStagedGroup);
            }
            digest = previous;
            let path = RelativePath::new(PathBuf::from(OsString::from_vec(bytes)))
                .map_err(|_| RootedFsError::IncompleteStagedGroup)?;
            publication::verify_file_binding(file, &self.rooted, &path, observed)?;
        }
        if end != 0 || digest != blake3::hash(b"sy/source-guarded-group-members/v1\0") {
            return Err(RootedFsError::IncompleteStagedGroup);
        }
        // Member replay may be long. Recheck the private anchor binding last,
        // rather than carrying its proof across those blocking native reads.
        if let Some(anchor) = &self.anchor {
            self.rooted
                .verify_parent_binding_blocking(&anchor.path, &anchor.parent)?;
            anchor
                .creation
                .verify_parent_blocking(anchor.parent.as_raw_fd(), &anchor.path)?;
            let held = stat_fd(anchor.directory.as_raw_fd())?;
            let named = stat_at_optional(anchor.parent.as_raw_fd(), &anchor.name)?;
            if !named
                .as_ref()
                .is_some_and(|named| staging::same_inode(&held, named))
            {
                return Err(RootedFsError::StagingDirectoryChanged);
            }
            let contents = stat_at_optional(anchor.directory.as_raw_fd(), &anchor.contents)?;
            if !contents
                .as_ref()
                .is_some_and(|named| staging::same_observation(observed, named))
            {
                return Err(RootedFsError::StagingEntryChanged(anchor.path.clone()));
            }
        }
        self.provenance = LinkProvenance::Certified;
        Ok(())
    }
}

impl RootedNamespaceTransaction {
    pub(super) fn verify_group_links_blocking(&self) -> Result<()> {
        if let Some(links) = &self.group_links {
            let owned = self
                .staged
                .as_ref()
                .ok_or(RootedFsError::IncompleteStagedGroup)?;
            let sealed = self.sealed.ok_or(RootedFsError::IncompleteStagedGroup)?;
            links
                .lock()
                .map_err(|_| RootedFsError::IncompleteStagedGroup)?
                .verify(&owned.file, &sealed.stat)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corrupt_member_tape_is_terminal_for_link_retirement_authority() {
        for damage in ["path", "length", "append", "truncate", "chain"] {
            let directory = tempfile::tempdir().unwrap();
            let rooted = RootedFs::open_blocking(directory.path().to_path_buf()).unwrap();
            let mut anchor = rooted
                .begin_staged_file_blocking(&RelativePath::new("member").unwrap())
                .unwrap();
            anchor.file_mut().write_all(b"owned").unwrap();
            let mut links = GroupLinks::new(&anchor.namespace).unwrap();
            std::fs::hard_link(
                directory
                    .path()
                    .join(&anchor.namespace.staging_dir_name)
                    .join("contents"),
                directory.path().join("member"),
            )
            .unwrap();
            links.record_publication(Path::new("member")).unwrap();
            let before = stat_fd(anchor.file.as_raw_fd()).unwrap();
            links.verify(&anchor.file, &before).unwrap();
            match damage {
                "path" => {
                    links.tape.seek(SeekFrom::Start(36)).unwrap();
                    links.tape.write_all(b"/").unwrap();
                }
                "length" => {
                    links.tape.seek(SeekFrom::Start(links.end - 4)).unwrap();
                    links.tape.write_all(&u32::MAX.to_le_bytes()).unwrap();
                }
                "append" => {
                    links.tape.seek(SeekFrom::End(0)).unwrap();
                    links.tape.write_all(b"extra").unwrap();
                }
                "truncate" => links.tape.set_len(links.end - 1).unwrap(),
                "chain" => {
                    links.tape.seek(SeekFrom::Start(0)).unwrap();
                    links.tape.write_all(&[0; 32]).unwrap();
                }
                _ => unreachable!(),
            }
            assert!(links.verify(&anchor.file, &before).is_err(), "{damage}");
            assert!(
                matches!(
                    links.record_publication(Path::new("member")),
                    Err(RootedFsError::GroupLinkOwnershipUncertain)
                ),
                "{damage}"
            );
            assert!(
                matches!(
                    links.verify(&anchor.file, &before),
                    Err(RootedFsError::GroupLinkOwnershipUncertain)
                ),
                "{damage}"
            );
            assert!(staging::same_observation(
                &before,
                &stat_fd(anchor.file.as_raw_fd()).unwrap()
            ));
        }
    }
}
