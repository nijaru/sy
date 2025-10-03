mod cli;
mod delta;
mod error;
mod path;
mod ssh;
mod sync;
mod transport;

use anyhow::Result;
use clap::Parser;
use cli::Cli;
use sync::SyncEngine;
use transport::router::TransportRouter;
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
        println!("Syncing {} → {}", cli.source, cli.destination);

        if cli.dry_run {
            println!("Mode: Dry-run (no changes will be made)\n");
        }
    }

    // Create transport router based on source and destination
    let transport = TransportRouter::new(&cli.source, &cli.destination).await?;
    let engine = SyncEngine::new(transport, cli.dry_run, cli.delete, cli.quiet, cli.parallel);

    // Run sync (single file or directory)
    let stats = if cli.is_single_file() {
        if !cli.quiet {
            println!("Mode: Single file sync\n");
        }
        engine
            .sync_single_file(cli.source.path(), cli.destination.path())
            .await?
    } else {
        engine
            .sync(cli.source.path(), cli.destination.path())
            .await?
    };

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

        // Calculate and display transfer rate
        let duration_secs = stats.duration.as_secs_f64();
        if duration_secs > 0.0 && stats.bytes_transferred > 0 {
            let bytes_per_sec = stats.bytes_transferred as f64 / duration_secs;
            println!("  Transfer rate:    {}/s", format_bytes(bytes_per_sec as u64));
        }

        println!("  Duration:         {}", format_duration(stats.duration));

        if stats.files_delta_synced > 0 {
            println!(
                "  Delta sync:       {} files, {} saved",
                stats.files_delta_synced,
                format_bytes(stats.delta_bytes_saved)
            );
        }
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

fn format_duration(duration: std::time::Duration) -> String {
    let secs = duration.as_secs();
    let millis = duration.subsec_millis();

    if secs >= 60 {
        let mins = secs / 60;
        let secs = secs % 60;
        if mins >= 60 {
            let hours = mins / 60;
            let mins = mins % 60;
            format!("{}h {}m {}s", hours, mins, secs)
        } else {
            format!("{}m {}s", mins, secs)
        }
    } else if secs > 0 {
        format!("{}.{:03}s", secs, millis)
    } else {
        format!("{}ms", millis)
    }
}
