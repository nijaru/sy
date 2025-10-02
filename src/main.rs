mod cli;
mod error;
mod ssh;
mod sync;
mod transport;

use anyhow::Result;
use clap::Parser;
use cli::Cli;
use sync::SyncEngine;
use transport::local::LocalTransport;
use tracing_subscriber::{fmt, EnvFilter};

#[tokio::main]
async fn main() -> Result<()> {
    // Parse CLI arguments
    let cli = Cli::parse();

    // Setup logging
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(cli.log_level().as_str()));

    fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_thread_ids(false)
        .with_file(false)
        .with_line_number(false)
        .compact()
        .init();

    // Validate arguments
    cli.validate()?;

    if !cli.quiet {
        println!("sy v{}", env!("CARGO_PKG_VERSION"));
        println!(
            "Syncing {} → {}",
            cli.source.display(),
            cli.destination.display()
        );

        if cli.dry_run {
            println!("Mode: Dry-run (no changes will be made)\n");
        }
    }

    // Create transport and sync engine
    let transport = LocalTransport::new();
    let engine = SyncEngine::new(transport, cli.dry_run, cli.delete, cli.quiet);

    // Run sync
    let stats = engine.sync(&cli.source, &cli.destination).await?;

    // Print summary
    if !cli.quiet {
        println!("\n✓ Sync complete");
        println!("  Files scanned:    {}", stats.files_scanned);
        println!("  Files created:    {}", stats.files_created);
        println!("  Files updated:    {}", stats.files_updated);
        println!("  Files skipped:    {}", stats.files_skipped);
        if cli.delete {
            println!("  Files deleted:    {}", stats.files_deleted);
        }
        println!(
            "  Bytes transferred: {}",
            format_bytes(stats.bytes_transferred)
        );
    }

    Ok(())
}

fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.2} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}
