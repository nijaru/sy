use criterion::{black_box, criterion_group, criterion_main, Criterion};
use std::path::PathBuf;
use std::time::SystemTime;
use sy::integrity::Checksum;
use sy::sync::checksumdb::ChecksumDatabase;
use tempfile::TempDir;

#[cfg(feature = "seerdb-bench")]
use seerdb::{DBOptions, DB};

/// Benchmark fjall writes (1,000 checksums)
fn bench_fjall_write(c: &mut Criterion) {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path();

    c.bench_function("fjall_checksumdb_write_1k", |b| {
        b.iter(|| {
            let db = ChecksumDatabase::open(db_path).unwrap();

            for i in 0..1000 {
                let path = PathBuf::from(format!("file_{:04}.txt", i));
                let checksum = Checksum::cryptographic(vec![0u8; 32]);
                let mtime = SystemTime::now();
                let size = 1024u64;

                db.store_checksum(&path, mtime, size, &checksum).unwrap();
            }
        });
    });
}

/// Benchmark fjall reads (1,000 checksums)
fn bench_fjall_read(c: &mut Criterion) {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path();

    // Pre-populate
    let db = ChecksumDatabase::open(db_path).unwrap();
    let now = SystemTime::now();
    for i in 0..1000 {
        let path = PathBuf::from(format!("file_{:04}.txt", i));
        let checksum = Checksum::cryptographic(vec![0u8; 32]);
        let size = 1024u64;
        db.store_checksum(&path, now, size, &checksum).unwrap();
    }
    drop(db);

    c.bench_function("fjall_checksumdb_read_1k", |b| {
        b.iter(|| {
            let db = ChecksumDatabase::open(db_path).unwrap();

            for i in 0..1000 {
                let path = PathBuf::from(format!("file_{:04}.txt", i));
                let mtime = SystemTime::now();
                let size = 1024u64;

                let _ = black_box(
                    db.get_checksum(&path, mtime, size, "cryptographic")
                        .unwrap(),
                );
            }
        });
    });
}

#[cfg(feature = "seerdb-bench")]
fn bench_seerdb_write(c: &mut Criterion) {
    let temp_dir = TempDir::new().unwrap();

    c.bench_function("seerdb_checksumdb_write_1k", |b| {
        b.iter(|| {
            let mut opts = DBOptions::default();
            opts.data_dir = temp_dir.path().to_path_buf();
            let db = DB::open(opts).unwrap();

            for i in 0..1000 {
                let key = format!("file_{:04}.txt", i);
                let value = vec![0u8; 32 + 16]; // checksum + metadata
                db.put(key.as_bytes(), &value).unwrap();
            }
        });
    });
}

#[cfg(feature = "seerdb-bench")]
fn bench_seerdb_read(c: &mut Criterion) {
    let temp_dir = TempDir::new().unwrap();
    let mut opts = DBOptions::default();
    opts.data_dir = temp_dir.path().to_path_buf();

    // Pre-populate
    let db = DB::open(opts.clone()).unwrap();
    for i in 0..1000 {
        let key = format!("file_{:04}.txt", i);
        let value = vec![0u8; 32 + 16];
        db.put(key.as_bytes(), &value).unwrap();
    }
    drop(db);

    c.bench_function("seerdb_checksumdb_read_1k", |b| {
        b.iter(|| {
            let opts = DBOptions {
                data_dir: temp_dir.path().to_path_buf(),
                ..Default::default()
            };
            let db = DB::open(opts).unwrap();

            for i in 0..1000 {
                let key = format!("file_{:04}.txt", i);
                let _ = black_box(db.get(key.as_bytes()).unwrap());
            }
        });
    });
}

#[cfg(not(feature = "seerdb-bench"))]
fn bench_seerdb_write(_c: &mut Criterion) {
    // Placeholder when seerdb feature is disabled
}

#[cfg(not(feature = "seerdb-bench"))]
fn bench_seerdb_read(_c: &mut Criterion) {
    // Placeholder when seerdb feature is disabled
}

criterion_group!(
    benches,
    bench_fjall_write,
    bench_fjall_read,
    bench_seerdb_write,
    bench_seerdb_read
);
criterion_main!(benches);
