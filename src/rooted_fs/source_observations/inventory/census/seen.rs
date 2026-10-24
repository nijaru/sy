//! Fresh duplicate/exhaustion bookkeeping for one native census, not a cached
//! SOURCE proof. Ordinals must first be authenticated against the original key.
use super::{invalid, AuthenticatedIndex};
use std::io;

// At most 1 KiB of resident membership bits; larger censuses retain exact,
// authenticated disk-backed tracking instead of allocating with tree size.
const RESIDENT_ORDINALS: u64 = 8192;

enum Entries {
    Resident(Box<[u64]>),
    Disk(AuthenticatedIndex<8>),
}

pub(super) struct SeenOriginals {
    count: u64,
    entries: Entries,
}

impl SeenOriginals {
    pub(super) fn new(count: u64) -> io::Result<Self> {
        let non_root = count.checked_sub(1).ok_or_else(invalid)?;
        let entries = if non_root <= RESIDENT_ORDINALS {
            let words = usize::try_from(non_root.div_ceil(64)).map_err(|_| invalid())?;
            Entries::Resident(vec![0; words].into_boxed_slice())
        } else {
            Entries::Disk(AuthenticatedIndex::new()?)
        };
        Ok(Self { count, entries })
    }

    fn check_ordinal(&self, ordinal: u64) -> io::Result<()> {
        if ordinal == 0 || ordinal >= self.count {
            return Err(invalid());
        }
        Ok(())
    }

    pub(super) fn insert(&mut self, key: [u8; 32], ordinal: u64) -> io::Result<bool> {
        self.check_ordinal(ordinal)?;
        match &mut self.entries {
            Entries::Resident(bits) => {
                let position = usize::try_from(ordinal - 1).map_err(|_| invalid())?;
                let word = bits.get_mut(position / 64).ok_or_else(invalid)?;
                let mask = 1 << (position % 64);
                let fresh = *word & mask == 0;
                *word |= mask;
                Ok(fresh)
            }
            Entries::Disk(index) => {
                if index.get_bytes(key)?.is_some() {
                    return Ok(false);
                }
                index.set_bytes(key, ordinal.to_le_bytes())?;
                Ok(true)
            }
        }
    }

    pub(super) fn contains(&mut self, key: [u8; 32], ordinal: u64) -> io::Result<bool> {
        self.check_ordinal(ordinal)?;
        match &mut self.entries {
            Entries::Resident(bits) => {
                let position = usize::try_from(ordinal - 1).map_err(|_| invalid())?;
                let word = bits.get(position / 64).ok_or_else(invalid)?;
                Ok(word & (1 << (position % 64)) != 0)
            }
            Entries::Disk(index) => Ok(index.get_bytes(key)? == Some(ordinal.to_le_bytes())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(ordinal: u64) -> [u8; 32] {
        *blake3::hash(&ordinal.to_le_bytes()).as_bytes()
    }

    #[test]
    fn exact_membership_at_word_and_spill_boundaries() {
        assert!(SeenOriginals::new(0).is_err());
        for count in [
            1,
            2,
            64,
            65,
            66,
            RESIDENT_ORDINALS + 1,
            RESIDENT_ORDINALS + 2,
            u64::MAX,
        ] {
            let mut seen = SeenOriginals::new(count).unwrap();
            assert!(seen.insert(key(0), 0).is_err());
            assert!(seen.contains(key(count), count).is_err());
            let mut ordinals = vec![1, 63, 64, 65, count - 1];
            ordinals.retain(|ordinal| *ordinal > 0 && *ordinal < count);
            ordinals.sort_unstable();
            ordinals.dedup();
            for &ordinal in ordinals.iter().rev() {
                assert!(!seen.contains(key(ordinal), ordinal).unwrap());
                assert!(seen.insert(key(ordinal), ordinal).unwrap());
                assert!(seen.contains(key(ordinal), ordinal).unwrap());
                assert!(!seen.insert(key(ordinal), ordinal).unwrap());
            }
            for &ordinal in &ordinals {
                assert!(seen.contains(key(ordinal), ordinal).unwrap());
            }
            // A new census never inherits a previous pass's evidence.
            let mut fresh = SeenOriginals::new(count).unwrap();
            for ordinal in ordinals {
                assert!(!fresh.contains(key(ordinal), ordinal).unwrap());
            }
        }
    }
}
