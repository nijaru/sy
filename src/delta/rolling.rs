/// Adler-32 rolling hash implementation
///
/// This is the weak checksum used by rsync for fast matching.
/// It's designed to be updated incrementally as a window slides
/// through data.
///
/// Adler-32 computes two 16-bit sums:
/// - A: sum of all bytes
/// - B: sum of (n-i+1) * byte\[i\] for each byte
///
/// The final checksum is (B << 16) | A
///
/// Note: We don't maintain the window in memory since the hash state
/// (a, b) is sufficient for O(1) rolling updates.
#[derive(Debug, Clone)]
pub struct Adler32 {
    a: u32,
    b: u32,
    block_size: usize,
    // Precomputed table for (block_size * byte) % MOD_ADLER
    // Index is the byte value [0..255]
    n_mod_table: [u32; 256],
}

const MOD_ADLER: u32 = 65521; // Largest prime < 2^16

impl Adler32 {
    /// Create a new Adler-32 hasher
    pub fn new(block_size: usize) -> Self {
        // Precompute lookup table for (block_size * i) % MOD_ADLER
        let mut n_mod_table = [0u32; 256];
        let n = block_size as u32;
        for i in 0..256 {
            n_mod_table[i] = (n * (i as u32)) % MOD_ADLER;
        }

        Self {
            a: 1,
            b: 0,
            block_size,
            n_mod_table,
        }
    }

    /// Hash a block of data (non-rolling)
    pub fn hash(data: &[u8]) -> u32 {
        let mut a: u32 = 1;
        let mut b: u32 = 0;

        // Process in chunks to defer modulo
        // N=5552: largest n such that 255 * n * (n+1) / 2 + (n+1) * (MOD_ADLER-1) <= 2^32 - 1
        // Safe value is 3800 to be conservative with B accumulation starting from non-zero
        const CHUNK_SIZE: usize = 3800;

        for chunk in data.chunks(CHUNK_SIZE) {
            for &byte in chunk {
                a += byte as u32;
                b += a;
            }
            a %= MOD_ADLER;
            b %= MOD_ADLER;
        }

        (b << 16) | a
    }

    /// Initialize with a full block
    pub fn update_block(&mut self, block: &[u8]) {
        self.a = 1;
        self.b = 0;

        const CHUNK_SIZE: usize = 3800;

        for chunk in block.chunks(CHUNK_SIZE) {
            for &byte in chunk {
                self.a += byte as u32;
                self.b += self.a;
            }
            self.a %= MOD_ADLER;
            self.b %= MOD_ADLER;
        }
    }

    /// Roll the hash: remove old byte, add new byte
    /// This is the key operation for rsync algorithm
    ///
    /// Optimized implementation using lookup table and conditional subtraction
    /// to avoid expensive division/modulo operations.
    #[inline]
    pub fn roll(&mut self, old_byte: u8, new_byte: u8) {
        let old = old_byte as u32;
        let new = new_byte as u32;

        // Update A: remove old byte, add new byte
        // Formula: A = (A - old + new) % M
        // Logic:
        // 1. A + new can overflow 65521 slightly (max 65520 + 255 = 65775)
        // 2. Subtract old.
        // 3. If negative (wrapped), add MOD_ADLER.

        let mut a = self.a + new;
        if old > a {
            // Handle wrap-around conceptually (a - old < 0)
            a += MOD_ADLER;
        }
        a -= old;
        if a >= MOD_ADLER {
            a -= MOD_ADLER;
        }
        self.a = a;

        // Update B: remove contribution of old byte across all positions
        // Formula: B = (B - n*old + A - 1) % M
        //
        // 1. B + A - 1. Max ~131000.
        // 2. Subtract (n*old)%M from lookup table.
        // 3. Normalize.

        let n_old_mod = unsafe { *self.n_mod_table.get_unchecked(old_byte as usize) };

        let mut b = self.b + self.a;
        // b + a can be up to ~131040.
        // We want to subtract (n_old_mod + 1).
        // n_old_mod is < 65521. 1 is 1.
        // So we subtract up to 65522.

        let sub = n_old_mod + 1;
        if sub > b {
            b += MOD_ADLER * 2; // Add enough multiples to ensure positivity
        }
        b -= sub;

        // Now b is positive. Normalize.
        // Since we added at most 2*M, and original sum was ~2*M, result is ~4*M max.
        // Fast modulo by subtraction loop (at most 3-4 iterations)
        while b >= MOD_ADLER {
            b -= MOD_ADLER;
        }
        self.b = b;
    }

    /// Get the current hash value
    pub fn digest(&self) -> u32 {
        (self.b << 16) | self.a
    }

    /// Reset the hasher
    #[allow(dead_code)]
    pub fn reset(&mut self) {
        self.a = 1;
        self.b = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_adler32_basic() {
        let data = b"hello world";
        let hash = Adler32::hash(data);
        assert_ne!(hash, 0);
        assert_ne!(hash, 1);
    }

    #[test]
    fn test_adler32_deterministic() {
        let data = b"test data 123";
        assert_eq!(Adler32::hash(data), Adler32::hash(data));
    }

    #[test]
    fn test_adler32_rolling() {
        let data = b"abcdefghijklmnop";
        let block_size = 4;

        // Hash first block statically
        let mut hasher = Adler32::new(block_size);
        hasher.update_block(&data[0..4]); // "abcd"
        let hash1 = hasher.digest();

        // Roll to next block
        hasher.roll(data[0], data[4]); // Remove 'a', add 'e'
        let hash2 = hasher.digest();

        // Verify rolling matches static hash
        let expected = Adler32::hash(&data[1..5]); // "bcde"
        assert_eq!(hash2, expected);

        // Hashes should be different
        assert_ne!(hash1, hash2);
    }

    #[test]
    fn test_adler32_rolling_correctness() {
        // Test that rolling hash matches static hash for entire sequence
        let data = b"The quick brown fox jumps over the lazy dog";
        let block_size = 8;

        let mut hasher = Adler32::new(block_size);
        hasher.update_block(&data[0..block_size]);

        for i in 1..=(data.len() - block_size) {
            // Roll to next position
            hasher.roll(data[i - 1], data[i + block_size - 1]);

            // Verify against static hash
            let expected = Adler32::hash(&data[i..i + block_size]);
            assert_eq!(
                hasher.digest(),
                expected,
                "Rolling hash mismatch at position {}",
                i
            );
        }
    }

    #[test]
    fn test_adler32_different_data() {
        assert_ne!(Adler32::hash(b"abc"), Adler32::hash(b"def"));
        assert_ne!(Adler32::hash(b"test"), Adler32::hash(b"TEST"));
    }

    #[test]
    fn test_adler32_empty() {
        let hash = Adler32::hash(b"");
        assert_eq!(hash, 1); // Adler-32 of empty data is 1
    }

    #[test]
    fn test_adler32_rolling_large_block() {
        // Test with large block size (128KB)
        let block_size = 128 * 1024;
        let mut data = vec![0u8; block_size * 2];
        for (i, byte) in data.iter_mut().enumerate() {
            *byte = (i % 256) as u8;
        }

        let mut hasher = Adler32::new(block_size);
        hasher.update_block(&data[0..block_size]);

        // Roll forward
        hasher.roll(data[0], data[block_size]);
        let expected = Adler32::hash(&data[1..block_size + 1]);
        assert_eq!(hasher.digest(), expected);
    }

    #[test]
    fn test_adler32_rolling_all_zeros() {
        // Edge case: all zeros
        let data = [0u8; 100];
        let block_size = 10;

        let mut hasher = Adler32::new(block_size);
        hasher.update_block(&data[0..block_size]);

        for i in 1..=(data.len() - block_size) {
            hasher.roll(data[i - 1], data[i + block_size - 1]);
            let expected = Adler32::hash(&data[i..i + block_size]);
            assert_eq!(hasher.digest(), expected);
        }
    }

    #[test]
    fn test_adler32_rolling_all_ones() {
        // Edge case: all 0xFF
        let data = [0xFF; 100];
        let block_size = 16;

        let mut hasher = Adler32::new(block_size);
        hasher.update_block(&data[0..block_size]);

        for i in 1..=(data.len() - block_size) {
            hasher.roll(data[i - 1], data[i + block_size - 1]);
            let expected = Adler32::hash(&data[i..i + block_size]);
            assert_eq!(hasher.digest(), expected);
        }
    }

    #[test]
    fn test_adler32_rolling_repeating_pattern() {
        // Test with repeating pattern
        let pattern = b"ABCD";
        let mut data = Vec::with_capacity(100 * pattern.len());
        for _ in 0..100 {
            data.extend_from_slice(pattern);
        }
        let block_size = 32;

        let mut hasher = Adler32::new(block_size);
        hasher.update_block(&data[0..block_size]);

        for i in 1..=(data.len() - block_size) {
            hasher.roll(data[i - 1], data[i + block_size - 1]);
            let expected = Adler32::hash(&data[i..i + block_size]);
            assert_eq!(
                hasher.digest(),
                expected,
                "Mismatch at position {} with repeating pattern",
                i
            );
        }
    }

    #[test]
    fn test_adler32_rolling_modulo_boundary() {
        // Test near MOD_ADLER boundary
        // Create data that will push checksums close to MOD_ADLER
        let block_size = 256;
        let data = vec![0xFF; block_size * 3];

        let mut hasher = Adler32::new(block_size);
        hasher.update_block(&data[0..block_size]);

        for i in 1..block_size {
            hasher.roll(data[i - 1], data[i + block_size - 1]);
            let expected = Adler32::hash(&data[i..i + block_size]);
            assert_eq!(
                hasher.digest(),
                expected,
                "Modulo boundary test failed at position {}",
                i
            );
        }
    }
}
