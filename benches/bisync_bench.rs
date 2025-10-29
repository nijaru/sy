use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;
use sy::bisync::{classify_changes, resolve_changes, ConflictResolution};
use sy::sync::scanner::FileEntry;

fn make_file_entry(path: &str, size: u64, mtime_secs_ago: u64) -> FileEntry {
    let now = SystemTime::now();
    let mtime = now - std::time::Duration::from_secs(mtime_secs_ago);
    FileEntry {
        path: Arc::new(PathBuf::from(path)),
        relative_path: Arc::new(PathBuf::from(path)),
        size,
        modified: mtime,
        is_dir: false,
        is_symlink: false,
        symlink_target: None,
        is_sparse: false,
        allocated_size: size,
        xattrs: None,
        inode: None,
        nlink: 1,
        acls: None,
        bsd_flags: None,
    }
}

fn bench_classify_changes(c: &mut Criterion) {
    let mut group = c.benchmark_group("classify_changes");

    for file_count in [100, 500, 1000, 5000].iter() {
        // Create test data
        let mut source_files = Vec::new();
        let mut dest_files = Vec::new();
        let prior_state = HashMap::new();

        for i in 0..*file_count {
            let path = format!("file{}.txt", i);
            source_files.push(make_file_entry(&path, 1000, 60));
            dest_files.push(make_file_entry(&path, 1000, 60));
        }

        group.bench_with_input(
            BenchmarkId::from_parameter(file_count),
            file_count,
            |b, _| {
                b.iter(|| {
                    classify_changes(
                        black_box(&source_files),
                        black_box(&dest_files),
                        black_box(&prior_state),
                    )
                    .unwrap()
                });
            },
        );
    }
    group.finish();
}

fn bench_classify_changes_with_conflicts(c: &mut Criterion) {
    let mut group = c.benchmark_group("classify_changes_conflicts");

    for file_count in [100, 500, 1000].iter() {
        let mut source_files = Vec::new();
        let mut dest_files = Vec::new();
        let prior_state = HashMap::new();

        // Create scenarios with conflicts
        for i in 0..*file_count {
            let path = format!("file{}.txt", i);
            // Alternate between different sizes to create conflicts
            if i % 2 == 0 {
                source_files.push(make_file_entry(&path, 1000, 0));
                dest_files.push(make_file_entry(&path, 2000, 0));
            } else {
                source_files.push(make_file_entry(&path, 1000, 60));
                dest_files.push(make_file_entry(&path, 1000, 60));
            }
        }

        group.bench_with_input(
            BenchmarkId::from_parameter(file_count),
            file_count,
            |b, _| {
                b.iter(|| {
                    classify_changes(
                        black_box(&source_files),
                        black_box(&dest_files),
                        black_box(&prior_state),
                    )
                    .unwrap()
                });
            },
        );
    }
    group.finish();
}

fn bench_resolve_changes(c: &mut Criterion) {
    let mut group = c.benchmark_group("resolve_changes");

    for conflict_count in [10, 50, 100, 500].iter() {
        // Create changes with conflicts
        let mut changes = Vec::new();
        for i in 0..*conflict_count {
            let path = format!("conflict{}.txt", i);
            let source_entry = make_file_entry(&path, 1000, 0);
            let dest_entry = make_file_entry(&path, 2000, 0);

            changes.push(sy::bisync::Change {
                path: PathBuf::from(&path),
                change_type: sy::bisync::ChangeType::ModifiedBoth,
                source_entry: Some(source_entry),
                dest_entry: Some(dest_entry),
            });
        }

        group.bench_with_input(
            BenchmarkId::from_parameter(conflict_count),
            conflict_count,
            |b, _| {
                b.iter(|| {
                    resolve_changes(
                        black_box(changes.clone()),
                        black_box(ConflictResolution::Newer),
                    )
                    .unwrap()
                });
            },
        );
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_classify_changes,
    bench_classify_changes_with_conflicts,
    bench_resolve_changes
);
criterion_main!(benches);
