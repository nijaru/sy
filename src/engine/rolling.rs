/// Rsync-style weak rolling checksum used only as a fast candidate filter.
///
/// Both sums intentionally wrap modulo 2^16. A strong BLAKE3 signature must
/// verify every weak-checksum match before bytes are reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WeakChecksum {
    low: u16,
    high: u16,
    block_len_mod: u16,
}

impl WeakChecksum {
    pub fn hash(bytes: &[u8]) -> u32 {
        let mut low = 0_u16;
        let mut high = 0_u16;
        for &byte in bytes {
            low = low.wrapping_add(signed_byte(byte));
            high = high.wrapping_add(low);
        }
        u32::from(low) | (u32::from(high) << 16)
    }

    pub fn from_block(bytes: &[u8]) -> Self {
        let digest = Self::hash(bytes);
        Self {
            low: digest as u16,
            high: (digest >> 16) as u16,
            // The recurrence is modulo 2^16, so only the low 16 bits of the
            // window length contribute to the outgoing-byte term.
            block_len_mod: bytes.len() as u16,
        }
    }

    pub fn roll(&mut self, outgoing: u8, incoming: u8) {
        let outgoing = signed_byte(outgoing);
        let incoming = signed_byte(incoming);
        self.low = self.low.wrapping_sub(outgoing).wrapping_add(incoming);
        self.high = self
            .high
            .wrapping_sub(self.block_len_mod.wrapping_mul(outgoing))
            .wrapping_add(self.low);
    }

    pub fn digest(self) -> u32 {
        u32::from(self.low) | (u32::from(self.high) << 16)
    }
}

#[inline]
fn signed_byte(byte: u8) -> u16 {
    (byte as i8 as i16) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rolling_matches_static_at_every_offset() {
        let data = b"The quick brown fox jumps over the lazy dog";
        let block_len = 8;
        let mut rolling = WeakChecksum::from_block(&data[..block_len]);
        assert_eq!(rolling.digest(), WeakChecksum::hash(&data[..block_len]));

        for offset in 1..=data.len() - block_len {
            rolling.roll(data[offset - 1], data[offset + block_len - 1]);
            assert_eq!(
                rolling.digest(),
                WeakChecksum::hash(&data[offset..offset + block_len])
            );
        }
    }

    #[test]
    fn bytes_above_127_follow_rsync_signed_byte_semantics() {
        let data = [0x80, 0xff, 0x01, 0x7f];
        let expected_low = (-128_i32 - 1 + 1 + 127) as u16;
        let expected_high = (-128_i32 - 129 - 128 - 1) as u16;
        assert_eq!(
            WeakChecksum::hash(&data),
            u32::from(expected_low) | (u32::from(expected_high) << 16)
        );
    }

    #[test]
    fn large_window_length_wraps_only_in_the_recurrence_modulus() {
        let mut data = vec![0_u8; 64 * 1024 + 3];
        for (index, byte) in data.iter_mut().enumerate() {
            *byte = (index % 251) as u8;
        }
        let block_len = 64 * 1024;
        let mut rolling = WeakChecksum::from_block(&data[..block_len]);
        rolling.roll(data[0], data[block_len]);
        assert_eq!(rolling.digest(), WeakChecksum::hash(&data[1..=block_len]));
    }
}
