//! One live group's exact, ordered physical-address transcript.
#[cfg(test)]
#[path = "declarations_tests.rs"]
mod tests;
use crate::engine::disk_radix::DiskRadix;
use crate::protocol::{ProtocolError, WireGroupMember, WireGroupSeal};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};

#[derive(Debug, thiserror::Error)]
pub(crate) enum DeclarationError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
    #[error("invalid group declaration transcript: {0}")]
    Invalid(&'static str),
}

pub(super) struct Declarations {
    file: File,
    paths: DiskRadix,
    end: u64,
    count: u64,
    digest: blake3::Hasher,
    seal: Option<WireGroupSeal>,
    healthy: bool,
}

impl Declarations {
    pub(super) fn new() -> Result<Self, DeclarationError> {
        Ok(Self {
            file: tempfile::tempfile()?,
            paths: DiskRadix::new()?,
            end: 0,
            count: 0,
            digest: blake3::Hasher::new(),
            seal: None,
            healthy: true,
        })
    }

    pub(super) fn append(&mut self, member: &WireGroupMember) -> Result<(), DeclarationError> {
        self.check_extent()?;
        self.healthy = false;
        if self.seal.is_some() || member.ordinal != self.count {
            return Err(DeclarationError::Invalid("member ordinal or phase"));
        }
        // Hashes are commitment keys, not probabilistic membership filters.
        let key = *blake3::hash(member.path.as_encoded()).as_bytes();
        if self.paths.get(key)?.is_some() {
            return Err(DeclarationError::Invalid("duplicate physical path"));
        }
        let payload = member.encode();
        let length = u32::try_from(payload.len())
            .map_err(|_| DeclarationError::Invalid("record too large"))?;
        let record_digest = blake3::hash(&payload);
        self.file.seek(SeekFrom::Start(self.end))?;
        self.file.write_all(&length.to_be_bytes())?;
        self.file.write_all(&payload)?;
        self.file.write_all(record_digest.as_bytes())?;
        self.paths.insert(key, self.end)?;
        self.end = self
            .end
            .checked_add(4 + u64::from(length) + 32)
            .ok_or(DeclarationError::Invalid("record extent overflow"))?;
        self.count = self
            .count
            .checked_add(1)
            .ok_or(DeclarationError::Invalid("member count overflow"))?;
        self.digest.update(&length.to_be_bytes());
        self.digest.update(&payload);
        self.healthy = true;
        Ok(())
    }

    pub(super) fn seal(&mut self, supplied: WireGroupSeal) -> Result<(), DeclarationError> {
        self.check_extent()?;
        if self.seal.is_some()
            || self.count == 0
            || self.count != supplied.count
            || self.digest.finalize().as_bytes() != &supplied.digest
        {
            self.healthy = false;
            return Err(DeclarationError::Invalid("declaration seal mismatch"));
        }
        self.seal = Some(supplied);
        Ok(())
    }

    pub(super) fn cursor(&self) -> Result<Cursor, DeclarationError> {
        self.check_extent()?;
        Ok(Cursor {
            offset: 0,
            ordinal: 0,
            expected: self
                .seal
                .ok_or(DeclarationError::Invalid("unsealed declarations"))?,
            digest: blake3::Hasher::new(),
        })
    }

    pub(super) fn next(
        &mut self,
        cursor: &mut Cursor,
    ) -> Result<Option<WireGroupMember>, DeclarationError> {
        self.check_extent()?;
        if cursor.ordinal == cursor.expected.count {
            if cursor.offset != self.end
                || cursor.digest.finalize().as_bytes() != &cursor.expected.digest
            {
                self.healthy = false;
                return Err(DeclarationError::Invalid("replay seal mismatch"));
            }
            return Ok(None);
        }
        self.healthy = false;
        self.file.seek(SeekFrom::Start(cursor.offset))?;
        let mut raw_length = [0; 4];
        self.file.read_exact(&mut raw_length)?;
        let length = u32::from_be_bytes(raw_length) as usize;
        if length > crate::protocol::MAX_FRAME_PAYLOAD || length == 0 {
            return Err(DeclarationError::Invalid("record length"));
        }
        let next_offset = cursor
            .offset
            .checked_add(
                4 + u64::try_from(length)
                    .map_err(|_| DeclarationError::Invalid("record length"))?
                    + 32,
            )
            .ok_or(DeclarationError::Invalid("cursor overflow"))?;
        if next_offset > self.end {
            return Err(DeclarationError::Invalid("record beyond sealed extent"));
        }
        let mut payload = vec![0; length];
        self.file.read_exact(&mut payload)?;
        let mut digest = [0; 32];
        self.file.read_exact(&mut digest)?;
        if blake3::hash(&payload).as_bytes() != &digest {
            return Err(DeclarationError::Invalid("record digest"));
        }
        let member = WireGroupMember::decode(&payload)?;
        if member.ordinal != cursor.ordinal {
            return Err(DeclarationError::Invalid("replay ordinal"));
        }
        cursor.offset = next_offset;
        cursor.ordinal += 1;
        cursor.digest.update(&raw_length);
        cursor.digest.update(&payload);
        self.healthy = true;
        Ok(Some(member))
    }

    fn check_extent(&self) -> Result<(), DeclarationError> {
        if !self.healthy || self.file.metadata()?.len() != self.end {
            return Err(DeclarationError::Invalid(
                "unhealthy transcript or changed extent",
            ));
        }
        Ok(())
    }
}

pub(super) struct Cursor {
    offset: u64,
    ordinal: u64,
    expected: WireGroupSeal,
    digest: blake3::Hasher,
}

/// Clients use the same transcript framing without owning a second journal.
pub(crate) fn add_to_seal(
    hasher: &mut blake3::Hasher,
    member: &WireGroupMember,
) -> Result<(), ProtocolError> {
    let payload = member.encode();
    let length = u32::try_from(payload.len())
        .map_err(|_| ProtocolError::InvalidMessage("group member length"))?;
    hasher.update(&length.to_be_bytes());
    hasher.update(&payload);
    Ok(())
}
