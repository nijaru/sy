//! Exact session-local hardlink representatives without a tree-sized cache.
//!
//! A compressed binary radix index and its variable-length records share one
//! anonymous scratch file. Lookup visits at most 256 branches, retaining one
//! record; neither group count nor path ordering increases RAM or descriptors.
//! Executors serialize publication and insertion with their async group lock.

use super::domain::{EntryIdentity, RelativePath, Timestamp};
use super::native_path;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::sync::{Arc, Mutex};

const MAX_PATH_BYTES: usize = 1024 * 1024;
const BRANCH_BYTES: usize = 19;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HardlinkRepresentative {
    pub path: RelativePath,
    pub publication: EntryIdentity,
    pub unix_mode: Option<u32>,
    pub modified: Option<Timestamp>,
}

impl HardlinkRepresentative {
    /// Inode metadata is applied by the representative transaction only.
    pub fn validate_metadata(
        &self,
        unix_mode: Option<u32>,
        modified: Option<Timestamp>,
    ) -> io::Result<()> {
        let mode = |mode: Option<u32>| mode.map(|value| value & 0o7777);
        if mode(self.unix_mode) != mode(unix_mode) || self.modified != modified {
            return Err(io::Error::other(
                "hardlink group members request incompatible inode metadata",
            ));
        }
        Ok(())
    }
}

/// Lazily creates scratch state only when a grouped transfer actually runs.
/// The blocking lock protects file seeks inside workers, never across `.await`;
/// the executor's async lock separately protects the publication protocol.
#[derive(Default)]
pub(crate) struct HardlinkGroups {
    disk: Arc<Mutex<Option<DiskIndex>>>,
}

impl HardlinkGroups {
    pub async fn get(&self, key: [u8; 32]) -> io::Result<Option<HardlinkRepresentative>> {
        self.with_disk(move |disk| disk.get(key)).await
    }

    pub async fn insert(
        &self,
        key: [u8; 32],
        representative: HardlinkRepresentative,
    ) -> io::Result<()> {
        self.with_disk(move |disk| disk.insert(key, representative))
            .await
    }

    pub async fn advance(&self, key: [u8; 32], identity: EntryIdentity) -> io::Result<()> {
        self.with_disk(move |disk| disk.advance(key, identity))
            .await
    }

    async fn with_disk<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&mut DiskIndex) -> io::Result<T> + Send + 'static,
    ) -> io::Result<T> {
        let disk = Arc::clone(&self.disk);
        tokio::task::spawn_blocking(move || {
            let mut state = disk
                .lock()
                .map_err(|_| io::Error::other("hardlink scratch lock poisoned"))?;
            if state.is_none() {
                *state = Some(DiskIndex {
                    file: tempfile::tempfile()?,
                    root: None,
                    healthy: true,
                });
            }
            operation(
                state
                    .as_mut()
                    .ok_or_else(|| invalid("missing scratch index"))?,
            )
        })
        .await
        .map_err(io::Error::other)?
    }
}

struct DiskIndex {
    file: File,
    root: Option<u64>,
    // A partial scratch write must fail the session, not look like a missing
    // representative and authorize an independent transfer of a group member.
    healthy: bool,
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
                let mut bytes = [0; BRANCH_BYTES - 1];
                self.file.read_exact(&mut bytes)?;
                let bit = u16::from_le_bytes([bytes[0], bytes[1]]);
                if bit >= 256 {
                    return Err(invalid("invalid hardlink branch bit"));
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
            _ => Err(invalid("invalid hardlink node tag")),
        }
    }

    fn get(&mut self, key: [u8; 32]) -> io::Result<Option<HardlinkRepresentative>> {
        if !self.healthy {
            return Err(invalid("hardlink scratch index is incomplete"));
        }
        let Some(mut cursor) = self.root else {
            return Ok(None);
        };
        let mut previous_bit = None;
        loop {
            match self.node(cursor)? {
                Node::Branch { bit, children } => {
                    if previous_bit.is_some_and(|previous| bit <= previous) {
                        return Err(invalid("unordered hardlink branch bits"));
                    }
                    previous_bit = Some(bit);
                    cursor = children[side(&key, bit)];
                }
                Node::Leaf(found) if found == key => return self.read_representative().map(Some),
                Node::Leaf(_) => return Ok(None),
            }
        }
    }

    fn read_representative(&mut self) -> io::Result<HardlinkRepresentative> {
        let mut header = [0; 21];
        self.file.read_exact(&mut header)?;
        let len = u32::from_le_bytes(header[..4].try_into().map_err(|_| invalid("path length"))?)
            as usize;
        if len > MAX_PATH_BYTES || header[4] & !3 != 0 {
            return Err(invalid("invalid hardlink representative header"));
        }
        let unix_mode = if header[4] & 1 != 0 {
            Some(u32::from_le_bytes(
                header[5..9].try_into().map_err(|_| invalid("mode"))?,
            ))
        } else {
            None
        };
        let modified = if header[4] & 2 != 0 {
            Some(
                Timestamp::new(
                    i64::from_le_bytes(header[9..17].try_into().map_err(|_| invalid("seconds"))?),
                    u32::from_le_bytes(header[17..21].try_into().map_err(|_| invalid("nanos"))?),
                )
                .map_err(|_| invalid("invalid hardlink timestamp"))?,
            )
        } else {
            None
        };
        let mut publication = [0; 32];
        self.file.read_exact(&mut publication)?;
        let mut path = vec![0; len];
        self.file.read_exact(&mut path)?;
        let path = RelativePath::new(native_path::decode(&path)?)
            .map_err(|_| invalid("invalid hardlink path"))?;
        Ok(HardlinkRepresentative {
            path,
            publication: EntryIdentity::from_bytes(publication),
            unix_mode,
            modified,
        })
    }

    fn advance(&mut self, key: [u8; 32], identity: EntryIdentity) -> io::Result<()> {
        if !self.healthy {
            return Err(invalid("hardlink scratch index is incomplete"));
        }
        let mut cursor = self.root.ok_or_else(|| invalid("missing hardlink group"))?;
        let mut previous_bit = None;
        loop {
            match self.node(cursor)? {
                Node::Branch { bit, children } => {
                    if previous_bit.is_some_and(|previous| bit <= previous) {
                        return Err(invalid("unordered hardlink branch bits"));
                    }
                    previous_bit = Some(bit);
                    cursor = children[side(&key, bit)];
                }
                Node::Leaf(found) if found == key => {
                    self.healthy = false;
                    self.file.seek(SeekFrom::Start(cursor + 1 + 32 + 21))?;
                    self.file.write_all(identity.as_bytes())?;
                    self.healthy = true;
                    return Ok(());
                }
                Node::Leaf(_) => return Err(invalid("missing hardlink group")),
            }
        }
    }

    fn insert(&mut self, key: [u8; 32], value: HardlinkRepresentative) -> io::Result<()> {
        if self.get(key)?.is_some() {
            return Err(invalid("hardlink representative already published"));
        }
        let path = native_path::encode(value.path.as_path().as_os_str());
        if path.len() > MAX_PATH_BYTES {
            return Err(invalid("hardlink path exceeds scratch record limit"));
        }
        self.healthy = false;
        let leaf = self.file.seek(SeekFrom::End(0))?;
        self.file.write_all(&[1])?;
        self.file.write_all(&key)?;
        self.file.write_all(&(path.len() as u32).to_le_bytes())?;
        self.file.write_all(&[
            u8::from(value.unix_mode.is_some()) | (u8::from(value.modified.is_some()) << 1)
        ])?;
        self.file
            .write_all(&value.unix_mode.unwrap_or_default().to_le_bytes())?;
        let modified = value.modified.unwrap_or(Timestamp::UNIX_EPOCH);
        self.file.write_all(&modified.seconds().to_le_bytes())?;
        self.file.write_all(&modified.nanoseconds().to_le_bytes())?;
        self.file.write_all(value.publication.as_bytes())?;
        self.file.write_all(&path)?;

        if let Some(root) = self.root {
            let mut cursor = root;
            let found = loop {
                match self.node(cursor)? {
                    Node::Branch { bit, children } => cursor = children[side(&key, bit)],
                    Node::Leaf(found) => break found,
                }
            };
            let bit = key
                .iter()
                .zip(found)
                .enumerate()
                .find_map(|(index, (a, b))| {
                    let difference = a ^ b;
                    (difference != 0)
                        .then(|| (index * 8 + difference.leading_zeros() as usize) as u16)
                })
                .ok_or_else(|| invalid("duplicate hardlink key"))?;

            cursor = root;
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
                self.root = Some(branch);
            }
        } else {
            self.root = Some(leaf);
        }
        self.healthy = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn representative(index: usize) -> HardlinkRepresentative {
        HardlinkRepresentative {
            path: RelativePath::new(format!("directory/{index}")).unwrap(),
            publication: EntryIdentity::from_bytes([1; 32]),
            unix_mode: Some(0o640),
            modified: Some(Timestamp::new(-1, 999_999_999).unwrap()),
        }
    }

    #[tokio::test]
    async fn exact_lookup_survives_many_groups_and_long_common_prefixes() {
        let groups = HardlinkGroups::default();
        for index in 0..10_000_usize {
            let mut key = [0; 32];
            key[24..].copy_from_slice(&(index as u64).to_be_bytes());
            groups.insert(key, representative(index)).await.unwrap();
        }
        for index in (0..10_000_usize).rev() {
            let mut key = [0; 32];
            key[24..].copy_from_slice(&(index as u64).to_be_bytes());
            assert_eq!(groups.get(key).await.unwrap(), Some(representative(index)));
        }
        assert!(groups.get([255; 32]).await.unwrap().is_none());
        assert!(groups.insert([0; 32], representative(123)).await.is_err());
        assert_eq!(groups.get([0; 32]).await.unwrap(), Some(representative(0)));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn non_utf8_names_round_trip() {
        use std::os::unix::ffi::OsStringExt;
        let mut value = representative(0);
        value.path = RelativePath::new(std::ffi::OsString::from_vec(vec![b'f', 0xff])).unwrap();
        let groups = HardlinkGroups::default();
        groups.insert([0; 32], value.clone()).await.unwrap();
        assert_eq!(groups.get([0; 32]).await.unwrap(), Some(value));
    }

    // Run in an isolated process: FD limits and RSS must not depend on other
    // concurrent tests, and the original tree-sized map would exceed this
    // memory budget with the same 64 MiB of representative paths.
    #[cfg(unix)]
    #[test]
    fn bounded_memory_and_descriptors() {
        const CHILD: &str = "SY_HARDLINK_RESOURCE_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let result = std::process::Command::new("sh")
                .args(["-c", "ulimit -n 32; exec \"$1\" --exact engine::hardlink_groups::tests::bounded_memory_and_descriptors --nocapture", "hardlink-resource-test"])
                .arg(std::env::current_exe().unwrap())
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            return;
        }
        fn rss_kib() -> usize {
            let output = std::process::Command::new("ps")
                .args(["-o", "rss=", "-p", &std::process::id().to_string()])
                .output()
                .unwrap();
            assert!(output.status.success());
            String::from_utf8(output.stdout)
                .unwrap()
                .trim()
                .parse()
                .unwrap()
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        runtime.block_on(async {
            let groups = HardlinkGroups::default();
            groups.insert([0; 32], representative(0)).await.unwrap();
            let before = rss_kib();
            let path = RelativePath::new("p".repeat(4096)).unwrap();
            for index in 1..=16_384_u64 {
                let key = *blake3::hash(&index.to_le_bytes()).as_bytes();
                groups
                    .insert(
                        key,
                        HardlinkRepresentative {
                            path: path.clone(),
                            publication: EntryIdentity::from_bytes([1; 32]),
                            unix_mode: None,
                            modified: None,
                        },
                    )
                    .await
                    .unwrap();
            }
            let after = rss_kib();
            assert!(
                after < before + 8 * 1024,
                "RSS grew from {before} to {after} KiB"
            );
            // Revisit early representatives after all groups have accumulated;
            // a bounded cache that evicts group fidelity is not acceptable.
            for index in [1_u64, 8_192, 16_384] {
                let key = *blake3::hash(&index.to_le_bytes()).as_bytes();
                assert_eq!(groups.get(key).await.unwrap().unwrap().path, path);
            }
            assert_eq!(groups.get([0; 32]).await.unwrap(), Some(representative(0)));
        });
    }

    #[tokio::test]
    async fn scratch_corruption_fails_without_unbounded_allocation_or_cycles() {
        let groups = HardlinkGroups::default();
        groups.insert([0; 32], representative(0)).await.unwrap();
        groups
            .with_disk(|disk| {
                disk.file.seek(SeekFrom::Start(33))?;
                disk.file.write_all(&u32::MAX.to_le_bytes())
            })
            .await
            .unwrap();
        assert_eq!(
            groups.get([0; 32]).await.unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );

        let groups = HardlinkGroups::default();
        groups.insert([0; 32], representative(0)).await.unwrap();
        groups.insert([255; 32], representative(1)).await.unwrap();
        groups
            .with_disk(|disk| {
                let root = disk.root.unwrap();
                disk.file.seek(SeekFrom::Start(root + 3))?;
                disk.file.write_all(&root.to_le_bytes())
            })
            .await
            .unwrap();
        assert_eq!(
            groups.get([0; 32]).await.unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[tokio::test]
    async fn later_members_cannot_change_inode_metadata() {
        let groups = HardlinkGroups::default();
        let value = representative(0);
        groups.insert([0; 32], value.clone()).await.unwrap();
        let stored = groups.get([0; 32]).await.unwrap().unwrap();
        assert!(stored
            .validate_metadata(Some(0o100640), value.modified)
            .is_ok());
        assert!(stored
            .validate_metadata(Some(0o600), value.modified)
            .is_err());
        assert!(stored.validate_metadata(value.unix_mode, None).is_err());
    }
}
