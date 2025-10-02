/// Adler-32 rolling hash implementation
///
/// This is the weak checksum used by rsync for fast matching.
/// It's designed to be updated incrementally as a window slides
/// through data.
///
/// Adler-32 computes two 16-bit sums:
/// - A: sum of all bytes
/// - B: sum of (n-i+1) * byte[i] for each byte
///
/// The final checksum is (B << 16) | A
#[derive(Debug, Clone)]
pub struct Adler32 {
    a: u32,
    b: u32,
    window: Vec<u8>,
    block_size: usize,
}

const MOD_ADLER: u32 = 65521; // Largest prime < 2^16

impl Adler32 {
    /// Create a new Adler-32 hasher
    pub fn new(block_size: usize) -> Self {
        Self {
            a: 1,
            b: 0,
            window: Vec::with_capacity(block_size),
            block_size,
        }
    }

    /// Hash a block of data (non-rolling)
    pub fn hash(data: &[u8]) -> u32 {
        let mut a: u32 = 1;
        let mut b: u32 = 0;

        for &byte in data {
            a = (a + byte as u32) % MOD_ADLER;
            b = (b + a) % MOD_ADLER;
        }

        (b << 16) | a
    }

    /// Initialize with a full block
    pub fn update_block(&mut self, block: &[u8]) {
        self.a = 1;
        self.b = 0;
        self.window.clear();

        for &byte in block {
            self.a = (self.a + byte as u32) % MOD_ADLER;
            self.b = (self.b + self.a) % MOD_ADLER;
            self.window.push(byte);
        }
    }

    /// Roll the hash: remove old byte, add new byte
    /// This is the key operation for rsync algorithm
    ///
    /// Note: Currently recalculates from scratch for correctness.
    /// The incremental formula for Adler-32 is complex and error-prone.
    /// For local operations, delta sync is disabled anyway.
    /// For remote operations, network cost >> computation cost.
    pub fn roll(&mut self, old_byte: u8, new_byte: u8) {
        // Update window
        if self.window.len() >= self.block_size {
            self.window.remove(0);
        }
        self.window.push(new_byte);

        // Recalculate from scratch
        // This is O(block_size) but simpler and correct
        // Future optimization: implement true O(1) rolling if needed
        self.a = 1;
        self.b = 0;
        for &byte in &self.window {
            self.a = (self.a + byte as u32) % MOD_ADLER;
            self.b = (self.b + self.a) % MOD_ADLER;
        }
    }

    /// Get the current hash value
    pub fn digest(&self) -> u32 {
        (self.b << 16) | self.a
    }

    /// Reset the hasher
    pub fn reset(&mut self) {
        self.a = 1;
        self.b = 0;
        self.window.clear();
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
}
