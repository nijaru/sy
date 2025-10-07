mod cli;
mod compress;
mod config;
mod delta;
mod error;
mod integrity;
mod path;
mod ssh;
mod sync;
mod transport;

use anyhow::Result;
use clap::Parser;
use cli::Cli;
use colored::Colorize;
use config::Config;
use path::SyncPath;
use sync::{SyncEngine, watch::WatchMode};
use transport::router::TransportRouter;
use tracing_subscriber::{fmt, EnvFilter};
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<()> {
    // Parse CLI arguments
    let mut cli = Cli::parse();

    // Load config file
    let config = Config::load()?;

    // Handle profile-only flags (print and exit)
    if cli.list_profiles {
        let profiles = config.list_profiles();
        if profiles.is_empty() {
            println!("No profiles configured");
            println!("\nCreate profiles in: {}", Config::config_path()?.display());
        } else {
            println!("Available profiles:");
            for name in profiles {
                println!("  {}", name);
            }
        }
        return Ok(());
    }

    if let Some(ref profile_name) = cli.show_profile {
        match config.show_profile(profile_name) {
            Some(output) => {
                println!("{}", output);
                return Ok(());
            }
            None => {
                anyhow::bail!("Profile '{}' not found", profile_name);
            }
        }
    }

    // Merge profile with CLI args if --profile is set
    if let Some(ref profile_name) = cli.profile {
        let profile = config.get_profile(profile_name)
            .ok_or_else(|| anyhow::anyhow!("Profile '{}' not found", profile_name))?;

        // Apply profile settings (CLI args take precedence)
        if cli.source.is_none() {
            if let Some(ref source_str) = profile.source {
                cli.source = Some(SyncPath::parse(source_str));
            }
        }
        if cli.destination.is_none() {
            if let Some(ref dest_str) = profile.destination {
                cli.destination = Some(SyncPath::parse(dest_str));
            }
        }

        // Merge other profile settings
        if profile.delete.is_some() && !cli.delete {
            cli.delete = profile.delete.unwrap_or(false);
        }
        if profile.dry_run.is_some() && !cli.dry_run {
            cli.dry_run = profile.dry_run.unwrap_or(false);
        }
        if profile.quiet.is_some() && !cli.quiet {
            cli.quiet = profile.quiet.unwrap_or(false);
        }
        if let Some(verbose) = profile.verbose {
            if cli.verbose == 0 {
                cli.verbose = verbose;
            }
        }
        if let Some(parallel) = profile.parallel {
            if cli.parallel == 10 {  // Default value
                cli.parallel = parallel;
            }
        }
        if let Some(ref _bwlimit_str) = profile.bwlimit {
            if cli.bwlimit.is_none() {
                // TODO: Parse bwlimit from profile (needs parse_size exposed from cli module)
            }
        }
        if let Some(ref excludes) = profile.exclude {
            if cli.exclude.is_empty() {
                cli.exclude = excludes.clone();
            }
        }
        if let Some(resume) = profile.resume {
            cli.resume = resume;
        }
    }

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

    // After validation, source and destination must be present
    let source = cli.source.as_ref().expect("source required after validation");
    let destination = cli.destination.as_ref().expect("destination required after validation");

    // Print header (skip if JSON mode)
    if !cli.quiet && !cli.json {
        println!("sy v{}", env!("CARGO_PKG_VERSION"));
        println!("Syncing {} → {}", source, destination);

        if cli.dry_run {
            println!("Mode: Dry-run (no changes will be made)\n");
        }
    }

    // Create transport router based on source and destination
    let transport = TransportRouter::new(source, destination).await?;
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

    // Watch mode or regular sync
    if cli.watch {
        // Watch mode - continuous sync on file changes
        let watch_mode = WatchMode::new(
            engine,
            source.path().to_path_buf(),
            destination.path().to_path_buf(),
            Duration::from_millis(500), // 500ms debounce
        );

        watch_mode.watch().await?;
        return Ok(()); // Watch mode handles its own output
    }

    // Run sync (single file or directory)
    let stats = if cli.is_single_file() {
        if !cli.quiet && !cli.json {
            println!("Mode: Single file sync\n");
        }
        engine
            .sync_single_file(source.path(), destination.path())
            .await?
    } else {
        engine
            .sync(source.path(), destination.path())
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
