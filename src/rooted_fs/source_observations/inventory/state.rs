//! Original state belongs to its canonical entry, alongside the observation
//! locator. Native evidence and traversal epochs retain separate lifetimes.
use super::{invalid, Catalogue, IndexRecord, Result, SourceDirectoryDisposition};

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

    pub(in crate::rooted_fs::source_observations) fn encode(self) -> [u8; 17] {
        let mut bytes = [0; 17];
        bytes[..8].copy_from_slice(&self.ordinal.to_le_bytes());
        bytes[8..16].copy_from_slice(&self.children.to_le_bytes());
        bytes[16] = match self.directory {
            SourceDirectoryDisposition::Opaque => 0,
            SourceDirectoryDisposition::Traversed => 1,
            SourceDirectoryDisposition::Protected => 2,
        };
        bytes
    }

    pub(in crate::rooted_fs::source_observations) fn decode(bytes: [u8; 17]) -> Result<Self> {
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
        self.entry(key)?
            .and_then(|entry| entry.original)
            .ok_or_else(|| invalid().into())
    }

    pub(in crate::rooted_fs::source_observations) fn set_original_state(
        &mut self,
        key: [u8; 32],
        state: OriginalState,
    ) -> Result<()> {
        let mut entry = self.entry(key)?.ok_or_else(invalid)?;
        let original = entry.original.ok_or_else(invalid)?;
        if original.ordinal != state.ordinal {
            return Err(invalid().into());
        }
        entry.original = Some(state);
        self.set_index_record(key, IndexRecord::Entry(entry))
    }
}
