//! SOURCE-unlink continuation payload in the shared authenticated radix.
use crate::authenticated_index::AuthenticatedIndex;
use crate::rooted_fs::HardlinkSourceState;
use std::io;
#[cfg(test)]
mod tests;

pub(super) type StateIndex = AuthenticatedIndex<{ HardlinkSourceState::ENCODED_BYTES }>;
#[cfg(test)]
pub(super) const STATE_RECORD_BYTES: usize = 33 + HardlinkSourceState::ENCODED_BYTES;

impl StateIndex {
    pub(crate) fn get(&mut self, key: [u8; 32]) -> io::Result<Option<HardlinkSourceState>> {
        self.get_bytes(key)?
            .map(|bytes| HardlinkSourceState::decode(&bytes))
            .transpose()
    }

    pub(crate) fn set(&mut self, key: [u8; 32], state: HardlinkSourceState) -> io::Result<()> {
        self.set_bytes(key, state.encode())
    }
}
