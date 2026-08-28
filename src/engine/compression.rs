use std::num::{NonZeroU32, NonZeroU64};
use std::time::Duration;

/// Initial zstd profile selected by the v3 chunk benchmark.
///
/// This is an implementation profile, not a user-facing quality setting.
/// `auto` may still choose no compression when that is faster end to end.
pub const ZSTD_FAST_LEVEL: i32 = -5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressionChoice {
    None,
    ZstdFast,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRate(NonZeroU64);

impl ByteRate {
    pub const fn new(bytes_per_second: u64) -> Option<Self> {
        match NonZeroU64::new(bytes_per_second) {
            Some(rate) => Some(Self(rate)),
            None => None,
        }
    }

    pub const fn get(self) -> u64 {
        self.0.get()
    }

    fn duration_for(self, bytes: u64) -> Duration {
        if bytes == 0 {
            return Duration::ZERO;
        }

        let rate = self.get();
        let seconds = bytes / rate;
        let remainder = bytes % rate;
        let nanos = u128::from(remainder)
            .saturating_mul(1_000_000_000)
            .div_ceil(u128::from(rate));

        if nanos == 1_000_000_000 {
            Duration::from_secs(seconds.saturating_add(1))
        } else {
            Duration::new(seconds, nanos as u32)
        }
    }
}

/// Measured compressibility of representative bytes from the payload.
///
/// The sample is deliberately ratio-only. Extension, file type, and other
/// hints may decide whether sampling is worthwhile, but they are not equality
/// or performance authorities once real measurements are available.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompressionSample {
    original_bytes: NonZeroU64,
    compressed_bytes: u64,
}

impl CompressionSample {
    pub const fn new(original_bytes: u64, compressed_bytes: u64) -> Option<Self> {
        match NonZeroU64::new(original_bytes) {
            Some(original_bytes) => Some(Self {
                original_bytes,
                compressed_bytes,
            }),
            None => None,
        }
    }

    pub const fn original_bytes(self) -> u64 {
        self.original_bytes.get()
    }

    pub const fn compressed_bytes(self) -> u64 {
        self.compressed_bytes
    }

    fn estimate_compressed_bytes(self, original_bytes: u64) -> u64 {
        let numerator = u128::from(original_bytes) * u128::from(self.compressed_bytes);
        let estimate = numerator.div_ceil(u128::from(self.original_bytes.get()));
        u64::try_from(estimate).unwrap_or(u64::MAX)
    }
}

/// Runtime rates used by `compression=auto` to minimize estimated wall time.
///
/// Encode/decode rates are expressed in original payload bytes per second.
/// The model assumes v3 chunk streaming: the first chunk incurs pipeline
/// startup, while later encode, wire, and decode work can overlap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompressionTiming {
    link: ByteRate,
    encode: ByteRate,
    decode: ByteRate,
    chunk_bytes: NonZeroU32,
}

impl CompressionTiming {
    pub const fn new(
        link: ByteRate,
        encode: ByteRate,
        decode: ByteRate,
        chunk_bytes: NonZeroU32,
    ) -> Self {
        Self {
            link,
            encode,
            decode,
            chunk_bytes,
        }
    }

    pub const fn link(self) -> ByteRate {
        self.link
    }

    pub const fn encode(self) -> ByteRate {
        self.encode
    }

    pub const fn decode(self) -> ByteRate {
        self.decode
    }

    pub const fn chunk_bytes(self) -> u32 {
        self.chunk_bytes.get()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompressionEstimate {
    pub uncompressed: Duration,
    pub zstd_fast: Duration,
}

/// Estimate end-to-end transfer time for the two initial v3 choices.
///
/// This is intentionally a wall-clock model. Compression ratio, codec speed,
/// link speed, and chunk size are inputs only because they affect elapsed time.
/// CPU/power/bandwidth consumption are not competing default objectives; the
/// scheduler may still enforce resource budgets independently.
pub fn estimate_transfer_time(
    payload_bytes: u64,
    sample: CompressionSample,
    timing: CompressionTiming,
) -> CompressionEstimate {
    let uncompressed = timing.link.duration_for(payload_bytes);
    if payload_bytes == 0 {
        return CompressionEstimate {
            uncompressed,
            zstd_fast: Duration::ZERO,
        };
    }

    let first_bytes = payload_bytes.min(u64::from(timing.chunk_bytes.get()));
    let first_compressed = sample.estimate_compressed_bytes(first_bytes);
    let startup = timing.encode.duration_for(first_bytes)
        + timing.link.duration_for(first_compressed)
        + timing.decode.duration_for(first_bytes);

    let remaining_bytes = payload_bytes - first_bytes;
    let remaining_compressed = sample.estimate_compressed_bytes(remaining_bytes);
    let steady_state = timing
        .encode
        .duration_for(remaining_bytes)
        .max(timing.link.duration_for(remaining_compressed))
        .max(timing.decode.duration_for(remaining_bytes));

    CompressionEstimate {
        uncompressed,
        zstd_fast: startup + steady_state,
    }
}

/// Choose the representation predicted to complete sooner.
///
/// Equal estimates deliberately choose `None`: compression should not spend
/// work unless the timing model predicts an actual wall-clock win.
pub fn choose_for_min_elapsed(
    payload_bytes: u64,
    sample: CompressionSample,
    timing: CompressionTiming,
) -> CompressionChoice {
    if sample.compressed_bytes() >= sample.original_bytes() {
        return CompressionChoice::None;
    }

    let estimate = estimate_transfer_time(payload_bytes, sample, timing);
    if estimate.zstd_fast < estimate.uncompressed {
        CompressionChoice::ZstdFast
    } else {
        CompressionChoice::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rate(bytes_per_second: u64) -> ByteRate {
        ByteRate::new(bytes_per_second).unwrap()
    }

    fn timing(link: u64, encode: u64, decode: u64) -> CompressionTiming {
        CompressionTiming::new(
            rate(link),
            rate(encode),
            rate(decode),
            NonZeroU32::new(1024 * 1024).unwrap(),
        )
    }

    #[test]
    fn slow_link_prefers_compression_when_it_reduces_elapsed_time() {
        let payload = 100 * 1024 * 1024;
        let sample = CompressionSample::new(1024, 512).unwrap();
        let timing = timing(100 * 1024 * 1024, 1024 * 1024 * 1024, 1024 * 1024 * 1024);

        let estimate = estimate_transfer_time(payload, sample, timing);
        assert!(estimate.zstd_fast < estimate.uncompressed);
        assert_eq!(
            choose_for_min_elapsed(payload, sample, timing),
            CompressionChoice::ZstdFast
        );
    }

    #[test]
    fn fast_link_prefers_raw_when_codec_work_is_the_bottleneck() {
        let payload = 100 * 1024 * 1024;
        let sample = CompressionSample::new(1024, 512).unwrap();
        let timing = timing(
            10 * 1024 * 1024 * 1024,
            1024 * 1024 * 1024,
            1024 * 1024 * 1024,
        );

        let estimate = estimate_transfer_time(payload, sample, timing);
        assert!(estimate.uncompressed < estimate.zstd_fast);
        assert_eq!(
            choose_for_min_elapsed(payload, sample, timing),
            CompressionChoice::None
        );
    }

    #[test]
    fn incompressible_sample_never_spends_codec_time() {
        let payload = 100 * 1024 * 1024;
        let sample = CompressionSample::new(1024, 1024).unwrap();
        let timing = timing(10 * 1024 * 1024, 1024 * 1024 * 1024, 1024 * 1024 * 1024);

        assert_eq!(
            choose_for_min_elapsed(payload, sample, timing),
            CompressionChoice::None
        );
    }

    #[test]
    fn one_chunk_includes_encode_wire_and_decode_latency() {
        let payload = 1024 * 1024;
        let sample = CompressionSample::new(1024, 512).unwrap();
        let timing = timing(100 * 1024 * 1024, 1024 * 1024 * 1024, 1024 * 1024 * 1024);

        let estimate = estimate_transfer_time(payload, sample, timing);
        assert_eq!(estimate.uncompressed, Duration::from_millis(10));
        assert!(estimate.zstd_fast < estimate.uncompressed);
    }

    #[test]
    fn empty_payload_has_no_compression_win() {
        let sample = CompressionSample::new(1024, 512).unwrap();
        let timing = timing(100, 100, 100);

        let estimate = estimate_transfer_time(0, sample, timing);
        assert_eq!(estimate.uncompressed, Duration::ZERO);
        assert_eq!(estimate.zstd_fast, Duration::ZERO);
        assert_eq!(
            choose_for_min_elapsed(0, sample, timing),
            CompressionChoice::None
        );
    }
}