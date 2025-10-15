use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use std::io::Write;
use sy::compress::{compress, decompress, Compression};

fn generate_test_data(size: usize, pattern: &str) -> Vec<u8> {
    match pattern {
        "text" => {
            // Realistic text data (log files, source code)
            "The quick brown fox jumps over the lazy dog. "
                .repeat(size / 46)
                .as_bytes()
                .to_vec()
        }
        "repetitive" => {
            // Highly compressible data
            vec![b'A'; size]
        }
        "random" => {
            // Incompressible data (simulates already-compressed files)
            (0..size).map(|i| (i % 256) as u8).collect()
        }
        _ => vec![0u8; size],
    }
}

fn compress_zstd_level(data: &[u8], level: i32) -> std::io::Result<Vec<u8>> {
    let mut encoder = zstd::Encoder::new(Vec::new(), level)?;
    encoder.write_all(data)?;
    encoder.finish()
}

fn bench_compression_speed(c: &mut Criterion) {
    let mut group = c.benchmark_group("compression_speed");
    group.sample_size(20); // Fewer samples for speed

    // Test only 10MB size for quick results
    let size = 10 * 1024 * 1024;

    // Test different data types
    for pattern in ["text", "random"].iter() {
        let data = generate_test_data(size, pattern);
        group.throughput(Throughput::Bytes(size as u64));

        // Benchmark Zstd compression (level 3)
        group.bench_with_input(
            BenchmarkId::new(format!("zstd_{}", pattern), size),
            &data,
            |b, data| {
                b.iter(|| compress(black_box(data), Compression::Zstd));
            },
        );
    }

    group.finish();
}

fn bench_decompression_speed(c: &mut Criterion) {
    let mut group = c.benchmark_group("decompression_speed");

    let size = 10 * 1024 * 1024; // 10 MB
    let text_data = generate_test_data(size, "text");

    // Pre-compress data
    let zstd_compressed = compress(&text_data, Compression::Zstd).unwrap();

    group.throughput(Throughput::Bytes(size as u64));

    group.bench_function("zstd_decompress", |b| {
        b.iter(|| decompress(black_box(&zstd_compressed), Compression::Zstd));
    });

    group.finish();
}

fn bench_compression_ratio(c: &mut Criterion) {
    let mut group = c.benchmark_group("compression_ratio");
    group.sample_size(10); // Fewer samples since we're measuring ratio, not speed

    let size = 10 * 1024 * 1024; // 10 MB

    for pattern in ["text", "repetitive", "random"].iter() {
        let data = generate_test_data(size, pattern);

        group.bench_with_input(
            BenchmarkId::new(format!("zstd_ratio_{}", pattern), pattern),
            &data,
            |b, data| {
                b.iter(|| {
                    let compressed = compress(black_box(data), Compression::Zstd).unwrap();
                    let ratio = compressed.len() as f64 / data.len() as f64;
                    black_box(ratio)
                });
            },
        );
    }

    group.finish();
}

fn bench_zstd_levels(c: &mut Criterion) {
    let mut group = c.benchmark_group("zstd_levels");
    group.sample_size(15); // Moderate sample size

    let size = 10 * 1024 * 1024; // 10 MB
    let text_data = generate_test_data(size, "text");
    group.throughput(Throughput::Bytes(size as u64));

    // Test various Zstd compression levels
    // Level 1: Fastest
    // Level 3: Default (balanced)
    // Level 6: Better compression
    // Level 9: High compression
    // Level 11: Very high compression
    // Level 15: Maximum practical
    // Level 19: Maximum (slow)
    for level in [1, 3, 6, 9, 11, 15, 19] {
        group.bench_with_input(BenchmarkId::new("zstd", level), &level, |b, &level| {
            b.iter(|| {
                let compressed = compress_zstd_level(black_box(&text_data), level).unwrap();
                // Also return ratio for comparison
                let ratio = compressed.len() as f64 / text_data.len() as f64;
                black_box((compressed, ratio))
            });
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_compression_speed,
    bench_decompression_speed,
    bench_compression_ratio,
    bench_zstd_levels
);
criterion_main!(benches);
