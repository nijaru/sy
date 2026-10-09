//! Exact run-owned old-inode lineage. One anonymous disk radix stores ancestry
//! and current identities; lookups take at most 256 branches per index, with
//! constant RAM and one scratch descriptor regardless of tree/group size.
//!
//! The root's blocking-worker lock covers validation, native mutation and both
//! observations of the same old descriptor. It is independent of cancellation
//! admission: closing admission never waits for filesystem work or this lock.

use super::{
    identity_from_stat, stat_at_optional, stat_fd, HeldDestinationExpectation, Result,
    RootedFsError,
};
use crate::engine::domain::EntryIdentity;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::path::Path;

pub(super) enum RetirementStep {
    Exchange,
    Unlink,
}

/// Lives only across native publication/cleanup, not across transfer work.
pub(super) struct RetiredDestinationObservation {
    file: File,
    before: libc::stat,
    tracks_lineage: bool,
}

impl RetiredDestinationObservation {
    pub(super) fn capture(
        parent: RawFd,
        leaf: &OsStr,
        path: &Path,
        expected: HeldDestinationExpectation,
        lineage: &mut RetirementLineage,
    ) -> Result<Option<Self>> {
        let Some(named) = stat_at_optional(parent, leaf)? else {
            return Ok(None);
        };
        let name = super::component_cstring(leaf)?;
        #[cfg(target_os = "linux")]
        let flags = libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        #[cfg(target_os = "macos")]
        let flags = libc::O_CLOEXEC
            | if named.st_mode & libc::S_IFMT == libc::S_IFLNK {
                libc::O_SYMLINK
            } else {
                libc::O_EVTONLY | libc::O_NOFOLLOW
            };
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK;
        // SAFETY: the held parent and live single-component name are valid;
        // native no-follow observation flags never traverse the leaf.
        let fd = unsafe { libc::openat(parent, name.as_ptr(), flags) };
        if fd < 0 {
            return Err(io::Error::last_os_error().into());
        }
        // SAFETY: successful openat returned one new owned descriptor.
        let file = unsafe { File::from_raw_fd(fd) };
        let before = stat_fd(file.as_raw_fd())?;
        // All old entries need exchange cleanup ownership. Only multiply-linked
        // nondirectories need destination-alias retirement authority.
        let tracks_lineage = before.st_mode & libc::S_IFMT != libc::S_IFDIR && before.st_nlink > 1;
        let observation = Self {
            file,
            before,
            tracks_lineage,
        };
        observation.verify(&named, path)?;
        if let HeldDestinationExpectation::Unchanged(expected) = expected {
            if identity_from_stat(&before) != Some(lineage.resolve(expected)?) {
                return Err(RootedFsError::DestinationChanged(path.to_path_buf()));
            }
        }
        Ok(Some(observation))
    }

    pub(super) fn cleanup_identity(&self) -> libc::stat {
        self.before
    }

    pub(super) fn verify(&self, named: &libc::stat, path: &Path) -> Result<()> {
        let held = stat_fd(self.file.as_raw_fd())?;
        if !super::staging::same_observation(&self.before, &held)
            || !super::staging::same_observation(&self.before, named)
        {
            return Err(RootedFsError::DestinationChanged(path.to_path_buf()));
        }
        Ok(())
    }

    pub(super) fn record(
        &mut self,
        lineage: &mut RetirementLineage,
        step: RetirementStep,
        path: &Path,
    ) -> Result<()> {
        // No surviving alias needs authority after unlink of an unshared entry.
        // In particular macOS cannot fstat an unlinked directory descriptor.
        if !self.tracks_lineage && matches!(step, RetirementStep::Unlink) {
            return Ok(());
        }
        let result = (|| {
            let after = stat_fd(self.file.as_raw_fd())?;
            let before = &self.before;
            let links_match = match step {
                RetirementStep::Exchange => before.st_nlink == after.st_nlink,
                RetirementStep::Unlink => before.st_nlink.checked_sub(1) == Some(after.st_nlink),
            };
            // Only native namespace-owned ctime/nlink changes may advance
            // authority. Other inode properties must survive exactly. As with
            // stat+rename itself, this is observational, not portable CAS or
            // isolation from foreign ctime-only edits during the native interval.
            let unchanged = super::staging::same_preserved_state(before, &after);
            if !links_match || !unchanged {
                return Err(RootedFsError::DestinationChanged(path.to_path_buf()));
            }
            let before_identity = identity_from_stat(before)
                .ok_or_else(|| RootedFsError::DestinationChanged(path.to_path_buf()))?;
            let after_identity = identity_from_stat(&after)
                .ok_or_else(|| RootedFsError::DestinationChanged(path.to_path_buf()))?;
            if self.tracks_lineage {
                lineage.record(before_identity, after_identity)?;
            }
            self.before = after;
            Ok(())
        })();
        if result.is_err() && self.tracks_lineage {
            lineage.invalidate();
        }
        result
    }
}
#[derive(Default)]
pub(super) struct RetirementLineage {
    disk: Option<DiskIndex>,
    failed: bool,
}

impl RetirementLineage {
    pub(super) fn resolve(&mut self, identity: EntryIdentity) -> io::Result<EntryIdentity> {
        self.check()?;
        let Some(disk) = self.disk.as_mut() else {
            return Ok(identity);
        };
        let Some(origin) = disk.get(0, *identity.as_bytes())? else {
            return Ok(identity);
        };
        disk.get(1, origin)?
            .map(EntryIdentity::from_bytes)
            .ok_or_else(|| invalid("retirement ancestry has no current identity"))
    }

    pub(super) fn record(&mut self, before: EntryIdentity, after: EntryIdentity) -> io::Result<()> {
        self.check()?;
        if self.disk.is_none() {
            self.disk = Some(DiskIndex {
                file: tempfile::tempfile()?,
                roots: [None; 2],
            });
        }
        // A partial write must poison authority, not become a missing lineage.
        self.failed = true;
        let disk = self
            .disk
            .as_mut()
            .ok_or_else(|| invalid("missing retirement index"))?;
        let origin = disk
            .get(0, *before.as_bytes())?
            .unwrap_or(*before.as_bytes());
        if let Some(latest) = disk.get(1, origin)? {
            if latest != *before.as_bytes() {
                return Err(invalid(
                    "retirement does not extend the recorded inode state",
                ));
            }
        }
        if disk
            .get(0, *after.as_bytes())?
            .is_some_and(|recorded| recorded != origin)
        {
            return Err(invalid(
                "retirement identity belongs to a different lineage",
            ));
        }
        disk.set(0, *before.as_bytes(), origin)?;
        disk.set(0, *after.as_bytes(), origin)?;
        disk.set(1, origin, *after.as_bytes())?;
        self.failed = false;
        Ok(())
    }

    pub(super) fn invalidate(&mut self) {
        self.failed = true;
    }

    fn check(&self) -> io::Result<()> {
        if self.failed {
            Err(invalid("retirement lineage is incomplete"))
        } else {
            Ok(())
        }
    }
}

struct DiskIndex {
    file: File,
    roots: [Option<u64>; 2],
}

enum Node {
    Branch { bit: u16, children: [u64; 2] },
    Leaf([u8; 32]),
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn side(key: &[u8; 32], bit: u16) -> usize {
    usize::from((key[usize::from(bit / 8)] >> (7 - bit % 8)) & 1)
}

impl DiskIndex {
    fn node(&mut self, offset: u64) -> io::Result<Node> {
        self.file.seek(SeekFrom::Start(offset))?;
        let mut tag = [0];
        self.file.read_exact(&mut tag)?;
        match tag[0] {
            0 => {
                let mut bytes = [0; 18];
                self.file.read_exact(&mut bytes)?;
                let bit = u16::from_le_bytes([bytes[0], bytes[1]]);
                if bit >= 256 {
                    return Err(invalid("invalid retirement branch bit"));
                }
                let mut children = [0; 2];
                for (child, bytes) in children.iter_mut().zip(bytes[2..].as_chunks::<8>().0) {
                    *child = u64::from_le_bytes(*bytes);
                }
                Ok(Node::Branch { bit, children })
            }
            1 => {
                let mut key = [0; 32];
                self.file.read_exact(&mut key)?;
                Ok(Node::Leaf(key))
            }
            _ => Err(invalid("invalid retirement node tag")),
        }
    }

    fn find(&mut self, index: usize, key: [u8; 32]) -> io::Result<Option<(u64, [u8; 32])>> {
        let Some(mut cursor) = self.roots[index] else {
            return Ok(None);
        };
        let mut previous_bit = None;
        loop {
            match self.node(cursor)? {
                Node::Branch { bit, children } => {
                    if previous_bit.is_some_and(|previous| bit <= previous) {
                        return Err(invalid("unordered retirement branch bits"));
                    }
                    previous_bit = Some(bit);
                    cursor = children[side(&key, bit)];
                }
                Node::Leaf(found) => return Ok(Some((cursor, found))),
            }
        }
    }

    fn get(&mut self, index: usize, key: [u8; 32]) -> io::Result<Option<[u8; 32]>> {
        match self.find(index, key)? {
            Some((_, found)) if found == key => {
                let mut value = [0; 32];
                self.file.read_exact(&mut value)?;
                Ok(Some(value))
            }
            _ => Ok(None),
        }
    }

    fn set(&mut self, index: usize, key: [u8; 32], value: [u8; 32]) -> io::Result<()> {
        let found = self.find(index, key)?;
        if let Some((offset, found)) = found {
            if found == key {
                self.file.seek(SeekFrom::Start(offset + 33))?;
                return self.file.write_all(&value);
            }
        }
        let leaf = self.file.seek(SeekFrom::End(0))?;
        self.file.write_all(&[1])?;
        self.file.write_all(&key)?;
        self.file.write_all(&value)?;
        let Some((_, found)) = found else {
            self.roots[index] = Some(leaf);
            return Ok(());
        };
        let bit = key
            .iter()
            .zip(found)
            .enumerate()
            .find_map(|(index, (a, b))| {
                let difference = a ^ b;
                (difference != 0).then(|| (index * 8 + difference.leading_zeros() as usize) as u16)
            })
            .ok_or_else(|| invalid("duplicate retirement key"))?;
        let mut cursor = self.roots[index].ok_or_else(|| invalid("missing retirement root"))?;
        let mut parent_slot = None;
        loop {
            match self.node(cursor)? {
                Node::Branch {
                    bit: branch_bit,
                    children,
                } if branch_bit < bit => {
                    let child = side(&key, branch_bit);
                    parent_slot = Some(cursor + 3 + child as u64 * 8);
                    cursor = children[child];
                }
                _ => break,
            }
        }
        let branch = self.file.seek(SeekFrom::End(0))?;
        let mut children = [cursor; 2];
        children[side(&key, bit)] = leaf;
        self.file.write_all(&[0])?;
        self.file.write_all(&bit.to_le_bytes())?;
        for child in children {
            self.file.write_all(&child.to_le_bytes())?;
        }
        if let Some(slot) = parent_slot {
            self.file.seek(SeekFrom::Start(slot))?;
            self.file.write_all(&branch.to_le_bytes())?;
        } else {
            self.roots[index] = Some(branch);
        }
        Ok(())
    }
}
