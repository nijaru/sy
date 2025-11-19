use clap::Parser;
use rayon::prelude::*;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

#[derive(Parser, Debug)]
#[command(name = "sy-bench-gen")]
#[command(about = "Generates massive file trees for sy benchmarking")]
struct Args {
    /// Root directory for the dataset
    #[arg(short, long, default_value = "bench_data")]
    root: PathBuf,

    /// Total number of files to generate
    #[arg(short, long, default_value_t = 100_000)]
    count: usize,

    /// Directory depth (creates nested structure)
    #[arg(short, long, default_value_t = 5)]
    depth: usize,

    /// Max files per directory
    #[arg(short, long, default_value_t = 100)]
    width: usize,

    /// Min file size in bytes
    #[arg(long, default_value_t = 100)]
    min_size: usize,

    /// Max file size in bytes
    #[arg(long, default_value_t = 10_000)]
    max_size: usize,

    /// Verification mode (verifies files exist instead of creating)
    #[arg(long)]
    verify: bool,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let start = Instant::now();

    if args.verify {
        println!("Verifying dataset at {:?}...", args.root);
        // Verification logic could go here
        return Ok(());
    }

    println!("Generating dataset at {:?}", args.root);
    println!("  Files: {}", args.count);
    println!("  Depth: {}", args.depth);
    println!("  Width: {}", args.width);
    println!("  Size:  {}-{} bytes", args.min_size, args.max_size);

    if args.root.exists() {
        println!("Cleaning up existing directory...");
        fs::remove_dir_all(&args.root)?;
    }
    fs::create_dir_all(&args.root)?;

    let files_created = AtomicUsize::new(0);
    let bytes_written = AtomicUsize::new(0);

    // Calculate directory structure
    // We want a tree that roughly balances files
    // For simplicity, we'll just map index -> path

    (0..args.count).into_par_iter().for_each(|i| {
        let path = generate_path(&args.root, i, args.depth, args.width);
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }

        let size = pseudo_random(i, args.min_size, args.max_size);
        let content = generate_content(i, size);

        if let Ok(file) = File::create(&path) {
            let mut writer = BufWriter::new(file);
            if writer.write_all(&content).is_ok() {
                files_created.fetch_add(1, Ordering::Relaxed);
                bytes_written.fetch_add(size, Ordering::Relaxed);
            }
        }
    });

    let duration = start.elapsed();
    let files = files_created.load(Ordering::Relaxed);
    let bytes = bytes_written.load(Ordering::Relaxed);

    println!("\nDone!");
    println!("  Time:    {:.2?}", duration);
    println!("  Files:   {}", files);
    println!("  Bytes:   {:.2} MB", bytes as f64 / 1_000_000.0);
    println!(
        "  Rate:    {:.2} files/sec",
        files as f64 / duration.as_secs_f64()
    );

    Ok(())
}

fn generate_path(root: &Path, index: usize, depth: usize, width: usize) -> PathBuf {
    let mut path = root.to_path_buf();
    let mut current = index;

    // Create deeper structure by modulo division
    for _ in 0..depth {
        let dir_idx = current % width;
        current /= width;
        path.push(format!("dir_{:03}", dir_idx));
        if current == 0 {
            break;
        }
    }

    path.push(format!("file_{:06}.dat", index));
    path
}

// Simple LCG for deterministic pseudo-random numbers
fn pseudo_random(seed: usize, min: usize, max: usize) -> usize {
    let mut x = seed as u64;
    x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
    let r = (x >> 33) as usize; // Use upper bits
    min + (r % (max - min + 1))
}

fn generate_content(seed: usize, size: usize) -> Vec<u8> {
    let mut content = Vec::with_capacity(size);
    let mut x = seed as u64;

    // Fill with some pattern that isn't purely repeating (to test rolling hash/compression)
    for _ in 0..size {
        x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
        content.push((x >> 33) as u8);
    }
    content
}
