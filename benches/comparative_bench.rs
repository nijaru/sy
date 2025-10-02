use criterion::{black_box, criterion_group, criterion_main, Criterion};
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

fn bench_sy_vs_rsync_vs_cp(c: &mut Criterion) {
    let mut group = c.benchmark_group("tool_comparison_100_files");

    // Setup test data once
    let source = TempDir::new().unwrap();
    setup_files(&source, 100);

    // Benchmark sy
    group.bench_function("sy", |b| {
        b.iter(|| {
            let dest = TempDir::new().unwrap();
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

    // Benchmark rsync (if available)
    if Command::new("rsync").arg("--version").output().is_ok() {
        group.bench_function("rsync", |b| {
            b.iter(|| {
                let dest = TempDir::new().unwrap();
                let output = Command::new("rsync")
                    .args([
                        "-a",
                        &format!("{}/", source.path().display()),
                        dest.path().to_str().unwrap(),
                    ])
                    .output()
                    .unwrap();
                assert!(output.status.success());
                black_box(output);
            });
        });
    }

    // Benchmark cp -r
    group.bench_function("cp", |b| {
        b.iter(|| {
            let dest = TempDir::new().unwrap();
            let output = Command::new("cp")
                .args([
                    "-r",
                    source.path().to_str().unwrap(),
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

fn bench_sy_vs_rsync_large_file(c: &mut Criterion) {
    let mut group = c.benchmark_group("tool_comparison_large_file");
    group.sample_size(10);

    // Setup 50MB file
    let source = TempDir::new().unwrap();
    Command::new("git")
        .args(["init"])
        .current_dir(source.path())
        .output()
        .unwrap();

    let content = "x".repeat(50 * 1024 * 1024);
    fs::write(source.path().join("large.txt"), &content).unwrap();

    // Benchmark sy
    group.bench_function("sy", |b| {
        b.iter(|| {
            let dest = TempDir::new().unwrap();
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

    // Benchmark rsync (if available)
    if Command::new("rsync").arg("--version").output().is_ok() {
        group.bench_function("rsync", |b| {
            b.iter(|| {
                let dest = TempDir::new().unwrap();
                let output = Command::new("rsync")
                    .args([
                        "-a",
                        &format!("{}/", source.path().display()),
                        dest.path().to_str().unwrap(),
                    ])
                    .output()
                    .unwrap();
                assert!(output.status.success());
                black_box(output);
            });
        });
    }

    // Benchmark cp
    group.bench_function("cp", |b| {
        b.iter(|| {
            let dest = TempDir::new().unwrap();
            let output = Command::new("cp")
                .args([
                    "-r",
                    source.path().to_str().unwrap(),
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

fn bench_idempotent_comparison(c: &mut Criterion) {
    let mut group = c.benchmark_group("tool_comparison_idempotent");

    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();
    setup_files(&source, 100);

    // Initial sync for all tools
    Command::new(env!("CARGO_BIN_EXE_sy"))
        .args([
            source.path().to_str().unwrap(),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    let rsync_dest = TempDir::new().unwrap();
    if Command::new("rsync").arg("--version").output().is_ok() {
        Command::new("rsync")
            .args([
                "-a",
                &format!("{}/", source.path().display()),
                rsync_dest.path().to_str().unwrap(),
            ])
            .output()
            .unwrap();
    }

    // Benchmark sy idempotent
    group.bench_function("sy", |b| {
        b.iter(|| {
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

    // Benchmark rsync idempotent (if available)
    if Command::new("rsync").arg("--version").output().is_ok() {
        group.bench_function("rsync", |b| {
            b.iter(|| {
                let output = Command::new("rsync")
                    .args([
                        "-a",
                        &format!("{}/", source.path().display()),
                        rsync_dest.path().to_str().unwrap(),
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

fn bench_many_files_comparison(c: &mut Criterion) {
    let mut group = c.benchmark_group("tool_comparison_1000_files");
    group.sample_size(10);

    let source = TempDir::new().unwrap();
    setup_files(&source, 1000);

    // Benchmark sy
    group.bench_function("sy", |b| {
        b.iter(|| {
            let dest = TempDir::new().unwrap();
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

    // Benchmark rsync (if available)
    if Command::new("rsync").arg("--version").output().is_ok() {
        group.bench_function("rsync", |b| {
            b.iter(|| {
                let dest = TempDir::new().unwrap();
                let output = Command::new("rsync")
                    .args([
                        "-a",
                        &format!("{}/", source.path().display()),
                        dest.path().to_str().unwrap(),
                    ])
                    .output()
                    .unwrap();
                assert!(output.status.success());
                black_box(output);
            });
        });
    }

    // Benchmark cp
    group.bench_function("cp", |b| {
        b.iter(|| {
            let dest = TempDir::new().unwrap();
            let output = Command::new("cp")
                .args([
                    "-r",
                    source.path().to_str().unwrap(),
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

criterion_group!(
    benches,
    bench_sy_vs_rsync_vs_cp,
    bench_sy_vs_rsync_large_file,
    bench_idempotent_comparison,
    bench_many_files_comparison
);
criterion_main!(benches);
