//! Authenticated destination declarations, not SOURCE observations or permits.
use super::*;
use crate::authenticated_index::AuthenticatedIndex;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::ffi::OsStringExt;
#[cfg(test)]
mod tests;

// Root/kind, path length, two candidates, additional predicate and scope hash.
const MAX_RECORD_OVERHEAD: usize = 2 + 4 + 1 + 2 * 32 + 1 + 32 + 32;
const MAX_RECORD: usize = crate::protocol::MAX_WIRE_PATH_BYTES + MAX_RECORD_OVERHEAD;

pub(super) fn invalid() -> RootedFsError {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "invalid LOCAL effect declarations",
    )
    .into()
}

fn record_digest(ordinal: u64, bytes: &[u8]) -> [u8; 32] {
    let mut hash = blake3::Hasher::new();
    hash.update(b"sy/local-effect-record/v1\0");
    hash.update(&ordinal.to_le_bytes());
    hash.update(bytes);
    *hash.finalize().as_bytes()
}

fn index_key(domain: &[u8], bytes: &[u8]) -> [u8; 32] {
    let mut hash = blake3::Hasher::new();
    hash.update(b"sy/local-effect-declarations/v1\0");
    hash.update(domain);
    hash.update(bytes);
    *hash.finalize().as_bytes()
}

#[derive(PartialEq, Eq)]
pub(super) struct Snapshot {
    pub(super) candidates: Vec<native_object::NativeObjectKey>,
    pub(super) additional: SourceExclusion,
    scope: [u8; 32],
}

impl Snapshot {
    pub(super) fn capture(
        effect: &SourceEffect<'_>,
        footprint: footprint::NativeFootprint,
    ) -> Self {
        let mut hash = blake3::Hasher::new();
        hash.update(b"sy/local-effect-scope/v1\0");
        hash.update(&[kind_tag(effect.kind)]);
        hash.update(effect.path.as_os_str().as_bytes());
        Self {
            candidates: footprint.candidates,
            additional: footprint.additional,
            scope: *hash.finalize().as_bytes(),
        }
    }
}

pub(super) struct Record {
    pub(super) root: EffectRoot,
    pub(super) path: RelativePath,
    pub(super) kind: SourceEffectKind,
    pub(super) snapshot: Snapshot,
}

fn kind_tag(kind: SourceEffectKind) -> u8 {
    match kind {
        SourceEffectKind::LeafPublication => 0,
        SourceEffectKind::DirectoryNamespaceChange => 1,
        SourceEffectKind::InodeMetadata => 2,
        SourceEffectKind::SymlinkMetadata => 3,
        SourceEffectKind::DirectoryPolicy => 4,
        SourceEffectKind::DirectoryTimestamp => 5,
        SourceEffectKind::NamespaceRetirement => 6,
        SourceEffectKind::PrivateLeafMetadata => 7,
        SourceEffectKind::PrivateDirectoryMetadata => 8,
        SourceEffectKind::LinkCountChange => 9,
        SourceEffectKind::Unknown => 10,
    }
}

impl Record {
    fn encode(&self) -> Result<Vec<u8>> {
        let path = self.path.as_path().as_os_str().as_bytes();
        if path.len() > crate::protocol::MAX_WIRE_PATH_BYTES
            || !(1..=2).contains(&self.snapshot.candidates.len())
            || kind_tag(self.kind) > 6
        {
            return Err(invalid());
        }
        let mut bytes = Vec::with_capacity(path.len() + MAX_RECORD_OVERHEAD);
        bytes.extend([
            match self.root {
                EffectRoot::Main => 0,
                EffectRoot::Backup => 1,
            },
            kind_tag(self.kind),
        ]);
        bytes.extend(
            u32::try_from(path.len())
                .map_err(|_| invalid())?
                .to_le_bytes(),
        );
        bytes.extend(path);
        bytes.push(self.snapshot.candidates.len() as u8);
        for candidate in &self.snapshot.candidates {
            bytes.extend(candidate.0);
        }
        match self.snapshot.additional {
            SourceExclusion::Objects => bytes.push(0),
            SourceExclusion::OperatorLinks(object) => {
                bytes.push(1);
                bytes.extend(object.0);
            }
            SourceExclusion::DirectoryAncestors(object) => {
                bytes.push(2);
                bytes.extend(object.0);
            }
        }
        bytes.extend(self.snapshot.scope);
        Ok(bytes)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let mut cursor = std::io::Cursor::new(bytes);
        let mut byte = || -> Result<u8> {
            let mut value = [0];
            cursor.read_exact(&mut value)?;
            Ok(value[0])
        };
        let root = match byte()? {
            0 => EffectRoot::Main,
            1 => EffectRoot::Backup,
            _ => return Err(invalid()),
        };
        let kind = match byte()? {
            0 => SourceEffectKind::LeafPublication,
            1 => SourceEffectKind::DirectoryNamespaceChange,
            2 => SourceEffectKind::InodeMetadata,
            3 => SourceEffectKind::SymlinkMetadata,
            4 => SourceEffectKind::DirectoryPolicy,
            5 => SourceEffectKind::DirectoryTimestamp,
            6 => SourceEffectKind::NamespaceRetirement,
            _ => return Err(invalid()),
        };
        let mut length = [0; 4];
        cursor.read_exact(&mut length)?;
        let length = u32::from_le_bytes(length) as usize;
        if length > crate::protocol::MAX_WIRE_PATH_BYTES || length > bytes.len() {
            return Err(invalid());
        }
        let mut path = vec![0; length];
        cursor.read_exact(&mut path)?;
        let path =
            RelativePath::new(PathBuf::from(OsString::from_vec(path))).map_err(|_| invalid())?;
        let mut count = [0];
        cursor.read_exact(&mut count)?;
        if !(1..=2).contains(&count[0]) {
            return Err(invalid());
        }
        let mut candidates = Vec::with_capacity(2);
        for _ in 0..count[0] {
            let mut key = [0; 32];
            cursor.read_exact(&mut key)?;
            candidates.push(native_object::NativeObjectKey(key));
        }
        let mut tag = [0];
        cursor.read_exact(&mut tag)?;
        let additional = match tag[0] {
            0 => SourceExclusion::Objects,
            1 | 2 => {
                let mut object = [0; 32];
                cursor.read_exact(&mut object)?;
                let object = native_object::NativeObjectKey(object);
                if !candidates.contains(&object) {
                    return Err(invalid());
                }
                if tag[0] == 1 {
                    SourceExclusion::OperatorLinks(object)
                } else {
                    SourceExclusion::DirectoryAncestors(object)
                }
            }
            _ => return Err(invalid()),
        };
        let mut scope = [0; 32];
        cursor.read_exact(&mut scope)?;
        if cursor.position() != bytes.len() as u64 {
            return Err(invalid());
        }
        Ok(Self {
            root,
            path,
            kind,
            snapshot: Snapshot {
                candidates,
                additional,
                scope,
            },
        })
    }
}

pub(in crate::rooted_fs) struct Declarations {
    tape: File,
    index: AuthenticatedIndex<48>,
    end: u64,
    pub(in crate::rooted_fs) count: u64,
    domain: Option<[u8; 24]>,
    mixed_domain: bool,
    healthy: bool,
    pub(in crate::rooted_fs) refused: Option<u64>,
}

impl Declarations {
    pub(super) fn new() -> Result<Self> {
        Ok(Self {
            tape: tempfile::tempfile()?,
            index: AuthenticatedIndex::new()?,
            end: 0,
            count: 0,
            domain: None,
            mixed_domain: false,
            healthy: true,
            refused: None,
        })
    }

    // Scratch failure is sticky even if its bytes/extent are later restored.
    fn begin(&mut self) -> Result<()> {
        if !self.healthy {
            return Err(invalid());
        }
        self.healthy = false;
        if self.tape.metadata()?.len() != self.end {
            return Err(invalid());
        }
        Ok(())
    }

    pub(super) fn append(&mut self, record: Record) -> Result<()> {
        self.begin()?;
        let bytes = record.encode()?;
        for candidate in record.snapshot.candidates {
            let domain: [u8; 24] = candidate.0[..24].try_into().map_err(|_| invalid())?;
            match self.domain {
                None => self.domain = Some(domain),
                Some(expected) => self.mixed_domain |= domain != expected,
            }
            let key = index_key(b"candidate", &candidate.0);
            if self.index.get_bytes(key)?.is_none() {
                let mut state = [0; 48];
                state[..8].copy_from_slice(&self.count.to_le_bytes());
                self.index.set_bytes(key, state)?;
            }
        }
        let next = self
            .end
            .checked_add(bytes.len() as u64)
            .ok_or_else(invalid)?;
        let mut locator = [0; 48];
        locator[..8].copy_from_slice(&self.end.to_le_bytes());
        locator[8..16].copy_from_slice(&(bytes.len() as u64).to_le_bytes());
        locator[16..].copy_from_slice(&record_digest(self.count, &bytes));
        self.tape.seek(SeekFrom::Start(self.end))?;
        self.tape.write_all(&bytes)?;
        self.index
            .set_bytes(index_key(b"record", &self.count.to_le_bytes()), locator)?;
        self.count = self.count.checked_add(1).ok_or_else(invalid)?;
        self.end = next;
        self.healthy = true;
        Ok(())
    }

    pub(super) fn read(&mut self, ordinal: u64) -> Result<Record> {
        self.begin()?;
        if ordinal >= self.count {
            return Err(invalid());
        }
        let locator = self
            .index
            .get_bytes(index_key(b"record", &ordinal.to_le_bytes()))?
            .ok_or_else(invalid)?;
        let offset = u64::from_le_bytes(locator[..8].try_into().map_err(|_| invalid())?);
        let length = u64::from_le_bytes(locator[8..16].try_into().map_err(|_| invalid())?);
        if length > MAX_RECORD as u64 || offset.checked_add(length).is_none_or(|end| end > self.end)
        {
            return Err(invalid());
        }
        let mut bytes = vec![0; length as usize];
        self.tape.seek(SeekFrom::Start(offset))?;
        self.tape.read_exact(&mut bytes)?;
        if record_digest(ordinal, &bytes) != locator[16..] {
            return Err(invalid());
        }
        let record = Record::decode(&bytes)?;
        for candidate in &record.snapshot.candidates {
            let state = self
                .index
                .get_bytes(index_key(b"candidate", &candidate.0))?
                .ok_or_else(invalid)?;
            let first = u64::from_le_bytes(state[..8].try_into().map_err(|_| invalid())?);
            if state[8..] != [0; 40] || first > ordinal {
                return Err(invalid());
            }
        }
        self.healthy = true;
        Ok(record)
    }

    pub(in crate::rooted_fs) fn exclusion(&mut self, ordinal: u64) -> Result<SourceExclusion> {
        Ok(self.read(ordinal)?.snapshot.additional)
    }

    pub(in crate::rooted_fs) fn independent(
        &mut self,
        source: native_object::NativeObjectKey,
    ) -> Result<bool> {
        self.begin()?;
        let member = self.index.get_bytes(index_key(b"candidate", &source.0))?;
        let ordinal = match member {
            Some(state) => {
                let ordinal = u64::from_le_bytes(state[..8].try_into().map_err(|_| invalid())?);
                if state[8..] != [0; 40] || ordinal >= self.count {
                    return Err(invalid());
                }
                Some(ordinal)
            }
            None if self.mixed_domain
                || self
                    .domain
                    .as_ref()
                    .is_none_or(|domain| *domain != source.0[..24]) =>
            {
                Some(0)
            }
            None => None,
        };
        if let Some(ordinal) = ordinal {
            self.refused = Some(self.refused.map_or(ordinal, |old| old.min(ordinal)));
        }
        self.healthy = true;
        Ok(ordinal.is_none())
    }
}
