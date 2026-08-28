use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

#[derive(Debug, Clone, Copy)]
enum Candidate {
    Zstd(i32),
    Lz4,
}

impl Candidate {
    fn name(self) -> String {
        match self {
            Self::Zstd(level) => format!("zstd-{level}"),
            Self::Lz4 => "lz4".to_string(),
        }
    }

    fn compress(self, data: &[u8]) -> Vec<u8> {
        match self {
            Self::Zstd(level) => zstd::bulk::compress(data, level).unwrap(),
            Self::Lz4 => lz4_flex::compress_prepend_size(data),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Corpus {
    SourceText,
    Repetitive,
    Mixed,
    HighEntropy,
}

impl Corpus {
    const ALL: [Self; 4] = [
        Self::SourceText,
        Self::Repetitive,
        Self::Mixed,
        Self::HighEntropy,
    ];

    const fn name(self) -> &'static str {
        match self {
            Self::SourceText => "source-text",
            Self::Repetitive => "repetitive",
            Self::Mixed => "mixed",
            Self::HighEntropy => "high-entropy",
        }
    }

    fn generate(self, size: usize) -> Vec<u8> {
        match self {
            Self::SourceText => repeat_to_size(
                b"fn reconcile(path: &RelativePath) -> Result<SyncOp> {\n    planner.plan(path)?;\n}\n",
                size,
            ),
            Self::Repetitive => vec![b'A'; size],
            Self::Mixed => {
                let mut data = repeat_to_size(
                    b"2026-08-28T13:00:00Z INFO sync path=/srv/data status=changed bytes=65536\n",
                    size,
                );
                let entropy = high_entropy(size / 4, 0x4d59_5df4_d0f3_3173);
                let start = size.saturating_sub(entropy.len()) / 2;
                data[start..start + entropy.len()].copy_from_slice(&entropy);
                data
            }
            Self::HighEntropy => high_entropy(size, 0x9e37_79b9_7f4a_7c15),
        }
    }
}

fn repeat_to_size(pattern: &[u8], size: usize) -> Vec<u8> {
    let mut result = Vec::with_capacity(size);
    while result.len() < size {
        let remaining = size - result.len();
        result.extend_from_slice(&pattern[..pattern.len().min(remaining)]);
    }
    result
}

/// Deterministic xorshift64* output. Unlike the previous `i % 256` corpus,
/// this produces high-entropy bytes that are a useful proxy for encrypted or
/// already-compressed payloads without adding a benchmark-only RNG dependency.
fn high_entropy(size: usize, mut state: u64) -> Vec<u8> {
    let mut result = Vec::with_capacity(size);
    while result.len() < size {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        let word = state.wrapping_mul(0x2545_f491_4f6c_dd1d).to_le_bytes();
        let remaining = size - result.len();
        result.extend_from_slice(&word[..word.len().min(remaining)]);
    }
    result
}

fn compression_candidates() -> [Candidate; 6] {
    [
        Candidate::Zstd(-5),
        Candidate::Zstd(-3),
        Candidate::Zstd(-1),
        Candidate::Zstd(1),
        Candidate::Zstd(3),
        Candidate::Lz4,
    ]
}

fn bench_v3_chunk_compression(c: &mut Criterion) {
    // Protocol v3 compresses bounded literal/data payloads, not whole files.
    // Measure representative frame/chunk sizes rather than a single 10 MiB
    // whole-file allocation from the legacy protocol.
    for size in [64 * 1024, 256 * 1024, 1024 * 1024] {
        for corpus in Corpus::ALL {
            let data = corpus.generate(size);
            let mut group = c.benchmark_group(format!("compress/{}/{}", corpus.name(), size));
            group.sample_size(20);
            group.throughput(Throughput::Bytes(size as u64));

            for candidate in compression_candidates() {
                let compressed = candidate.compress(&data);
                let ratio = compressed.len() as f64 / data.len() as f64;
                let id = BenchmarkId::new(candidate.name(), format!("ratio={ratio:.3}"));
                group.bench_function(id, |b| {
                    b.iter(|| candidate.compress(black_box(&data)));
                });
            }
            group.finish();
        }
    }
}

fn bench_v3_chunk_decompression(c: &mut Criterion) {
    let size = 1024 * 1024;
    for corpus in [Corpus::SourceText, Corpus::Mixed, Corpus::HighEntropy] {
        let data = corpus.generate(size);
        let mut group = c.benchmark_group(format!("decompress/{}", corpus.name()));
        group.sample_size(20);
        group.throughput(Throughput::Bytes(size as u64));

        for level in [-5, -3, -1, 1, 3] {
            let encoded = zstd::bulk::compress(&data, level).unwrap();
            let ratio = encoded.len() as f64 / data.len() as f64;
            group.bench_function(
                BenchmarkId::new(format!("zstd-{level}"), format!("ratio={ratio:.3}")),
                |b| {
                    b.iter(|| {
                        zstd::bulk::decompress(black_box(&encoded), size).unwrap()
                    });
                },
            );
        }

        let encoded = lz4_flex::compress_prepend_size(&data);
        let ratio = encoded.len() as f64 / data.len() as f64;
        group.bench_function(BenchmarkId::new("lz4", format!("ratio={ratio:.3}")), |b| {
            b.iter(|| lz4_flex::decompress_size_prepended(black_box(&encoded)).unwrap());
        });
        group.finish();
    }
}

criterion_group!(benches, bench_v3_chunk_compression, bench_v3_chunk_decompression);
criterion_main!(benches);
