from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    file = Path(path)
    text = file.read_text()
    if new in text:
        return
    if old not in text:
        raise SystemExit(f"required edit anchor missing in {path}")
    file.write_text(text.replace(old, new, 1))


engine_mod = Path("src/engine/mod.rs")
text = engine_mod.read_text()
if "pub mod rolling;" not in text:
    text = text.replace("pub mod reconcile;\n", "pub mod reconcile;\npub mod rolling;\n", 1)
    engine_mod.write_text(text)

Path("src/engine/rolling.rs").write_text('''/// Rsync-style weak rolling checksum used only as a fast candidate filter.
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

    pub const fn digest(self) -> u32 {
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
        assert_eq!(
            rolling.digest(),
            WeakChecksum::hash(&data[1..=block_len])
        );
    }
}
''')

signature = Path("src/remote/signature.rs")
signature.write_text(
    signature.read_text().replace(
        "crate::delta::Adler32::hash", "crate::engine::rolling::WeakChecksum::hash"
    )
)

replace_once(
    "src/remote/mod.rs",
    '''    let capabilities = endpoint_capabilities(&Capabilities::local())
        | CapabilitySet::BLAKE3
        | CapabilitySet::RAW_PATHS
        | CapabilitySet::MULTIPLEXING;

    // Rolling-signature basis reads are advertised only where RootedFs can
    // enforce held-directory-FD confinement for every peer-controlled path.
    #[cfg(unix)]
    {
        capabilities | CapabilitySet::ROLLING_SIGNATURES
    }
    #[cfg(not(unix))]
    {
        capabilities
    }
}
''',
    '''    let mut capabilities = endpoint_capabilities(&Capabilities::local())
        | CapabilitySet::BLAKE3
        | CapabilitySet::RAW_PATHS
        | CapabilitySet::MULTIPLEXING;

    // Rolling-signature basis reads are advertised only where RootedFs can
    // enforce held-directory-FD confinement and the v3 path encoding has been
    // validated. Linux and macOS are the supported cross-OS family for 0.5.
    if supports_rolling_signatures(Platform::current().os) {
        capabilities.insert(CapabilitySet::ROLLING_SIGNATURES);
    }
    capabilities
}

const fn supports_rolling_signatures(os: PlatformOs) -> bool {
    matches!(os, PlatformOs::Linux | PlatformOs::Macos)
}
''',
)

replace_once(
    "src/remote/mod.rs",
    '''            cfg!(unix)
        );
        assert!(!client.ready.capabilities.contains(CapabilitySet::REFLINK));
    }

    #[tokio::test]
    async fn push_session_creates_missing_root() {
''',
    '''            supports_rolling_signatures(Platform::current().os)
        );
        assert!(!client.ready.capabilities.contains(CapabilitySet::REFLINK));
    }

    #[test]
    fn rolling_signatures_are_scoped_to_tested_os_family() {
        assert!(supports_rolling_signatures(PlatformOs::Linux));
        assert!(supports_rolling_signatures(PlatformOs::Macos));
        assert!(!supports_rolling_signatures(PlatformOs::Windows));
        assert!(!supports_rolling_signatures(PlatformOs::Other(4)));
    }

    #[tokio::test]
    async fn push_session_creates_missing_root() {
''',
)

replace_once(
    "src/remote/path.rs",
    '''mod tests {
    use super::*;

    #[test]
    fn relative_path_round_trip_preserves_components() {
''',
    '''mod tests {
    use super::*;

    #[test]
    fn linux_and_macos_path_encodings_are_cross_compatible() {
        assert!(compatible_path_encoding(PlatformOs::Linux, PlatformOs::Linux));
        assert!(compatible_path_encoding(PlatformOs::Macos, PlatformOs::Macos));
        assert!(compatible_path_encoding(PlatformOs::Linux, PlatformOs::Macos));
        assert!(compatible_path_encoding(PlatformOs::Macos, PlatformOs::Linux));
        assert!(!compatible_path_encoding(PlatformOs::Linux, PlatformOs::Windows));
        assert!(!compatible_path_encoding(PlatformOs::Macos, PlatformOs::Other(4)));
    }

    #[test]
    fn relative_path_round_trip_preserves_components() {
''',
)
