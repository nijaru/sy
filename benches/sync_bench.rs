use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use std::fs;
use std::process::Command;
use tempfile::TempDir;

fn setup_files(dir: &TempDir, count: usize) {
    Command::new("git")
        .args(["init"])
        .current_dir(dir.path())
        .output()
        .unwrap();

    for i in 0..count {
        fs::write(
            dir.path().join(format!("file_{}.txt", i)),
            format!("content_{}", i),
        )
        .unwrap();
    }
}

fn bench_sync_small_files(c: &mut Criterion) {
    let mut group = c.benchmark_group("sync_small_files");

    for file_count in [10, 50, 100, 500].iter() {
        group.bench_with_input(
            BenchmarkId::from_parameter(file_count),
            file_count,
            |b, &count| {
                b.iter(|| {
                    let source = TempDir::new().unwrap();
                    let dest = TempDir::new().unwrap();
                    setup_files(&source, count);

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

fn bench_sync_nested_dirs(c: &mut Criterion) {
    let mut group = c.benchmark_group("sync_nested_dirs");

    for depth in [5, 10, 20].iter() {
        group.bench_with_input(BenchmarkId::from_parameter(depth), depth, |b, &depth| {
            b.iter(|| {
                let source = TempDir::new().unwrap();
                let dest = TempDir::new().unwrap();

                Command::new("git")
                    .args(["init"])
                    .current_dir(source.path())
                    .output()
                    .unwrap();

                let mut path = source.path().to_path_buf();
                for i in 0..depth {
                    path = path.join(format!("level_{}", i));
                }
                fs::create_dir_all(&path).unwrap();
                fs::write(path.join("file.txt"), "content").unwrap();

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
        });
    }
    group.finish();
}

fn bench_sync_large_files(c: &mut Criterion) {
    let mut group = c.benchmark_group("sync_large_files");
    group.sample_size(10); // Fewer samples for large files

    for size_mb in [1, 5, 10].iter() {
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

                    let content = "x".repeat(size_mb * 1024 * 1024);
                    fs::write(source.path().join("large.txt"), &content).unwrap();

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

fn bench_sync_idempotent(c: &mut Criterion) {
    c.bench_function("sync_idempotent_100_files", |b| {
        let source = TempDir::new().unwrap();
        let dest = TempDir::new().unwrap();
        setup_files(&source, 100);

        // First sync
        Command::new(env!("CARGO_BIN_EXE_sy"))
            .args([
                source.path().to_str().unwrap(),
                dest.path().to_str().unwrap(),
            ])
            .output()
            .unwrap();

        b.iter(|| {
            // Subsequent syncs (should be fast - all skipped)
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
    });
}

fn bench_cache_full_vs_incremental(c: &mut Criterion) {
    let mut group = c.benchmark_group("cache_comparison");

    for file_count in [100, 500, 1000].iter() {
        // Benchmark without cache (full scan every time)
        group.bench_with_input(
            BenchmarkId::new("full_scan", file_count),
            file_count,
            |b, &count| {
                let source = TempDir::new().unwrap();
                let dest = TempDir::new().unwrap();
                setup_files(&source, count);

                // First sync to set up dest
                Command::new(env!("CARGO_BIN_EXE_sy"))
                    .args([
                        source.path().to_str().unwrap(),
                        dest.path().to_str().unwrap(),
                    ])
                    .output()
                    .unwrap();

                b.iter(|| {
                    let output = Command::new(env!("CARGO_BIN_EXE_sy"))
                        .args([
                            source.path().to_str().unwrap(),
                            dest.path().to_str().unwrap(),
                            "--use-cache=false",
                        ])
                        .output()
                        .unwrap();

                    assert!(output.status.success());
                    black_box(output);
                });
            },
        );

        // Benchmark with cache (incremental scan)
        group.bench_with_input(
            BenchmarkId::new("incremental_scan", file_count),
            file_count,
            |b, &count| {
                let source = TempDir::new().unwrap();
                let dest = TempDir::new().unwrap();
                setup_files(&source, count);

                // First sync with cache enabled
                Command::new(env!("CARGO_BIN_EXE_sy"))
                    .args([
                        source.path().to_str().unwrap(),
                        dest.path().to_str().unwrap(),
                        "--use-cache=true",
                    ])
                    .output()
                    .unwrap();

                b.iter(|| {
                    let output = Command::new(env!("CARGO_BIN_EXE_sy"))
                        .args([
                            source.path().to_str().unwrap(),
                            dest.path().to_str().unwrap(),
                            "--use-cache=true",
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

fn bench_cache_nested_directories(c: &mut Criterion) {
    let mut group = c.benchmark_group("cache_nested_dirs");

    for depth in [10, 20, 50].iter() {
        // Without cache
        group.bench_with_input(BenchmarkId::new("full_scan", depth), depth, |b, &depth| {
            let source = TempDir::new().unwrap();
            let dest = TempDir::new().unwrap();

            Command::new("git")
                .args(["init"])
                .current_dir(source.path())
                .output()
                .unwrap();

            let mut path = source.path().to_path_buf();
            for i in 0..depth {
                path = path.join(format!("level_{}", i));
            }
            fs::create_dir_all(&path).unwrap();
            fs::write(path.join("file.txt"), "content").unwrap();

            // First sync
            Command::new(env!("CARGO_BIN_EXE_sy"))
                .args([
                    source.path().to_str().unwrap(),
                    dest.path().to_str().unwrap(),
                ])
                .output()
                .unwrap();

            b.iter(|| {
                let output = Command::new(env!("CARGO_BIN_EXE_sy"))
                    .args([
                        source.path().to_str().unwrap(),
                        dest.path().to_str().unwrap(),
                        "--use-cache=false",
                    ])
                    .output()
                    .unwrap();

                assert!(output.status.success());
                black_box(output);
            });
        });

        // With cache
        group.bench_with_input(
            BenchmarkId::new("incremental_scan", depth),
            depth,
            |b, &depth| {
                let source = TempDir::new().unwrap();
                let dest = TempDir::new().unwrap();

                Command::new("git")
                    .args(["init"])
                    .current_dir(source.path())
                    .output()
                    .unwrap();

                let mut path = source.path().to_path_buf();
                for i in 0..depth {
                    path = path.join(format!("level_{}", i));
                }
                fs::create_dir_all(&path).unwrap();
                fs::write(path.join("file.txt"), "content").unwrap();

                // First sync with cache
                Command::new(env!("CARGO_BIN_EXE_sy"))
                    .args([
                        source.path().to_str().unwrap(),
                        dest.path().to_str().unwrap(),
                        "--use-cache=true",
                    ])
                    .output()
                    .unwrap();

                b.iter(|| {
                    let output = Command::new(env!("CARGO_BIN_EXE_sy"))
                        .args([
                            source.path().to_str().unwrap(),
                            dest.path().to_str().unwrap(),
                            "--use-cache=true",
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

criterion_group!(
    benches,
    bench_sync_small_files,
    bench_sync_nested_dirs,
    bench_sync_large_files,
    bench_sync_idempotent,
    bench_cache_full_vs_incremental,
    bench_cache_nested_directories
);
criterion_main!(benches);
