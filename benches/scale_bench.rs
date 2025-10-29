use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;
use sy::sync::scanner::FileEntry;
use sy::sync::strategy::StrategyPlanner;
use tempfile::TempDir;

fn bench_deletion_planning(c: &mut Criterion) {
    let mut group = c.benchmark_group("deletion_planning");

    // Test different file counts to see threshold behavior
    for file_count in [1_000, 5_000, 10_000, 20_000, 50_000].iter() {
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{}_files", file_count)),
            file_count,
            |b, &count| {
                // Setup: Create temp dest with 100 files to delete
                let temp_dest = TempDir::new().unwrap();
                for i in 0..100 {
                    fs::write(temp_dest.path().join(format!("delete{}.txt", i)), "delete").unwrap();
                }

                // Create source file list (doesn't include delete*.txt files)
                let source_files: Vec<FileEntry> = (0..count)
                    .map(|i| FileEntry {
                        path: Arc::new(PathBuf::from(format!("/source/file{}.txt", i))),
                        relative_path: Arc::new(PathBuf::from(format!("file{}.txt", i))),
                        size: 100,
                        modified: SystemTime::now(),
                        is_dir: false,
                        is_symlink: false,
                        symlink_target: None,
                        is_sparse: false,
                        allocated_size: 100,
                        xattrs: None,
                        inode: None,
                        nlink: 1,
                        acls: None,
                        bsd_flags: None,
                    })
                    .collect();

                let planner = StrategyPlanner::new();

                b.iter(|| {
                    let deletions =
                        planner.plan_deletions(black_box(&source_files), temp_dest.path());
                    assert_eq!(deletions.len(), 100);
                });
            },
        );
    }

    group.finish();
}

fn bench_bloom_filter_memory(c: &mut Criterion) {
    use sy::sync::scale::FileSetBloom;

    let mut group = c.benchmark_group("bloom_filter_memory");

    for file_count in [10_000, 100_000, 1_000_000].iter() {
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{}_files", file_count)),
            file_count,
            |b, &count| {
                let paths: Vec<PathBuf> = (0..count)
                    .map(|i| PathBuf::from(format!("file{}.txt", i)))
                    .collect();

                b.iter(|| {
                    let mut bloom = FileSetBloom::new(count);
                    for path in &paths {
                        bloom.insert(black_box(path));
                    }

                    // Verify lookups work
                    assert!(bloom.contains(&paths[0]));
                    assert!(bloom.contains(&paths[count / 2]));
                    assert!(bloom.contains(&paths[count - 1]));

                    black_box(bloom)
                });
            },
        );
    }

    group.finish();
}

fn bench_bloom_filter_lookup(c: &mut Criterion) {
    use sy::sync::scale::FileSetBloom;

    let mut group = c.benchmark_group("bloom_filter_lookup");

    for file_count in [10_000, 100_000, 1_000_000].iter() {
        let paths: Vec<PathBuf> = (0..*file_count)
            .map(|i| PathBuf::from(format!("file{}.txt", i)))
            .collect();

        let mut bloom = FileSetBloom::new(*file_count);
        for path in &paths {
            bloom.insert(path);
        }

        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{}_files", file_count)),
            file_count,
            |b, _count| {
                let lookup_path = PathBuf::from("file500000.txt");
                b.iter(|| {
                    let result = bloom.contains(black_box(&lookup_path));
                    black_box(result)
                });
            },
        );
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_deletion_planning,
    bench_bloom_filter_memory,
    bench_bloom_filter_lookup
);
criterion_main!(benches);
