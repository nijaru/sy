//! Local execution: local source root -> local destination root.
//!
//! Uses the shared controller pipeline — shared preflight (one reconcile pass,
//! single-pass `DeleteTracker`), bounded concurrent execution, reverse-order
//! deletes, journal-owned finalize — with `LocalSyncExecutor` on both sides.
//! Native fast paths (clonefile, reflink+patch, sparse copy) live in the
//! transfer layer.

use super::policy::*;
use crate::cli::SymlinkMode;
use crate::error::{Result, SyncError};
use crate::sync::scanner::ScanOptions;
use crate::sync::{SyncConfig, SyncStats};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::time::Instant;
use sy::engine::controller::{
    preflight_sync_scoped_with_content, preview_sync, ControllerError, SyncController, SyncSummary,
};
use sy::engine::domain::{Entry, SyncScope};
use sy::engine::reconcile::OrderedReconciler;
use sy::engine::scheduler::{ResourceBudget, Scheduler};
use sy::remote::local_executor::LocalSyncExecutor;

pub(super) async fn run(
    source_root: &Path,
    destination_root: &Path,
    config: &SyncConfig,
    scan_options: ScanOptions,
    scope: SyncScope,
) -> Result<SyncStats> {
    // Controller journals are created before either entry stream is polled.
    // Refuse unsafe scratch configuration before spawning even the dest scan.
    if scope == SyncScope::Tree {
        sy::endpoint::local_entry_scan::validate_scratch_location(source_root.to_path_buf())
            .await
            .map_err(map_io)?;
    }
    let reporter = std::sync::Arc::new(sy::sync::output::SyncReporter::new(
        config.itemize_changes,
        config.json,
        config.quiet,
        config.perf,
    ));
    let scan_started = Instant::now();
    reporter.start(source_root, destination_root);

    // A missing tree destination is an empty observation during dry-run, not
    // an excuse to create it. Observe before admitting either scan so an early
    // filesystem failure cannot detach a source scanner. Selected-leaf scans
    // already represent missing parents without creating them.
    let destination_absent = if scope == SyncScope::Tree {
        let root = destination_root.to_path_buf();
        let dry_run = config.dry_run;
        tokio::task::spawn_blocking(move || match std::fs::metadata(&root) {
            Ok(_) => Ok(false),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && dry_run => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir_all(root)?;
                Ok(false)
            }
            Err(error) => Err(error),
        })
        .await
        .map_err(map_io)?
        .map_err(map_io)?
    } else {
        false
    };

    // Both scans are local. The source walk honors the ignore rules where
    // the files live; the destination scan stays COMPLETE (gitignore never
    // narrows what reconciliation sees) — the same boundary as every other
    // direction.
    let (source, destination) = match &scope {
        SyncScope::Tree => (
            sy::endpoint::local_entry_scan::local_entry_stream(
                source_root.to_path_buf(),
                source_scan_request(config, scan_options),
            ),
            if destination_absent {
                sy::engine::reconcile::EntryStream::new(futures::stream::empty())
            } else {
                sy::endpoint::local_entry_scan::local_entry_stream(
                    destination_root.to_path_buf(),
                    destination_scan_request(config),
                )
            },
        ),
        SyncScope::SelectedLeaf {
            source,
            destination,
        } => (
            sy::endpoint::local_entry_scan::selected_leaf_stream(
                source_root.to_path_buf(),
                source.clone(),
                source_scan_request(config, scan_options),
                false,
            ),
            sy::endpoint::local_entry_scan::selected_leaf_stream(
                destination_root.to_path_buf(),
                destination.clone(),
                destination_scan_request(config),
                true,
            ),
        ),
    };
    let source = filtered_source_stream(source, config.filter_engine.clone());
    let tree_scope = scope == SyncScope::Tree;

    let min_size = config.min_size;
    let max_size = config.max_size;
    let skip_symlinks = config.preserve.symlink_mode == SymlinkMode::Skip;
    let delete_filter = config.filter_engine.clone();
    let max_depth = selection_max_depth(config, scan_options);
    let include_git_dir = scan_options.include_git_dir;
    // Source-derived ignore scope: destination-only paths the source rules
    // would ignore are protected from deletion (see `engine::ignore_scope`).
    let ignore_scope = std::sync::Arc::new(std::sync::Mutex::new(
        sy::engine::ignore_scope::SourceIgnoreScope::new(
            source_root,
            scan_options.respect_gitignore,
        ),
    ));

    let mut plan = {
        let source_root_owned = source_root.to_path_buf();
        let destination_root_owned = destination_root.to_path_buf();
        preflight_sync_scoped_with_content(
            OrderedReconciler::with_scope(source, destination, scope.clone()),
            comparison_policy(
                config,
                crate::fs_util::namespace_semantics(destination_root),
            ),
            delete_policy(&config.delete),
            move |entry| {
                entry_in_size_scope(entry, min_size, max_size)
                    && entry_selected_by_symlink_mode(entry, skip_symlinks)
            },
            move |entry| {
                tree_scope
                    && delete_filter.should_include(entry.path.as_path(), entry.is_directory())
                    && entry_in_depth_scope(entry, max_depth)
                    && entry_in_vcs_scope(entry, include_git_dir)
                    && entry_not_source_ignored(&ignore_scope, entry)
            },
            move |source: Entry, destination: Entry| {
                let source_root = source_root_owned.clone();
                let destination_root = destination_root_owned.clone();
                async move {
                    let source_endpoint = sy::endpoint::local::LocalEndpoint::new(source_root);
                    let destination_endpoint =
                        sy::endpoint::local::LocalEndpoint::new(destination_root);
                    let source_hash = observed_hash(
                        &source_endpoint,
                        &source,
                        config.preserve.symlink_mode == SymlinkMode::Follow,
                    )
                    .await
                    .map_err(|error| ControllerError::backend("content comparison", error))?;
                    let destination_hash =
                        observed_hash(&destination_endpoint, &destination, false)
                            .await
                            .map_err(|error| {
                                ControllerError::backend("content comparison", error)
                            })?;
                    Ok(source_hash == destination_hash)
                }
            },
        )
        .await
        .map_err(map_controller_error)?
    };

    if let SyncScope::SelectedLeaf { source, .. } = &scope {
        super::selected::validate_effects(&mut plan, source_root, source, destination_root, config)
            .await?;
    }

    if config.preserve.hardlinks {
        plan.validate_hardlink_bytes(|commitment| {
            let (root, follow) = match &commitment {
                sy::engine::hardlink_preflight::ByteCommitment::Source(_) => (
                    source_root,
                    config.preserve.symlink_mode == SymlinkMode::Follow,
                ),
                sy::engine::hardlink_preflight::ByteCommitment::Destination(_) => {
                    (destination_root, false)
                }
            };
            async move {
                let endpoint = sy::endpoint::local::LocalEndpoint::new(root.to_path_buf());
                observed_hash(&endpoint, commitment.entry(), follow)
                    .await
                    .map_err(|error| ControllerError::backend("hardlink byte commitment", error))
            }
        })
        .await
        .map_err(map_controller_error)?;
    }

    if config.dry_run {
        let diff_mode = config.diff_mode;
        let preview = preview_sync(plan, |item| {
            if diff_mode {
                emit_diff_line(item);
            }
        })
        .await
        .map_err(map_controller_error)?;
        let stats = preview_stats(preview)?;
        finish_report(
            &reporter,
            &stats,
            scan_started.elapsed(),
            std::time::Duration::ZERO,
        );
        return Ok(stats);
    }

    let max_in_flight = NonZeroUsize::new(config.max_concurrent).ok_or_else(|| {
        SyncError::Config("parallel transfer count must be greater than zero".to_string())
    })?;
    let active_files = u32::try_from(config.max_concurrent).map_err(|_| {
        SyncError::Config("parallel transfer count exceeds v3 scheduler range".to_string())
    })?;
    let scheduler = Scheduler::new(ResourceBudget {
        active_files,
        ..ResourceBudget::default()
    })
    .map_err(map_io)?;
    let rate_limiter = config.bwlimit.map(|limit| {
        std::sync::Arc::new(std::sync::Mutex::new(
            sy::sync::ratelimit::RateLimiter::new(limit),
        ))
    });
    let executor = LocalSyncExecutor::new(
        source_root.to_path_buf(),
        destination_root.to_path_buf(),
        scheduler,
    )
    .with_backup(
        config.backup.is_some(),
        backup_dir(config, destination_root),
        config.suffix.clone(),
    )
    .with_follow_symlinks(config.preserve.symlink_mode == SymlinkMode::Follow)
    .with_rate_limiter(rate_limiter)
    .with_remove_source_files(config.remove_source_files)
    .with_verify_on_write(config.verification.verify_on_write)
    .with_hardlinks(config.preserve.hardlinks)
    .with_xattrs(config.preserve.xattrs)
    .with_acls(config.preserve.acls)
    .with_bsd_flags(config.preserve.flags)
    .with_reporter(Some(reporter.clone()));

    let scan_elapsed = scan_started.elapsed();
    let transfer_started = Instant::now();
    let summary: SyncSummary = SyncController::new(executor, max_in_flight)
        .execute(plan)
        .await
        .map_err(map_controller_error)?;
    let mut stats = summary_stats(summary)?;
    // --verify's staged verification is fail-fast on this path: a successful
    // run verified every transferred file.
    if config.verification.verify_on_write {
        stats.files_verified = usize::try_from(summary.files_transferred).map_err(|_| {
            SyncError::Config("v3 verified counter exceeds platform usize".to_string())
        })?;
    }
    stats.duration = scan_elapsed + transfer_started.elapsed();
    finish_report(&reporter, &stats, scan_elapsed, transfer_started.elapsed());
    Ok(stats)
}

/// --backup-dir is local: absolute honored as-is (the local engine's rule);
/// relative anchored to the destination root.
pub(super) fn backup_dir(config: &SyncConfig, destination_root: &Path) -> Option<PathBuf> {
    let dir = config.backup_dir.as_ref()?;
    if dir.is_absolute() {
        Some(dir.clone())
    } else {
        Some(destination_root.join(dir))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn hardlink_preflight_byte_commitments() {
        super::super::hardlink_tests::assert_byte_commitments(
            |source, destination, config| async move {
                run(
                    &source,
                    &destination,
                    &config,
                    ScanOptions::default(),
                    SyncScope::Tree,
                )
                .await
            },
        )
        .await;
    }
    use crate::sync::scanner::ScanOptions;
    use tempfile::TempDir;

    /// -H/--preserve-hardlinks locally: one transfer moves the
    /// representative's bytes; the other member links to it. Both
    /// destination paths share one inode.
    #[tokio::test]
    async fn hardlink_group_shares_one_transfer_locally() {
        let source_root = TempDir::new().unwrap();
        let destination_root = TempDir::new().unwrap();
        std::fs::write(source_root.path().join("first"), b"shared-bytes").unwrap();
        std::fs::hard_link(
            source_root.path().join("first"),
            source_root.path().join("second"),
        )
        .unwrap();

        let mut config = SyncConfig::test_default();
        config.preserve.hardlinks = true;
        let stats = run(
            source_root.path(),
            destination_root.path(),
            &config,
            ScanOptions::default(),
            SyncScope::Tree,
        )
        .await
        .unwrap();

        assert_eq!(stats.files_created, 2);
        assert_eq!(stats.bytes_transferred, b"shared-bytes".len() as u64);
        assert_eq!(
            std::fs::read(destination_root.path().join("first")).unwrap(),
            b"shared-bytes"
        );
        assert_eq!(
            std::fs::read(destination_root.path().join("second")).unwrap(),
            b"shared-bytes"
        );
        use std::os::unix::fs::MetadataExt;
        let first = std::fs::metadata(destination_root.path().join("first")).unwrap();
        let second = std::fs::metadata(destination_root.path().join("second")).unwrap();
        assert_eq!(first.ino(), second.ino());
    }

    #[tokio::test]
    async fn hardlink_remove_source_files_removes_group_after_publication_locally() {
        let source_root = TempDir::new().unwrap();
        let destination_root = TempDir::new().unwrap();
        std::fs::write(source_root.path().join("first"), b"shared-bytes").unwrap();
        std::fs::hard_link(
            source_root.path().join("first"),
            source_root.path().join("second"),
        )
        .unwrap();
        std::fs::hard_link(
            source_root.path().join("first"),
            source_root.path().join("third"),
        )
        .unwrap();

        let mut config = SyncConfig::test_default();
        config.preserve.hardlinks = true;
        config.remove_source_files = true;
        let stats = run(
            source_root.path(),
            destination_root.path(),
            &config,
            ScanOptions::default(),
            SyncScope::Tree,
        )
        .await
        .unwrap();

        assert_eq!(stats.files_created, 3);
        assert!(!source_root.path().join("first").exists());
        assert!(!source_root.path().join("second").exists());
        assert!(!source_root.path().join("third").exists());
        assert_eq!(
            std::fs::read(destination_root.path().join("first")).unwrap(),
            b"shared-bytes"
        );
        assert_eq!(
            std::fs::read(destination_root.path().join("second")).unwrap(),
            b"shared-bytes"
        );
        assert_eq!(
            std::fs::read(destination_root.path().join("third")).unwrap(),
            b"shared-bytes"
        );
    }

    /// -X/--preserve-xattrs locally: source attributes are mirrored onto the
    /// destination after the transfer, and a later pass clears values the
    /// source dropped.
    #[cfg(unix)]
    #[tokio::test]
    async fn xattrs_are_mirrored_and_stale_values_removed_locally() {
        let source_root = TempDir::new().unwrap();
        let destination_root = TempDir::new().unwrap();
        let name = "user.sy-e2e";
        let stale = "user.sy-stale";
        let source_file = source_root.path().join("file");
        std::fs::write(&source_file, b"one").unwrap();
        xattr::set(&source_file, name, b"first").unwrap();
        xattr::set(&source_file, stale, b"gone").unwrap();

        let mut config = SyncConfig::test_default();
        config.preserve.xattrs = true;
        run(
            source_root.path(),
            destination_root.path(),
            &config,
            ScanOptions::default(),
            SyncScope::Tree,
        )
        .await
        .unwrap();

        let destination_file = destination_root.path().join("file");
        assert_eq!(
            xattr::get(&destination_file, name).unwrap(),
            Some(b"first".to_vec())
        );
        assert_eq!(
            xattr::get(&destination_file, stale).unwrap(),
            Some(b"gone".to_vec())
        );

        std::fs::write(&source_file, b"one-two").unwrap();
        xattr::remove(&source_file, stale).unwrap();
        xattr::set(&source_file, name, b"second").unwrap();
        run(
            source_root.path(),
            destination_root.path(),
            &config,
            ScanOptions::default(),
            SyncScope::Tree,
        )
        .await
        .unwrap();

        assert_eq!(
            xattr::get(&destination_file, name).unwrap(),
            Some(b"second".to_vec())
        );
        assert!(xattr::get(&destination_file, stale).unwrap().is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn xattrs_are_reconciled_for_quick_unchanged_files_and_directories_locally() {
        let source_root = TempDir::new().unwrap();
        let destination_root = TempDir::new().unwrap();
        let name = "user.sy-unchanged";
        let stale = "user.sy-stale";
        std::fs::create_dir(source_root.path().join("dir")).unwrap();
        std::fs::write(source_root.path().join("file"), b"same").unwrap();
        xattr::set(source_root.path().join("file"), name, b"source-file").unwrap();
        xattr::set(source_root.path().join("dir"), name, b"source-dir").unwrap();

        let mut config = SyncConfig::test_default();
        config.preserve.xattrs = true;
        run(
            source_root.path(),
            destination_root.path(),
            &config,
            ScanOptions::default(),
            SyncScope::Tree,
        )
        .await
        .unwrap();

        let destination_file = destination_root.path().join("file");
        let destination_dir = destination_root.path().join("dir");
        xattr::set(&destination_file, name, b"wrong").unwrap();
        xattr::set(&destination_file, stale, b"remove").unwrap();
        xattr::remove(source_root.path().join("dir"), name).unwrap();
        xattr::set(&destination_dir, name, b"remove-dir").unwrap();

        run(
            source_root.path(),
            destination_root.path(),
            &config,
            ScanOptions::default(),
            SyncScope::Tree,
        )
        .await
        .unwrap();

        assert_eq!(
            xattr::get(&destination_file, name).unwrap(),
            Some(b"source-file".to_vec())
        );
        assert!(xattr::get(&destination_file, stale).unwrap().is_none());
        assert!(xattr::get(&destination_dir, name).unwrap().is_none());
    }

    #[cfg(all(unix, feature = "acl"))]
    #[tokio::test]
    async fn acls_are_reconciled_for_quick_unchanged_files_and_directories_locally() {
        let source_root = TempDir::new().unwrap();
        let destination_root = TempDir::new().unwrap();
        std::fs::create_dir(source_root.path().join("dir")).unwrap();
        std::fs::write(source_root.path().join("file"), b"same").unwrap();

        let source_file = source_root.path().join("file");
        let source_dir = source_root.path().join("dir");
        let base_file_acl = exacl::getfacl(&source_file, None).unwrap();
        let base_dir_acl = exacl::getfacl(&source_dir, None).unwrap();
        // SAFETY: `getuid` has no pointer arguments or other preconditions.
        let uid = unsafe { libc::getuid() };
        let add_acl = |path: &std::path::Path| {
            let mut acl = exacl::getfacl(path, None).unwrap();
            acl.push(exacl::AclEntry::allow_user(
                &uid.to_string(),
                exacl::Perm::READ,
                exacl::Flag::empty(),
            ));
            exacl::setfacl(&[path], &acl, None).unwrap();
            exacl::to_string(&exacl::getfacl(path, None).unwrap()).unwrap()
        };
        let source_file_acl = add_acl(&source_file);
        let source_dir_acl = add_acl(&source_dir);

        let mut config = SyncConfig::test_default();
        config.preserve.acls = true;
        run(
            source_root.path(),
            destination_root.path(),
            &config,
            ScanOptions::default(),
            SyncScope::Tree,
        )
        .await
        .unwrap();

        let destination_file = destination_root.path().join("file");
        let destination_dir = destination_root.path().join("dir");
        exacl::setfacl(&[destination_file.as_path()], &base_file_acl, None).unwrap();
        exacl::setfacl(&[destination_dir.as_path()], &base_dir_acl, None).unwrap();

        run(
            source_root.path(),
            destination_root.path(),
            &config,
            ScanOptions::default(),
            SyncScope::Tree,
        )
        .await
        .unwrap();

        assert_eq!(
            exacl::to_string(&exacl::getfacl(&destination_file, None).unwrap()).unwrap(),
            source_file_acl
        );
        assert_eq!(
            exacl::to_string(&exacl::getfacl(&destination_dir, None).unwrap()).unwrap(),
            source_dir_acl
        );
    }

    #[tokio::test]
    async fn copy_links_preserves_requested_metadata_on_followed_files() {
        let source_root = TempDir::new().unwrap();
        let target_root = TempDir::new().unwrap();
        let destination_root = TempDir::new().unwrap();
        let source_file = source_root.path().join("file");
        let target_file = target_root.path().join("target");
        std::fs::write(&target_file, b"followed content").unwrap();
        std::os::unix::fs::symlink(&target_file, &source_file).unwrap();
        xattr::set(&target_file, "user.sy-copy-links", b"preserved").unwrap();
        #[cfg(feature = "acl")]
        let expected_acl = {
            let mut acl = exacl::getfacl(&target_file, None).unwrap();
            // SAFETY: `getuid` has no pointer arguments or other preconditions.
            let uid = unsafe { libc::getuid() };
            acl.push(exacl::AclEntry::allow_user(
                &uid.to_string(),
                exacl::Perm::READ,
                exacl::Flag::empty(),
            ));
            exacl::setfacl(&[&target_file], &acl, None).unwrap();
            exacl::to_string(&exacl::getfacl(&target_file, None).unwrap())
                .unwrap()
                .trim()
                .to_string()
        };

        let mut config = SyncConfig::test_default();
        config.preserve.symlink_mode = SymlinkMode::Follow;
        config.preserve.xattrs = true;
        #[cfg(feature = "acl")]
        {
            config.preserve.acls = true;
        }
        run(
            source_root.path(),
            destination_root.path(),
            &config,
            ScanOptions::default(),
            SyncScope::Tree,
        )
        .await
        .unwrap();

        assert_eq!(
            xattr::get(destination_root.path().join("file"), "user.sy-copy-links").unwrap(),
            Some(b"preserved".to_vec())
        );
        assert_eq!(
            std::fs::read(destination_root.path().join("file")).unwrap(),
            b"followed content"
        );
        #[cfg(feature = "acl")]
        assert_eq!(
            exacl::to_string(&exacl::getfacl(destination_root.path().join("file"), None).unwrap())
                .unwrap()
                .trim(),
            expected_acl
        );
    }

    /// -A/--preserve-acls locally: the source list is mirrored onto the
    /// destination after the transfer, and a later pass clears entries the
    /// source dropped.
    #[cfg(all(unix, feature = "acl"))]
    #[tokio::test]
    async fn acls_are_mirrored_and_cleared_locally() {
        let source_root = TempDir::new().unwrap();
        let destination_root = TempDir::new().unwrap();
        let source_file = source_root.path().join("file");
        std::fs::write(&source_file, b"one").unwrap();
        let base_text = exacl::to_string(&exacl::getfacl(&source_file, None).unwrap()).unwrap();
        let uid = unsafe { libc::getuid() };
        let mut planted = exacl::getfacl(&source_file, None).unwrap();
        planted.push(exacl::AclEntry::allow_user(
            &uid.to_string(),
            exacl::Perm::READ,
            exacl::Flag::empty(),
        ));
        exacl::setfacl(&[&source_file], &planted, None).unwrap();
        let first_text = exacl::to_string(&exacl::getfacl(&source_file, None).unwrap()).unwrap();
        assert_ne!(first_text, base_text);

        let mut config = SyncConfig::test_default();
        config.preserve.acls = true;
        run(
            source_root.path(),
            destination_root.path(),
            &config,
            ScanOptions::default(),
            SyncScope::Tree,
        )
        .await
        .unwrap();

        let destination_file = destination_root.path().join("file");
        let mirrored = exacl::to_string(&exacl::getfacl(&destination_file, None).unwrap()).unwrap();
        assert_eq!(mirrored, first_text);

        std::fs::write(&source_file, b"one-two").unwrap();
        let base = exacl::from_str(&base_text).unwrap();
        exacl::setfacl(&[&source_file], &base, None).unwrap();
        run(
            source_root.path(),
            destination_root.path(),
            &config,
            ScanOptions::default(),
            SyncScope::Tree,
        )
        .await
        .unwrap();

        let cleared = exacl::to_string(&exacl::getfacl(&destination_file, None).unwrap()).unwrap();
        assert_eq!(cleared, base_text);
    }

    /// -F/--preserve-flags locally: the source flag word is mirrored onto
    /// the destination after the transfer, and a later pass clears flags
    /// the source dropped (macOS only).
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn bsd_flags_are_mirrored_and_cleared_locally() {
        use std::ffi::CString;
        use std::os::macos::fs::MetadataExt;
        use std::os::unix::ffi::OsStrExt;

        fn set_flags(path: &std::path::Path, flags: u32) {
            let cpath = CString::new(path.as_os_str().as_bytes()).unwrap();
            // SAFETY: `cpath` is a live NUL-terminated pathname borrowed for
            // the duration of the call.
            let ret = unsafe { libc::chflags(cpath.as_ptr(), flags) };
            assert_eq!(ret, 0);
        }

        let source_root = TempDir::new().unwrap();
        let destination_root = TempDir::new().unwrap();
        let source_file = source_root.path().join("file");
        std::fs::write(&source_file, b"one").unwrap();
        set_flags(&source_file, 0x1);

        let mut config = SyncConfig::test_default();
        config.preserve.flags = true;
        run(
            source_root.path(),
            destination_root.path(),
            &config,
            ScanOptions::default(),
            SyncScope::Tree,
        )
        .await
        .unwrap();

        let destination_file = destination_root.path().join("file");
        assert_eq!(
            std::fs::metadata(&destination_file).unwrap().st_flags(),
            0x1
        );

        std::fs::write(&source_file, b"one-two").unwrap();
        set_flags(&source_file, 0);
        run(
            source_root.path(),
            destination_root.path(),
            &config,
            ScanOptions::default(),
            SyncScope::Tree,
        )
        .await
        .unwrap();

        assert_eq!(std::fs::metadata(&destination_file).unwrap().st_flags(), 0);
    }

    #[tokio::test]
    async fn remove_source_files_moves_committed_and_retains_quick_check_unchanged() {
        let source_root = TempDir::new().unwrap();
        let destination_root = TempDir::new().unwrap();

        let created = source_root.path().join("created.txt");
        std::fs::write(&created, b"new-content").unwrap();

        let unchanged_src = source_root.path().join("unchanged.txt");
        let unchanged_dst = destination_root.path().join("unchanged.txt");
        std::fs::write(&unchanged_src, b"same-content").unwrap();
        std::fs::write(&unchanged_dst, b"same-content").unwrap();

        let meta = std::fs::metadata(&unchanged_src).unwrap();
        let mtime = filetime::FileTime::from_last_modification_time(&meta);
        filetime::set_file_times(&unchanged_dst, mtime, mtime).unwrap();

        let mut config = SyncConfig::test_default();
        config.remove_source_files = true;

        run(
            source_root.path(),
            destination_root.path(),
            &config,
            ScanOptions::default(),
            SyncScope::Tree,
        )
        .await
        .unwrap();

        // Created file was transferred and committed, so source is removed.
        assert!(!created.exists());
        assert_eq!(
            std::fs::read(destination_root.path().join("created.txt")).unwrap(),
            b"new-content"
        );

        // Unchanged file under quick check was not content-verified, so source MUST be retained.
        assert!(
            unchanged_src.exists(),
            "quick-check equality alone must not authorize source removal"
        );
        assert_eq!(std::fs::read(&unchanged_dst).unwrap(), b"same-content");
    }

    #[tokio::test]
    async fn remove_source_files_with_checksum_moves_content_verified_unchanged() {
        let source_root = TempDir::new().unwrap();
        let destination_root = TempDir::new().unwrap();

        let unchanged_src = source_root.path().join("unchanged.txt");
        let unchanged_dst = destination_root.path().join("unchanged.txt");
        std::fs::write(&unchanged_src, b"same-content").unwrap();
        std::fs::write(&unchanged_dst, b"same-content").unwrap();

        let meta = std::fs::metadata(&unchanged_src).unwrap();
        let mtime = filetime::FileTime::from_last_modification_time(&meta);
        filetime::set_file_times(&unchanged_dst, mtime, mtime).unwrap();

        let mut config = SyncConfig::test_default();
        config.remove_source_files = true;
        config.comparison.checksum = true;

        run(
            source_root.path(),
            destination_root.path(),
            &config,
            ScanOptions::default(),
            SyncScope::Tree,
        )
        .await
        .unwrap();

        // With --checksum, strong content parity authorizes a VerifiedExistingDestinationReceipt.
        assert!(
            !unchanged_src.exists(),
            "checksum-verified unchanged file must be removed"
        );
        assert_eq!(std::fs::read(&unchanged_dst).unwrap(), b"same-content");
    }
}
