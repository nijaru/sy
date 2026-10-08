use criterion::{black_box, criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion};
use std::fs;
use std::io::Write;
use std::process::{Command, Output};
use tempfile::TempDir;

fn setup_files(dir: &TempDir, count: usize) {
    for i in 0..count {
        fs::write(
            dir.path().join(format!("file_{}.txt", i)),
            format!("content_{}", i),
        )
        .unwrap();
    }
}

fn sync(source: &TempDir, dest: &TempDir, options: &[&str]) -> Output {
    let output = Command::new(env!("CARGO_BIN_EXE_sy"))
        .arg(format!("{}/", source.path().display()))
        .arg(dest.path())
        .args(options)
        .output()
        .expect("run benchmark sy binary");
    assert!(
        output.status.success(),
        "sy failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn bench_sync_small_files(c: &mut Criterion) {
    let mut group = c.benchmark_group("sync_small_files");

    for file_count in [10, 50, 100, 500].iter() {
        group.bench_with_input(
            BenchmarkId::from_parameter(file_count),
            file_count,
            |b, &count| {
                // PerIteration keeps fixture creation AND TempDir cleanup outside timing.
                b.iter_batched_ref(
                    || {
                        let source = TempDir::new().unwrap();
                        let dest = TempDir::new().unwrap();
                        setup_files(&source, count);
                        (source, dest)
                    },
                    |(source, dest)| black_box(sync(source, dest, &[])),
                    BatchSize::PerIteration,
                );
            },
        );
    }
    group.finish();
}

fn bench_sync_nested_dirs(c: &mut Criterion) {
    let mut group = c.benchmark_group("sync_nested_dirs");

    for depth in [5, 10, 20, 50].iter() {
        group.bench_with_input(BenchmarkId::from_parameter(depth), depth, |b, &depth| {
            b.iter_batched_ref(
                || {
                    let source = TempDir::new().unwrap();
                    let dest = TempDir::new().unwrap();
                    let mut path = source.path().to_path_buf();
                    for i in 0..depth {
                        path = path.join(format!("level_{}", i));
                    }
                    fs::create_dir_all(&path).unwrap();
                    fs::write(path.join("file.txt"), "content").unwrap();
                    (source, dest)
                },
                |(source, dest)| black_box(sync(source, dest, &[])),
                BatchSize::PerIteration,
            );
        });
    }
    group.finish();
}

fn bench_sync_large_files(c: &mut Criterion) {
    let mut group = c.benchmark_group("sync_large_files");
    group.sample_size(10);

    // The Python harness already covers a ~100 MB fresh copy. Keep the
    // former 500 MiB / 1 GiB test workloads here, without normal-suite limits.
    for size_mb in [1, 5, 10, 500, 1024].iter() {
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{}MB", size_mb)),
            size_mb,
            |b, &size_mb| {
                b.iter_batched_ref(
                    || {
                        let source = TempDir::new().unwrap();
                        let dest = TempDir::new().unwrap();
                        let mut file = fs::File::create(source.path().join("large.txt")).unwrap();
                        let block = [b'x'; 64 * 1024];
                        for _ in 0..size_mb * 16 {
                            file.write_all(&block).unwrap();
                        }
                        (source, dest)
                    },
                    |(source, dest)| black_box(sync(source, dest, &[])),
                    BatchSize::PerIteration,
                );
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
        sync(&source, &dest, &[]);

        b.iter(|| black_box(sync(&source, &dest, &[])));
    });
}

fn bench_sync_gitignore(c: &mut Criterion) {
    c.bench_function("sync_gitignore_50_included_50_ignored", |b| {
        b.iter_batched_ref(
            || {
                let source = TempDir::new().unwrap();
                let dest = TempDir::new().unwrap();
                let git = Command::new("git")
                    .arg("init")
                    .current_dir(source.path())
                    .output()
                    .unwrap();
                assert!(
                    git.status.success(),
                    "{}",
                    String::from_utf8_lossy(&git.stderr)
                );
                fs::write(source.path().join(".gitignore"), "*.ignored\n").unwrap();
                setup_files(&source, 50);
                for index in 0..50 {
                    fs::write(
                        source.path().join(format!("file_{index}.ignored")),
                        "ignored",
                    )
                    .unwrap();
                }
                (source, dest)
            },
            |(source, dest)| black_box(sync(source, dest, &["--gitignore", "--exclude-vcs"])),
            BatchSize::PerIteration,
        );
    });
}

criterion_group!(
    benches,
    bench_sync_small_files,
    bench_sync_nested_dirs,
    bench_sync_large_files,
    bench_sync_idempotent,
    bench_sync_gitignore
);
criterion_main!(benches);
