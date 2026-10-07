use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use std::io::Cursor;
use std::time::Duration;
use sy::engine::rolling::WeakChecksum;
use sy::transfer::delta::{match_delta, BasisBlock, BasisIndex, BasisIndexLimits, DeltaOp};

fn corpus(size: usize, mut seed: u64) -> Vec<u8> {
    (0..size)
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed.to_le_bytes()[0]
        })
        .collect()
}

fn basis(bytes: &[u8], block_size: u32) -> BasisIndex {
    BasisIndex::new(
        block_size,
        bytes
            .chunks(block_size as usize)
            .enumerate()
            .map(|(index, chunk)| {
                let mut strong = [0; 16];
                strong.copy_from_slice(&blake3::hash(chunk).as_bytes()[..16]);
                BasisBlock {
                    index: index as u64,
                    size: chunk.len() as u32,
                    weak: WeakChecksum::hash(chunk),
                    strong,
                }
            }),
        BasisIndexLimits::default(),
    )
    .unwrap()
}

fn verify_case(old: &[u8], source: &[u8], index: &BasisIndex) {
    let mut reconstructed = Vec::new();
    let summary = match_delta(Cursor::new(source), index, |op| {
        match op {
            DeltaOp::Copy {
                basis_offset: offset,
                len,
            } => {
                reconstructed
                    .extend_from_slice(&old[offset as usize..offset as usize + len as usize]);
            }
            DeltaOp::Literal(bytes) => reconstructed.extend_from_slice(&bytes),
        }
        Ok(())
    })
    .unwrap();
    assert_eq!(reconstructed, source);
    assert_eq!(summary.source_digest, *blake3::hash(source).as_bytes());
}

// Measure the actual streaming SSH matcher, not local CLI copies (which use
// native whole/sparse/reflink strategies). Signature creation is setup work.
fn streaming_delta(c: &mut Criterion) {
    let mut group = c.benchmark_group("streaming_delta");
    group.sample_size(10);
    group.warm_up_time(Duration::from_millis(200));
    group.measurement_time(Duration::from_secs(1));

    for size in [1024 * 1024, 8 * 1024 * 1024] {
        let old = corpus(size, 0x1234_5678);
        let mut prepend = corpus(4096, 0xabcd_ef01);
        prepend.extend_from_slice(&old);
        let mut edits = old.clone();
        for byte in edits.iter_mut().step_by(100) {
            *byte ^= 0x5a;
        }
        let rewrite = corpus(size, 0x7654_3210);
        for block_size in [4096, 64 * 1024, 1024 * 1024] {
            let index = basis(&old, block_size);
            for (case, source) in [
                ("unchanged", old.as_slice()),
                ("prepend", prepend.as_slice()),
                ("one_percent_edits", edits.as_slice()),
                ("rewrite", rewrite.as_slice()),
            ] {
                verify_case(&old, source, &index);
                group.throughput(Throughput::Bytes(source.len() as u64));
                group.bench_with_input(
                    BenchmarkId::new(case, format!("{size}B-{block_size}block")),
                    &source,
                    |b, source| {
                        b.iter(|| {
                            black_box(
                                match_delta(Cursor::new(black_box(*source)), &index, |op| {
                                    black_box(op);
                                    Ok(())
                                })
                                .unwrap(),
                            );
                        });
                    },
                );
            }
        }
    }

    // These constant windows share the 16-bit rolling checksum at 64 KiB,
    // but not the strong signature. This exposes overlapping-hash amplification.
    let old = vec![0; 128 * 1024];
    let source = vec![2; old.len()];
    let index = basis(&old, 64 * 1024);
    verify_case(&old, &source, &index);
    group.throughput(Throughput::Bytes(source.len() as u64));
    group.bench_function("weak_collision/128KiB-64KiBblock", |b| {
        b.iter(|| {
            black_box(
                match_delta(Cursor::new(black_box(source.as_slice())), &index, |op| {
                    black_box(op);
                    Ok(())
                })
                .unwrap(),
            );
        });
    });
    group.finish();
}

criterion_group!(benches, streaming_delta);
criterion_main!(benches);
