use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use std::fs;
use std::io::{Seek, SeekFrom, Write};
use std::process::Command;
use tempfile::TempDir;

fn create_sparse_file(path: &std::path::Path, size_mb: usize, modification_offset_mb: usize) {
    let mut file = fs::File::create(path).unwrap();

    // Create sparse file by seeking
    file.seek(SeekFrom::Start((size_mb * 1024 * 1024) as u64 - 1)).unwrap();
    file.write_all(&[0]).unwrap();
    file.flush().unwrap();

    // Write some data at the beginning
    file.seek(SeekFrom::Start(0)).unwrap();
    file.write_all(b"HEADER DATA AT START").unwrap();

    // Write modification at specific offset (this will trigger delta sync)
    file.seek(SeekFrom::Start((modification_offset_mb * 1024 * 1024) as u64)).unwrap();
    file.write_all(b"MODIFIED DATA HERE").unwrap();
    file.flush().unwrap();
}

fn bench_delta_sync_small_change(c: &mut Criterion) {
    let mut group = c.benchmark_group("delta_sync_small_change");
    group.sample_size(10);

    for size_mb in [10, 50, 100].iter() {
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{}MB", size_mb)),
            size_mb,
            |b, &size_mb| {
                b.iter(|| {
                    let source = TempDir::new().unwrap();
                    let dest = TempDir::new().unwrap();

                    Command::new("git")
                        .args(["init"])
                        .current_dir(source.path())
                        .output()
                        .unwrap();

                    // Create initial file
                    create_sparse_file(&source.path().join("large.bin"), size_mb, 0);

                    // First sync - full copy
                    Command::new(env!("CARGO_BIN_EXE_sy"))
                        .args([
                            source.path().to_str().unwrap(),
                            dest.path().to_str().unwrap(),
                        ])
                        .output()
                        .unwrap();

                    // Modify file slightly (1MB into the file)
                    create_sparse_file(&source.path().join("large.bin"), size_mb, 1);

                    // Second sync - should use delta sync
                    let output = Command::new(env!("CARGO_BIN_EXE_sy"))
                        .args([
                            source.path().to_str().unwrap(),
                            dest.path().to_str().unwrap(),
                        ])
                        .output()
                        .unwrap();

                    assert!(output.status.success());
                    black_box(output);
                });
            },
        );
    }
    group.finish();
}

fn bench_delta_sync_vs_full_copy(c: &mut Criterion) {
    let mut group = c.benchmark_group("delta_vs_full_50MB");
    group.sample_size(10);

    let size_mb = 50;

    // Setup: Create modified file for delta sync
    let source_delta = TempDir::new().unwrap();
    let dest_delta = TempDir::new().unwrap();

    Command::new("git")
        .args(["init"])
        .current_dir(source_delta.path())
        .output()
        .unwrap();

    create_sparse_file(&source_delta.path().join("file.bin"), size_mb, 0);

    // Initial sync
    Command::new(env!("CARGO_BIN_EXE_sy"))
        .args([
            source_delta.path().to_str().unwrap(),
            dest_delta.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    // Modify file
    create_sparse_file(&source_delta.path().join("file.bin"), size_mb, 1);

    // Benchmark delta sync (update)
    group.bench_function("delta_sync", |b| {
        b.iter(|| {
            let output = Command::new(env!("CARGO_BIN_EXE_sy"))
                .args([
                    source_delta.path().to_str().unwrap(),
                    dest_delta.path().to_str().unwrap(),
                ])
                .output()
                .unwrap();

            assert!(output.status.success());

            // Restore original for next iteration
            create_sparse_file(&source_delta.path().join("file.bin"), size_mb, 0);
            Command::new(env!("CARGO_BIN_EXE_sy"))
                .args([
                    source_delta.path().to_str().unwrap(),
                    dest_delta.path().to_str().unwrap(),
                ])
                .output()
                .unwrap();
            create_sparse_file(&source_delta.path().join("file.bin"), size_mb, 1);

            black_box(output);
        });
    });

    // Setup for full copy benchmark
    let source_full = TempDir::new().unwrap();
    Command::new("git")
        .args(["init"])
        .current_dir(source_full.path())
        .output()
        .unwrap();
    create_sparse_file(&source_full.path().join("file.bin"), size_mb, 1);

    // Benchmark full copy (create)
    group.bench_function("full_copy", |b| {
        b.iter(|| {
            let dest = TempDir::new().unwrap();
            let output = Command::new(env!("CARGO_BIN_EXE_sy"))
                .args([
                    source_full.path().to_str().unwrap(),
                    dest.path().to_str().unwrap(),
                ])
                .output()
                .unwrap();

            assert!(output.status.success());
            black_box(output);
        });
    });

    group.finish();
}

fn bench_delta_sync_large_file(c: &mut Criterion) {
    let mut group = c.benchmark_group("delta_sync_1GB");
    group.sample_size(10);

    group.bench_function("1GB_file_small_change", |b| {
        let source = TempDir::new().unwrap();
        let dest = TempDir::new().unwrap();

        Command::new("git")
            .args(["init"])
            .current_dir(source.path())
            .output()
            .unwrap();

        // Create 1GB sparse file
        create_sparse_file(&source.path().join("huge.bin"), 1024, 0);

        // Initial sync
        Command::new(env!("CARGO_BIN_EXE_sy"))
            .args([
                source.path().to_str().unwrap(),
                dest.path().to_str().unwrap(),
            ])
            .output()
            .unwrap();

        // Modify file at 100MB offset
        create_sparse_file(&source.path().join("huge.bin"), 1024, 100);

        b.iter(|| {
            let output = Command::new(env!("CARGO_BIN_EXE_sy"))
                .args([
                    source.path().to_str().unwrap(),
                    dest.path().to_str().unwrap(),
                ])
                .output()
                .unwrap();

            assert!(output.status.success());

            // Restore for next iteration
            create_sparse_file(&source.path().join("huge.bin"), 1024, 0);
            Command::new(env!("CARGO_BIN_EXE_sy"))
                .args([
                    source.path().to_str().unwrap(),
                    dest.path().to_str().unwrap(),
                ])
                .output()
                .unwrap();
            create_sparse_file(&source.path().join("huge.bin"), 1024, 100);

            black_box(output);
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_delta_sync_small_change,
    bench_delta_sync_vs_full_copy,
    bench_delta_sync_large_file
);
criterion_main!(benches);
