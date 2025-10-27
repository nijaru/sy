mod bisync;
mod cli;
mod compress;
mod config;
mod delta;
mod error;
mod filter;
mod fs_util;
mod hooks;
mod integrity;
mod path;
mod perf;
mod resource;
mod sparse;
mod ssh;
mod sync;
mod temp_file;
mod transport;

use anyhow::{Context as _, Result};
use clap::Parser;
use cli::Cli;
use colored::Colorize;
use config::Config;
use filter::FilterEngine;
use hooks::{HookContext, HookExecutor, HookType};
use path::SyncPath;
use std::path::PathBuf;
use std::time::Duration;
use sync::{watch::WatchMode, SyncEngine};
use tracing_subscriber::{fmt, EnvFilter};
use transport::router::TransportRouter;

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
        let profile = config
            .get_profile(profile_name)
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
            if cli.parallel == 10 {
                // Default value
                cli.parallel = parallel;
            }
        }
        if let Some(ref bwlimit_str) = profile.bwlimit {
            if cli.bwlimit.is_none() {
                cli.bwlimit = Some(cli::parse_size(bwlimit_str).map_err(|e| {
                    anyhow::anyhow!("Invalid bwlimit in profile '{}': {}", profile_name, e)
                })?);
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
    let source = cli
        .source
        .as_ref()
        .expect("source required after validation");
    let destination = cli
        .destination
        .as_ref()
        .expect("destination required after validation");

    // Create hook executor (unless disabled)
    let hook_executor = if cli.no_hooks {
        None
    } else {
        HookExecutor::new()
            .ok()
            .map(|e| e.with_abort_on_failure(cli.abort_on_hook_failure))
    };

    // Clean state files if requested
    if cli.clean_state {
        use sync::resume::ResumeState;
        if let Err(e) = ResumeState::delete(destination.path()) {
            tracing::warn!("Failed to clean state file: {}", e);
        } else if !cli.quiet && !cli.json {
            tracing::info!("Cleaned existing state files");
        }
    }

    // Clear cache if requested (before creating engine)
    if cli.clear_cache {
        use sync::dircache::DirectoryCache;
        if let Err(e) = DirectoryCache::delete(destination.path()) {
            tracing::warn!("Failed to clear directory cache: {}", e);
        } else if !cli.quiet && !cli.json {
            tracing::info!("Cleared directory cache");
        }
    }

    // Print header (skip if JSON mode)
    if !cli.quiet && !cli.json {
        println!("sy v{}", env!("CARGO_PKG_VERSION"));
        println!("Syncing {} → {}", source, destination);

        if cli.dry_run {
            println!("Mode: Dry-run (no changes will be made)\n");
        }
    }

    // Get verification mode
    let verification_mode = cli.verification_mode();
    let checksum_type = verification_mode.checksum_type();
    let verify_on_write = verification_mode.verify_blocks();

    // Create transport router based on source and destination
    // Use worker count for SSH connection pool size to enable true parallel transfers
    let transport = TransportRouter::new(
        source,
        destination,
        checksum_type,
        verify_on_write,
        cli.parallel, // SSH connection pool size = number of workers
    )
    .await?;

    // Get symlink mode
    let symlink_mode = cli.symlink_mode();

    // Build filter engine from CLI arguments
    let mut filter_engine = FilterEngine::new();

    // Process --filter rules first (explicit order matters)
    for rule in &cli.filter {
        if let Err(e) = filter_engine.add_rule(rule) {
            anyhow::bail!("Invalid filter rule '{}': {}", rule, e);
        }
    }

    // Process --include patterns
    for pattern in &cli.include {
        if let Err(e) = filter_engine.add_include(pattern) {
            anyhow::bail!("Invalid include pattern '{}': {}", pattern, e);
        }
    }

    // Process --exclude patterns
    for pattern in &cli.exclude {
        if let Err(e) = filter_engine.add_exclude(pattern) {
            anyhow::bail!("Invalid exclude pattern '{}': {}", pattern, e);
        }
    }

    // Load --include-from file
    if let Some(ref include_from) = cli.include_from {
        // Read as include patterns (not rsync rules)
        use std::fs::File;
        use std::io::{BufRead, BufReader};

        let file = File::open(include_from)
            .with_context(|| format!("Failed to open include file: {}", include_from.display()))?;
        let reader = BufReader::new(file);

        for (line_num, line) in reader.lines().enumerate() {
            let line = line.with_context(|| {
                format!(
                    "Failed to read line {} from {}",
                    line_num + 1,
                    include_from.display()
                )
            })?;
            let line = line.trim();

            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            if let Err(e) = filter_engine.add_include(line) {
                anyhow::bail!(
                    "Invalid include pattern at line {} in {}: {}",
                    line_num + 1,
                    include_from.display(),
                    e
                );
            }
        }
    }

    // Load --exclude-from file
    if let Some(ref exclude_from) = cli.exclude_from {
        // Read as exclude patterns (not rsync rules)
        use std::fs::File;
        use std::io::{BufRead, BufReader};

        let file = File::open(exclude_from)
            .with_context(|| format!("Failed to open exclude file: {}", exclude_from.display()))?;
        let reader = BufReader::new(file);

        for (line_num, line) in reader.lines().enumerate() {
            let line = line.with_context(|| {
                format!(
                    "Failed to read line {} from {}",
                    line_num + 1,
                    exclude_from.display()
                )
            })?;
            let line = line.trim();

            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            if let Err(e) = filter_engine.add_exclude(line) {
                anyhow::bail!(
                    "Invalid exclude pattern at line {} in {}: {}",
                    line_num + 1,
                    exclude_from.display(),
                    e
                );
            }
        }
    }

    // Load ignore templates
    for template_name in &cli.ignore_template {
        if let Err(e) = filter_engine.add_template(template_name) {
            tracing::warn!("Failed to load template '{}': {}", template_name, e);
        } else if !cli.quiet && !cli.json {
            tracing::info!("Loaded ignore template: {}", template_name);
        }
    }

    // Load .syignore from source directory (if local)
    if source.is_local() {
        let source_dir = if source.path().is_file() {
            source.path().parent().unwrap_or(source.path())
        } else {
            source.path()
        };

        match filter_engine.add_syignore_if_exists(source_dir) {
            Ok(true) => {
                if !cli.quiet && !cli.json {
                    tracing::info!("Loaded .syignore from {}", source_dir.display());
                }
            }
            Ok(false) => {
                // No .syignore file, that's fine
            }
            Err(e) => {
                tracing::warn!("Failed to load .syignore: {}", e);
            }
        }
    }

    let engine = SyncEngine::new(
        transport,
        cli.dry_run,
        cli.diff,
        cli.delete,
        cli.delete_threshold,
        cli.trash,
        cli.force_delete,
        cli.quiet || cli.json, // JSON mode implies quiet
        cli.parallel,
        cli.max_errors,
        cli.min_size,
        cli.max_size,
        filter_engine,
        cli.bwlimit,
        cli.resume,
        cli.checkpoint_files,
        cli.checkpoint_bytes,
        cli.json,
        checksum_type,
        verify_on_write,
        symlink_mode,
        cli.preserve_xattrs,
        cli.preserve_hardlinks,
        cli.preserve_acls,
        cli.preserve_flags,
        cli.ignore_times,
        cli.size_only,
        cli.checksum,
        cli.verify_only,
        cli.use_cache,
        cli.clear_cache,
        cli.checksum_db,
        cli.clear_checksum_db,
        cli.prune_checksum_db,
        cli.perf,
    );

    // Execute pre-sync hook
    if let Some(ref executor) = hook_executor {
        let pre_context = HookContext {
            source: source.to_string(),
            destination: destination.to_string(),
            files_scanned: 0,
            files_created: 0,
            files_updated: 0,
            files_deleted: 0,
            files_skipped: 0,
            bytes_transferred: 0,
            duration_secs: 0,
            dry_run: cli.dry_run,
        };

        if let Err(e) = executor.execute(HookType::PreSync, &pre_context) {
            tracing::error!("Pre-sync hook failed: {}", e);
            return Err(e.into());
        }
    }

    // Verify-only mode
    if cli.verify_only {
        if !cli.quiet && !cli.json {
            println!("sy v{}", env!("CARGO_PKG_VERSION"));
            println!("Verifying {} ↔ {}\n", source, destination);
        }

        let result = engine.verify(source.path(), destination.path()).await?;

        // Determine exit code
        let exit_code = if !result.errors.is_empty() {
            2 // Errors occurred
        } else if !result.files_mismatched.is_empty()
            || !result.files_only_in_source.is_empty()
            || !result.files_only_in_dest.is_empty()
        {
            1 // Mismatches found
        } else {
            0 // All matched
        };

        // JSON output
        if cli.json {
            use sy::sync::output::{SyncEvent, VerificationError};

            let errors_json: Vec<VerificationError> = result
                .errors
                .iter()
                .map(|e| VerificationError {
                    path: e.path.clone(),
                    error: e.error.clone(),
                    action: e.action.clone(),
                })
                .collect();

            let event = SyncEvent::VerificationResult {
                files_matched: result.files_matched,
                files_mismatched: result.files_mismatched.clone(),
                files_only_in_source: result.files_only_in_source.clone(),
                files_only_in_dest: result.files_only_in_dest.clone(),
                errors: errors_json,
                duration_secs: result.duration.as_secs_f64(),
                exit_code,
            };
            event.emit();
        } else if !cli.quiet {
            // Human-readable output
            println!("\n✓ Verification complete\n");
            println!("  Files matched:        {}", result.files_matched);

            if !result.files_mismatched.is_empty() {
                println!(
                    "  Files mismatched:     {} ✗",
                    result.files_mismatched.len()
                );
                for path in &result.files_mismatched {
                    println!("    - {}", path.display());
                }
            }

            if !result.files_only_in_source.is_empty() {
                println!(
                    "  Only in source:       {}",
                    result.files_only_in_source.len()
                );
                for path in &result.files_only_in_source {
                    println!("    → {}", path.display());
                }
            }

            if !result.files_only_in_dest.is_empty() {
                println!(
                    "  Only in destination:  {}",
                    result.files_only_in_dest.len()
                );
                for path in &result.files_only_in_dest {
                    println!("    ← {}", path.display());
                }
            }

            if !result.errors.is_empty() {
                println!("\n⚠️  Errors occurred during verification:\n");
                for (i, error) in result.errors.iter().enumerate() {
                    println!("{}. [{}] {}", i + 1, error.action, error.path.display());
                    println!("   {}", error.error);
                }
            }

            println!("\n  Duration:             {:?}", result.duration);
        }

        std::process::exit(exit_code);
    }

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

    // Run sync (single file, directory, or bidirectional)
    let stats = if cli.bidirectional {
        // Bidirectional sync mode
        if !cli.quiet && !cli.json {
            println!("sy v{}", env!("CARGO_PKG_VERSION"));
            println!("Mode: Bidirectional sync");
            println!("Strategy: {}", cli.conflict_resolve);
            println!("{} ↔ {}\n", source, destination);
        }

        // Create transports for source and destination
        let (source_transport, dest_transport): (
            std::sync::Arc<dyn transport::Transport>,
            std::sync::Arc<dyn transport::Transport>,
        ) = match (&source, &destination) {
            (crate::path::SyncPath::Local(_), crate::path::SyncPath::Local(_)) => {
                // Both local
                let verifier = integrity::IntegrityVerifier::new(checksum_type, verify_on_write);
                let local_source = std::sync::Arc::new(transport::local::LocalTransport::with_verifier(verifier.clone()));
                let local_dest = std::sync::Arc::new(transport::local::LocalTransport::with_verifier(verifier));
                (local_source, local_dest)
            }
            (crate::path::SyncPath::Local(_), crate::path::SyncPath::Remote { host, user, .. }) => {
                // Local → Remote
                let config = if let Some(user) = user {
                    ssh::config::SshConfig {
                        hostname: host.clone(),
                        user: user.clone(),
                        ..Default::default()
                    }
                } else {
                    ssh::config::parse_ssh_config(host)?
                };
                let verifier = integrity::IntegrityVerifier::new(checksum_type, verify_on_write);
                let local = std::sync::Arc::new(transport::local::LocalTransport::with_verifier(verifier));
                let remote = std::sync::Arc::new(transport::ssh::SshTransport::with_pool_size(&config, cli.parallel).await?);
                (local, remote)
            }
            (crate::path::SyncPath::Remote { host, user, .. }, crate::path::SyncPath::Local(_)) => {
                // Remote → Local
                let config = if let Some(user) = user {
                    ssh::config::SshConfig {
                        hostname: host.clone(),
                        user: user.clone(),
                        ..Default::default()
                    }
                } else {
                    ssh::config::parse_ssh_config(host)?
                };
                let verifier = integrity::IntegrityVerifier::new(checksum_type, verify_on_write);
                let remote = std::sync::Arc::new(transport::ssh::SshTransport::with_pool_size(&config, cli.parallel).await?);
                let local = std::sync::Arc::new(transport::local::LocalTransport::with_verifier(verifier));
                (remote, local)
            }
            (crate::path::SyncPath::Remote { host: host1, user: user1, .. }, crate::path::SyncPath::Remote { host: host2, user: user2, .. }) => {
                // Remote → Remote
                let config1 = if let Some(user) = user1 {
                    ssh::config::SshConfig {
                        hostname: host1.clone(),
                        user: user.clone(),
                        ..Default::default()
                    }
                } else {
                    ssh::config::parse_ssh_config(host1)?
                };
                let config2 = if let Some(user) = user2 {
                    ssh::config::SshConfig {
                        hostname: host2.clone(),
                        user: user.clone(),
                        ..Default::default()
                    }
                } else {
                    ssh::config::parse_ssh_config(host2)?
                };
                let remote1 = std::sync::Arc::new(transport::ssh::SshTransport::with_pool_size(&config1, cli.parallel).await?);
                let remote2 = std::sync::Arc::new(transport::ssh::SshTransport::with_pool_size(&config2, cli.parallel).await?);
                (remote1, remote2)
            }
            _ => {
                anyhow::bail!("Bidirectional sync does not support S3 paths");
            }
        };

        let bisync_engine = bisync::BisyncEngine::new(source_transport, dest_transport);
        let bisync_opts = bisync::BisyncOptions {
            conflict_resolution: bisync::ConflictResolution::from_str(&cli.conflict_resolve)
                .ok_or_else(|| anyhow::anyhow!("Invalid conflict resolution strategy"))?,
            max_delete_percent: cli.max_delete,
            dry_run: cli.dry_run,
            clear_state: cli.clear_bisync_state,
        };

        let bisync_result = bisync_engine.sync(source.path(), destination.path(), bisync_opts).await?;

        // Print conflicts if any
        if !bisync_result.conflicts.is_empty() && !cli.quiet && !cli.json {
            println!("\n{} conflicts detected:", bisync_result.conflicts.len());
            for conflict in &bisync_result.conflicts {
                println!("  {} - {}", conflict.path.display(), conflict.action);
            }
            println!();
        }

        // Convert BisyncStats to SyncStats for compatibility
        sync::SyncStats {
            files_scanned: (bisync_result.stats.files_synced_to_source
                + bisync_result.stats.files_synced_to_dest),
            files_created: bisync_result.stats.files_synced_to_dest,
            files_updated: bisync_result.stats.files_synced_to_source,
            files_deleted: bisync_result.stats.files_deleted_from_source
                + bisync_result.stats.files_deleted_from_dest,
            files_skipped: 0,
            bytes_transferred: bisync_result.stats.bytes_transferred,
            files_delta_synced: 0,
            delta_bytes_saved: 0,
            files_compressed: 0,
            compression_bytes_saved: 0,
            files_verified: 0,
            verification_failures: 0,
            duration: std::time::Duration::from_millis(bisync_result.stats.duration_ms as u64),
            bytes_would_add: 0,
            bytes_would_change: 0,
            bytes_would_delete: 0,
            errors: bisync_result.errors.into_iter().map(|e| sync::SyncError {
                path: PathBuf::new(),
                error: e,
                action: "bidirectional sync".to_string(),
            }).collect(),
        }
    } else if cli.is_single_file() {
        if !cli.quiet && !cli.json {
            println!("Mode: Single file sync\n");
        }
        engine
            .sync_single_file(source.path(), destination.path())
            .await?
    } else {
        engine.sync(source.path(), destination.path()).await?
    };

    // Execute post-sync hook
    if let Some(ref executor) = hook_executor {
        let post_context = HookContext {
            source: source.to_string(),
            destination: destination.to_string(),
            files_scanned: stats.files_scanned,
            files_created: stats.files_created,
            files_updated: stats.files_updated,
            files_deleted: stats.files_deleted,
            files_skipped: stats.files_skipped,
            bytes_transferred: stats.bytes_transferred,
            duration_secs: stats.duration.as_secs(),
            dry_run: cli.dry_run,
        };

        if let Err(e) = executor.execute(HookType::PostSync, &post_context) {
            tracing::error!("Post-sync hook failed: {}", e);
            // Don't abort after successful sync, just warn
        }
    }

    // Print summary (skip if JSON mode - already emitted JSON summary)
    if !cli.quiet && !cli.json {
        if cli.dry_run {
            println!(
                "\n{}\n",
                "✓ Dry-run complete (no changes made)".green().bold()
            );
        } else {
            println!("\n{}\n", "✓ Sync complete".green().bold());
        }

        // File operations
        println!(
            "  Files scanned:     {}",
            stats.files_scanned.to_string().blue()
        );
        if cli.dry_run {
            println!(
                "  Would create:      {}",
                stats.files_created.to_string().yellow()
            );
            println!(
                "  Would update:      {}",
                stats.files_updated.to_string().yellow()
            );
            println!(
                "  Would skip:        {}",
                stats.files_skipped.to_string().bright_black()
            );
            if cli.delete {
                println!(
                    "  Would delete:      {}",
                    stats.files_deleted.to_string().red()
                );
            }

            // Dry-run byte statistics
            if stats.bytes_would_add > 0
                || stats.bytes_would_change > 0
                || stats.bytes_would_delete > 0
            {
                println!();
                if stats.bytes_would_add > 0 {
                    println!(
                        "  Bytes to add:      {}",
                        format_bytes(stats.bytes_would_add).yellow()
                    );
                }
                if stats.bytes_would_change > 0 {
                    println!(
                        "  Bytes to change:   {}",
                        format_bytes(stats.bytes_would_change).yellow()
                    );
                }
                if stats.bytes_would_delete > 0 && cli.delete {
                    println!(
                        "  Bytes to delete:   {}",
                        format_bytes(stats.bytes_would_delete).red()
                    );
                }
            }
        } else {
            if stats.files_created > 0 {
                println!(
                    "  Files created:     {}",
                    stats.files_created.to_string().green()
                );
            } else {
                println!(
                    "  Files created:     {}",
                    stats.files_created.to_string().bright_black()
                );
            }
            if stats.files_updated > 0 {
                println!(
                    "  Files updated:     {}",
                    stats.files_updated.to_string().yellow()
                );
            } else {
                println!(
                    "  Files updated:     {}",
                    stats.files_updated.to_string().bright_black()
                );
            }
            println!(
                "  Files skipped:     {}",
                stats.files_skipped.to_string().bright_black()
            );
            if cli.delete && stats.files_deleted > 0 {
                println!(
                    "  Files deleted:     {}",
                    stats.files_deleted.to_string().red()
                );
            } else if cli.delete {
                println!(
                    "  Files deleted:     {}",
                    stats.files_deleted.to_string().bright_black()
                );
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
            println!(
                "  Transfer rate:     {}",
                format!("{}/s", format_bytes(bytes_per_sec as u64)).cyan()
            );
        }

        println!(
            "  Duration:          {}",
            format_duration(stats.duration).cyan()
        );

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

        // Verification stats (if enabled)
        if verification_mode != cli::VerificationMode::Fast {
            println!();
            if stats.verification_failures > 0 {
                println!(
                    "  {}    {} verified, {} failed",
                    "Verification:".red(),
                    stats.files_verified.to_string().red(),
                    stats.verification_failures.to_string().red().bold()
                );
            } else if stats.files_verified > 0 {
                println!(
                    "  {}    {} files ({})",
                    "Verification:".green(),
                    stats.files_verified.to_string().green(),
                    match verification_mode {
                        cli::VerificationMode::Standard => "xxHash3",
                        cli::VerificationMode::Verify => "BLAKE3",
                        cli::VerificationMode::Paranoid => "BLAKE3+blocks",
                        cli::VerificationMode::Fast => unreachable!(),
                    }
                    .bright_black()
                );
            }
        }

        // Print performance summary if --perf is enabled
        if cli.perf {
            if let Some(metrics) = engine.get_performance_metrics() {
                metrics.print_summary();
            }
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
