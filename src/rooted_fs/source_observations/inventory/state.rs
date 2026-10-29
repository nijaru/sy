//! One authenticated state record per original, not separate radix lookups for
//! its inverse ordinal, child count and directory disposition. Native evidence
//! and traversal epochs retain their own lifetimes and are never cached here.
use super::{invalid, state_key, Catalogue, Result, SourceDirectoryDisposition};

#[derive(Clone, Copy)]
pub(in crate::rooted_fs::source_observations) struct OriginalState {
    pub ordinal: u64,
    pub children: u64,
    pub directory: SourceDirectoryDisposition,
}

impl OriginalState {
    pub(in crate::rooted_fs::source_observations) fn new(ordinal: u64) -> Self {
        Self {
            ordinal,
            children: 0,
            directory: SourceDirectoryDisposition::Opaque,
        }
    }

    pub(in crate::rooted_fs::source_observations) fn encode(self) -> [u8; 48] {
        let mut bytes = [0; 48];
        bytes[..8].copy_from_slice(&self.ordinal.to_le_bytes());
        bytes[8..16].copy_from_slice(&self.children.to_le_bytes());
        bytes[16] = match self.directory {
            SourceDirectoryDisposition::Opaque => 0,
            SourceDirectoryDisposition::Traversed => 1,
            SourceDirectoryDisposition::Protected => 2,
        };
        bytes
    }

    pub(in crate::rooted_fs::source_observations) fn decode(bytes: [u8; 48]) -> Result<Self> {
        if bytes[17..] != [0; 31] {
            return Err(invalid().into());
        }
        Ok(Self {
            ordinal: u64::from_le_bytes(bytes[..8].try_into().map_err(|_| invalid())?),
            children: u64::from_le_bytes(bytes[8..16].try_into().map_err(|_| invalid())?),
            directory: match bytes[16] {
                0 => SourceDirectoryDisposition::Opaque,
                1 => SourceDirectoryDisposition::Traversed,
                2 => SourceDirectoryDisposition::Protected,
                _ => return Err(invalid().into()),
            },
        })
    }
}

impl Catalogue {
    pub(in crate::rooted_fs::source_observations) fn original_state(
        &mut self,
        key: [u8; 32],
    ) -> Result<OriginalState> {
        OriginalState::decode(
            self.index
                .get_bytes(state_key(b"original", key))?
                .ok_or_else(invalid)?,
        )
    }

    pub(in crate::rooted_fs::source_observations) fn set_original_state(
        &mut self,
        key: [u8; 32],
        state: OriginalState,
    ) -> Result<()> {
        self.index
            .set_bytes(state_key(b"original", key), state.encode())?;
        Ok(())
    }
}
