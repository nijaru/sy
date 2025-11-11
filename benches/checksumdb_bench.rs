use criterion::{black_box, criterion_group, criterion_main, Criterion};
use std::path::PathBuf;
use std::time::SystemTime;
use sy::integrity::Checksum;
use sy::sync::checksumdb::ChecksumDatabase;
use tempfile::TempDir;

/// Benchmark: Write checksums to database
fn bench_write_checksums(c: &mut Criterion) {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path();

    c.bench_function("checksumdb_write_1k", |b| {
        b.iter(|| {
            let db = ChecksumDatabase::open(db_path).unwrap();

            // Write 1,000 checksums
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

/// Benchmark: Read checksums from database
fn bench_read_checksums(c: &mut Criterion) {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path();

    // Pre-populate with 1,000 checksums
    let db = ChecksumDatabase::open(db_path).unwrap();
    let now = SystemTime::now();
    for i in 0..1000 {
        let path = PathBuf::from(format!("file_{:04}.txt", i));
        let checksum = Checksum::cryptographic(vec![0u8; 32]);
        let size = 1024u64;

        db.store_checksum(&path, now, size, &checksum).unwrap();
    }

    drop(db); // Close the database

    c.bench_function("checksumdb_read_1k", |b| {
        b.iter(|| {
            let db = ChecksumDatabase::open(db_path).unwrap();

            // Read all checksums
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

criterion_group!(benches, bench_write_checksums, bench_read_checksums);
criterion_main!(benches);
