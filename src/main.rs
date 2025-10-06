mod cli;
mod compress;
mod delta;
mod error;
mod path;
mod ssh;
mod sync;
mod transport;

use anyhow::Result;
use clap::Parser;
use cli::Cli;
use colored::Colorize;
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

    // Print header (skip if JSON mode)
    if !cli.quiet && !cli.json {
        println!("sy v{}", env!("CARGO_PKG_VERSION"));
        println!("Syncing {} → {}", cli.source, cli.destination);

        if cli.dry_run {
            println!("Mode: Dry-run (no changes will be made)\n");
        }
    }

    // Create transport router based on source and destination
    let transport = TransportRouter::new(&cli.source, &cli.destination).await?;
    let engine = SyncEngine::new(
        transport,
        cli.dry_run,
        cli.delete,
        cli.quiet || cli.json,  // JSON mode implies quiet
        cli.parallel,
        cli.min_size,
        cli.max_size,
        cli.exclude.clone(),
        cli.bwlimit,
        cli.resume,
        cli.checkpoint_files,
        cli.checkpoint_bytes,
        cli.json,
    );

    // Run sync (single file or directory)
    let stats = if cli.is_single_file() {
        if !cli.quiet && !cli.json {
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

    // Print summary (skip if JSON mode - already emitted JSON summary)
    if !cli.quiet && !cli.json {
        if cli.dry_run {
            println!("\n{}\n", "✓ Dry-run complete (no changes made)".green().bold());
        } else {
            println!("\n{}\n", "✓ Sync complete".green().bold());
        }

        // File operations
        println!("  Files scanned:     {}", stats.files_scanned.to_string().blue());
        if cli.dry_run {
            println!("  Would create:      {}", stats.files_created.to_string().yellow());
            println!("  Would update:      {}", stats.files_updated.to_string().yellow());
            println!("  Would skip:        {}", stats.files_skipped.to_string().bright_black());
            if cli.delete {
                println!("  Would delete:      {}", stats.files_deleted.to_string().red());
            }
        } else {
            if stats.files_created > 0 {
                println!("  Files created:     {}", stats.files_created.to_string().green());
            } else {
                println!("  Files created:     {}", stats.files_created.to_string().bright_black());
            }
            if stats.files_updated > 0 {
                println!("  Files updated:     {}", stats.files_updated.to_string().yellow());
            } else {
                println!("  Files updated:     {}", stats.files_updated.to_string().bright_black());
            }
            println!("  Files skipped:     {}", stats.files_skipped.to_string().bright_black());
            if cli.delete && stats.files_deleted > 0 {
                println!("  Files deleted:     {}", stats.files_deleted.to_string().red());
            } else if cli.delete {
                println!("  Files deleted:     {}", stats.files_deleted.to_string().bright_black());
            }
        }

        // Transfer stats
        println!();
        println!(
            "  Bytes transferred: {}",
            format_bytes(stats.bytes_transferred).cyan()
        );

        // Calculate and display transfer rate
        let duration_secs = stats.duration.as_secs_f64();
        if duration_secs > 0.0 && stats.bytes_transferred > 0 {
            let bytes_per_sec = stats.bytes_transferred as f64 / duration_secs;
            println!("  Transfer rate:     {}", format!("{}/s", format_bytes(bytes_per_sec as u64)).cyan());
        }

        println!("  Duration:          {}", format_duration(stats.duration).cyan());

        // Delta sync stats (if used)
        if stats.files_delta_synced > 0 {
            println!();
            println!(
                "  {}        {} files, {} saved",
                "Delta sync:".bright_magenta(),
                stats.files_delta_synced.to_string().bright_magenta(),
                format_bytes(stats.delta_bytes_saved).bright_magenta()
            );
        }

        // Compression stats (if used)
        if stats.files_compressed > 0 {
            println!();
            println!(
                "  {}     {} files, {} saved",
                "Compression:".bright_cyan(),
                stats.files_compressed.to_string().bright_cyan(),
                format_bytes(stats.compression_bytes_saved).bright_cyan()
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
