use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use std::fs;
use sy::sync::scanner::Scanner;
use tempfile::TempDir;

/// Create a test directory structure with given parameters
fn create_test_structure(dir: &std::path::Path, num_subdirs: usize, files_per_dir: usize) {
    for i in 0..num_subdirs {
        let subdir = dir.join(format!("dir{:04}", i));
        fs::create_dir(&subdir).unwrap();
        for j in 0..files_per_dir {
            fs::write(subdir.join(format!("file{:04}.txt", j)), "content").unwrap();
        }
    }
}

/// Create flat directory with many files (tests threshold detection)
fn create_flat_structure(dir: &std::path::Path, num_files: usize) {
    for i in 0..num_files {
        fs::write(dir.join(format!("file{:05}.txt", i)), "content").unwrap();
    }
}

fn bench_scanner_sequential_vs_parallel(c: &mut Criterion) {
    let mut group = c.benchmark_group("scanner");
    group.sample_size(20);

    // Test different directory structures
    let configs = [
        (10, 100),  // 1,000 files - 10 subdirs (below threshold)
        (50, 100),  // 5,000 files - 50 subdirs
        (100, 100), // 10,000 files - 100 subdirs
        (200, 50),  // 10,000 files - many shallow dirs
    ];

    for (num_subdirs, files_per_dir) in configs {
        let total = num_subdirs * files_per_dir;
        let label = format!("{}f_{}d", total, num_subdirs);

        let temp = TempDir::new().unwrap();
        create_test_structure(temp.path(), num_subdirs, files_per_dir);

        // Benchmark sequential (explicit)
        group.bench_with_input(
            BenchmarkId::new("sequential", &label),
            &(&temp, num_subdirs, files_per_dir),
            |b, (temp, subdirs, files)| {
                b.iter(|| {
                    let scanner = Scanner::with_threads(temp.path(), 1);
                    let entries = scanner.scan().unwrap();
                    assert_eq!(entries.len(), subdirs + subdirs * files);
                    black_box(entries)
                });
            },
        );

        // Benchmark parallel_4 (explicit)
        group.bench_with_input(
            BenchmarkId::new("parallel_4", &label),
            &(&temp, num_subdirs, files_per_dir),
            |b, (temp, subdirs, files)| {
                b.iter(|| {
                    let scanner = Scanner::with_threads(temp.path(), 4);
                    let entries = scanner.scan().unwrap();
                    assert_eq!(entries.len(), subdirs + subdirs * files);
                    black_box(entries)
                });
            },
        );

        // Benchmark auto (dynamic selection)
        group.bench_with_input(
            BenchmarkId::new("auto", &label),
            &(&temp, num_subdirs, files_per_dir),
            |b, (temp, subdirs, files)| {
                b.iter(|| {
                    let scanner = Scanner::new(temp.path());
                    let entries = scanner.scan().unwrap();
                    assert_eq!(entries.len(), subdirs + subdirs * files);
                    black_box(entries)
                });
            },
        );
    }

    group.finish();
}

fn bench_scanner_threshold(c: &mut Criterion) {
    let mut group = c.benchmark_group("scanner_threshold");
    group.sample_size(20);

    // Test around the threshold (500 immediate children)
    // This verifies the dynamic selection works correctly
    let file_counts = [100, 300, 500, 700, 1000];

    for num_files in file_counts {
        let temp = TempDir::new().unwrap();
        create_flat_structure(temp.path(), num_files);

        let label = format!("{}_files", num_files);

        // Sequential (explicit)
        group.bench_with_input(
            BenchmarkId::new("seq", &label),
            &(&temp, num_files),
            |b, (temp, files)| {
                b.iter(|| {
                    let scanner = Scanner::with_threads(temp.path(), 1);
                    let entries = scanner.scan().unwrap();
                    assert_eq!(entries.len(), *files);
                    black_box(entries)
                });
            },
        );

        // Auto (should choose based on threshold)
        group.bench_with_input(
            BenchmarkId::new("auto", &label),
            &(&temp, num_files),
            |b, (temp, files)| {
                b.iter(|| {
                    let scanner = Scanner::new(temp.path());
                    let entries = scanner.scan().unwrap();
                    assert_eq!(entries.len(), *files);
                    black_box(entries)
                });
            },
        );

        // Parallel (explicit, for comparison)
        group.bench_with_input(
            BenchmarkId::new("par4", &label),
            &(&temp, num_files),
            |b, (temp, files)| {
                b.iter(|| {
                    let scanner = Scanner::with_threads(temp.path(), 4);
                    let entries = scanner.scan().unwrap();
                    assert_eq!(entries.len(), *files);
                    black_box(entries)
                });
            },
        );
    }

    group.finish();
}

fn bench_scanner_scaling(c: &mut Criterion) {
    let mut group = c.benchmark_group("scanner_scaling");
    group.sample_size(10);

    // Test scaling with increasing subdirectory count (where parallelism helps most)
    let subdir_counts = [10, 25, 50, 100, 200];
    let files_per_dir = 50;

    for num_subdirs in subdir_counts {
        let temp = TempDir::new().unwrap();
        create_test_structure(temp.path(), num_subdirs, files_per_dir);

        let label = format!("{}_subdirs", num_subdirs);

        group.bench_with_input(
            BenchmarkId::new("seq", &label),
            &(&temp, num_subdirs),
            |b, (temp, subdirs)| {
                b.iter(|| {
                    let scanner = Scanner::with_threads(temp.path(), 1);
                    let entries = scanner.scan().unwrap();
                    assert_eq!(entries.len(), subdirs + subdirs * files_per_dir);
                    black_box(entries)
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("par", &label),
            &(&temp, num_subdirs),
            |b, (temp, subdirs)| {
                b.iter(|| {
                    let scanner = Scanner::new(temp.path());
                    let entries = scanner.scan().unwrap();
                    assert_eq!(entries.len(), subdirs + subdirs * files_per_dir);
                    black_box(entries)
                });
            },
        );
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_scanner_sequential_vs_parallel,
    bench_scanner_threshold,
    bench_scanner_scaling
);
criterion_main!(benches);
