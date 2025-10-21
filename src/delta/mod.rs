pub mod applier;
pub mod checksum;
pub mod generator;
pub mod ratio;
pub mod rolling;

// Delta sync functions for remote sync (not used for local sync which uses block comparison)
#[allow(unused_imports)]
pub use applier::apply_delta;
#[allow(unused_imports)]
pub use checksum::{compute_checksums, BlockChecksum};
#[allow(unused_imports)]
pub use generator::{generate_delta, generate_delta_streaming, Delta, DeltaOp};
#[allow(unused_imports)]
pub use ratio::{estimate_change_ratio, ChangeRatioResult};
pub use rolling::Adler32;

/// Default block size calculation: sqrt(filesize)
/// Capped between 512 bytes and 128KB
pub fn calculate_block_size(file_size: u64) -> usize {
    let size = (file_size as f64).sqrt() as usize;
    size.clamp(512, 128 * 1024)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_block_size_calculation() {
        assert_eq!(calculate_block_size(1024), 512); // Min size
        assert_eq!(calculate_block_size(1_000_000), 1000); // sqrt(1M) = 1000
        assert_eq!(calculate_block_size(100_000_000), 10000); // sqrt(100M) = 10000
        assert_eq!(calculate_block_size(100_000_000_000), 128 * 1024); // Capped at 128KB
    }
}
