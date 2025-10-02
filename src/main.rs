use clap::Parser;

#[derive(Parser)]
#[command(name = "sy")]
#[command(about = "Modern rsync alternative - Fast, parallel file synchronization", long_about = None)]
#[command(version)]
struct Cli {
    /// Source path
    source: String,

    /// Destination path (can be remote: user@host:/path)
    destination: String,

    /// Show changes without applying them
    #[arg(short, long)]
    preview: bool,

    /// Delete files in destination not present in source
    #[arg(short, long)]
    delete: bool,

    /// Verify integrity with cryptographic checksums (BLAKE3)
    #[arg(short, long)]
    verify: bool,

    /// Number of parallel workers (default: CPU cores)
    #[arg(short, long)]
    workers: Option<usize>,

    /// Compression: auto, zstd, lz4, none
    #[arg(short, long, default_value = "auto")]
    compress: String,

    /// Fast mode (lz4 compression)
    #[arg(long)]
    fast: bool,

    /// Maximum compression (zstd level 11)
    #[arg(long)]
    max_compression: bool,

    /// Disable parallel transfers
    #[arg(long)]
    no_parallel: bool,

    /// Bandwidth limit (e.g., 10M, 1G)
    #[arg(long)]
    bandwidth: Option<String>,

    /// Use config file
    #[arg(long)]
    config: Option<String>,
}

fn main() {
    let cli = Cli::parse();

    println!("sy v{}", env!("CARGO_PKG_VERSION"));
    println!("Syncing {} → {}", cli.source, cli.destination);

    if cli.preview {
        println!("Mode: Preview (no changes will be made)");
    }

    println!("\nNOTE: This is a skeleton. Implementation coming soon!");
    println!("\nPlanned features:");
    println!("  ✓ Parallel file transfers");
    println!("  ✓ Parallel chunk transfers for large files");
    println!("  ✓ Adaptive compression (zstd/lz4)");
    println!("  ✓ xxHash3 for fast integrity checks");
    println!("  ✓ BLAKE3 for cryptographic verification");
    println!("  ✓ gitignore/syncignore support");
    println!("  ✓ Beautiful progress bars");
    println!("\nSee DESIGN.md for architecture details.");
}
