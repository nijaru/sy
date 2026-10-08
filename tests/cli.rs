use std::ffi::OsStr;
use std::process::{Command, Output};
use sy::cli::{Arguments, Cli, SymlinkMode, VerifyMode};
use sy::compress::CompressionDetection;
use usage::test::{self as harness, Page};

fn parse(words: &[&str]) -> Cli {
    let words: Vec<_> = words.iter().map(OsStr::new).collect();
    Arguments::parse_from(&words).unwrap().into_cli().unwrap()
}

fn binary(words: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sy"))
        .args(words)
        .env("NO_COLOR", "1")
        .output()
        .unwrap()
}

#[test]
fn defaults_and_profile_only_invocations() {
    let cli = parse(&[]);
    assert!(cli.source.is_none() && cli.destination.is_none());
    assert_eq!(cli.parallel, 10);
    assert_eq!(cli.verbose, 0);
    assert_eq!(cli.suffix, "~");
    assert_eq!(cli.max_delete, "50%");
    assert_eq!(cli.verify, VerifyMode::No);
    assert_eq!(cli.compress, CompressionDetection::Never);
    assert_eq!(cli.links, SymlinkMode::Preserve);
    assert!(cli.backup.is_none());
    assert!(cli.validate().is_err());
    for words in [
        vec!["--list-profiles"],
        vec!["--show-profile", "daily"],
        vec!["--profile", "daily"],
    ] {
        assert!(parse(&words).validate().is_ok());
    }
}

#[test]
fn short_aliases_and_bundles() {
    type Switch = (&'static str, &'static str, fn(&Cli) -> bool);
    let switches: &[Switch] = &[
        ("-n", "--dry-run", |c| c.dry_run),
        ("-d", "--delete", |c| c.delete),
        ("-q", "--quiet", |c| c.quiet),
        ("-i", "--itemize-changes", |c| c.itemize_changes),
        ("-L", "--copy-links", |c| c.copy_links),
        ("-X", "--preserve-xattrs", |c| c.preserve_xattrs),
        ("-H", "--preserve-hardlinks", |c| c.preserve_hardlinks),
        ("-A", "--preserve-acls", |c| c.preserve_acls),
        ("-F", "--preserve-flags", |c| c.preserve_flags),
        ("-p", "--preserve-permissions", |c| c.preserve_permissions),
        ("-t", "--preserve-times", |c| c.preserve_times),
        ("-a", "--archive", |c| c.archive),
        ("-c", "--checksum", |c| c.checksum),
        ("-u", "--update", |c| c.update),
        ("-w", "--watch", |c| c.watch),
    ];
    for &(short, long, enabled) in switches {
        assert!(enabled(&parse(&[short])), "{short}");
        assert!(enabled(&parse(&[long])), "{long}");
    }
    let cli = parse(&["-avvnr", "-j20"]);
    assert!(cli.archive && cli.dry_run && cli.recursive);
    assert_eq!(cli.verbose, 2);
    assert_eq!(cli.parallel, 20);
    assert_eq!(parse(&["--verbose", "-vv"]).verbose, 3);
}

#[test]
fn supported_long_switches() {
    let cli = parse(&[
        "--diff",
        "--force-delete",
        "--perf",
        "--stats",
        "--human-readable",
        "--remove-source-files",
        "--existing",
        "--dirs",
        "--gitignore",
        "--exclude-vcs",
        "--ignore-times",
        "--size-only",
        "--ignore-existing",
        "--json",
        "--no-hooks",
        "--abort-on-hook-failure",
        "--list-profiles",
    ]);
    assert!(cli.diff && cli.force_delete && cli.perf && cli.stats && cli.human_readable);
    assert!(cli.remove_source_files && cli.existing && cli.dirs);
    assert!(cli.gitignore && cli.exclude_vcs && cli.ignore_times && cli.size_only);
    assert!(
        cli.ignore_existing
            && cli.json
            && cli.no_hooks
            && cli.abort_on_hook_failure
            && cli.list_profiles
    );
}

#[test]
fn values_cardinality_and_exclusion_order() {
    let cli = parse(&[
        "host:/source/",
        "/dest",
        "--min-size=1.5KB",
        "--max-size",
        "2M",
        "--bwlimit",
        "500KB",
        "--exclude",
        "*.log",
        "--exclude",
        "target/",
        "--include",
        "*.rs",
        "--include",
        "important.log",
        "--filter",
        "- *.tmp",
        "--filter",
        "+ keep.tmp",
        "--exclude-template",
        "rust",
        "--exclude-template",
        "node",
        "--exclude-from",
        "exclude.txt",
        "--include-from",
        "include.txt",
        "--backup-dir",
        "backups",
        "--suffix=.bak",
        "--timeout=30",
        "--contimeout",
        "10",
        "--parallel",
        "20",
        "--max-delete=1000",
        "--profile=daily",
        "--show-profile",
        "daily",
    ]);
    assert_eq!(cli.min_size, Some(1536));
    assert_eq!(cli.max_size, Some(2 * 1024 * 1024));
    assert_eq!(cli.bwlimit, Some(500 * 1024));
    assert_eq!(cli.exclude, ["*.log", "target/"]);
    assert_eq!(cli.include, ["*.rs", "important.log"]);
    assert_eq!(cli.filter, ["- *.tmp", "+ keep.tmp"]);
    assert_eq!(
        parse(&["--filter", "--delete=false"]).filter,
        ["--delete=false"]
    );
    assert_eq!(
        parse(&["--", "--delete=false"]).source.unwrap().path(),
        std::path::Path::new("--delete=false")
    );
    assert_eq!(cli.exclude_template, ["rust", "node"]);
    assert_eq!(
        cli.exclude_from.as_deref(),
        Some(std::path::Path::new("exclude.txt"))
    );
    assert_eq!(
        cli.include_from.as_deref(),
        Some(std::path::Path::new("include.txt"))
    );
    assert_eq!(
        cli.backup_dir.as_deref(),
        Some(std::path::Path::new("backups"))
    );
    assert_eq!(cli.suffix, ".bak");
    assert_eq!(cli.timeout, Some(30));
    assert_eq!(cli.contimeout, Some(10));
    assert_eq!(cli.parallel, 20);
    assert_eq!(cli.max_delete, "1000");
    assert_eq!(cli.profile.as_deref(), Some("daily"));
    assert_eq!(cli.show_profile.as_deref(), Some("daily"));
    assert!(cli.source.unwrap().has_trailing_slash());
}

#[test]
fn optional_values_and_enum_choices() {
    for flag in ["-b", "--backup"] {
        assert_eq!(parse(&[flag]).backup.as_deref(), Some("simple"));
    }
    for words in [vec!["--backup=none"], vec!["-b", "none"]] {
        assert_eq!(parse(&words).backup.as_deref(), Some("none"));
    }
    // Validation of arbitrary backup modes remains the engine's contract.
    assert_eq!(
        parse(&["--backup=custom"]).backup.as_deref(),
        Some("custom")
    );
    for flag in ["-z", "--compress"] {
        assert_eq!(parse(&[flag]).compress, CompressionDetection::Auto);
    }
    for (word, expected) in [
        ("auto", CompressionDetection::Auto),
        ("extension", CompressionDetection::Extension),
        ("always", CompressionDetection::Always),
        ("never", CompressionDetection::Never),
    ] {
        assert_eq!(parse(&["--compress", word]).compress, expected);
    }
    for (word, expected) in [
        ("no", VerifyMode::No),
        ("after", VerifyMode::After),
        ("only", VerifyMode::Only),
    ] {
        assert_eq!(parse(&["--verify", word]).verify, expected);
    }
    for (word, expected) in [
        ("preserve", SymlinkMode::Preserve),
        ("follow", SymlinkMode::Follow),
        ("skip", SymlinkMode::Skip),
    ] {
        assert_eq!(parse(&["--links", word]).links, expected);
    }
}

#[test]
fn parse_errors_are_nonzero_and_do_not_start_sync() {
    for words in [
        vec!["--unknown"],
        vec!["--delete=false"],
        vec!["--quiet=true"],
        vec!["--verbose=3"],
        vec!["--help=bad"],
        vec!["--version=bad"],
        vec!["--verify"],
        vec!["--verify=bad"],
        vec!["--compress=bad"],
        vec!["--links=bad"],
        vec!["--parallel=many"],
        vec!["--timeout=-1"],
        vec!["--min-size=NaN"],
        vec!["--max-size=100PB"],
        vec!["--bwlimit=18446744073709551616"],
        vec!["--exclude"],
        vec!["--quiet", "--quiet"],
        vec!["-j1", "--parallel=2"],
        vec!["source", "dest", "extra"],
    ] {
        let native: Vec<_> = words.iter().map(OsStr::new).collect();
        assert!(Arguments::parse_from(&native).is_err(), "{words:?}");
        let result = binary(&words);
        assert_eq!(result.status.code(), Some(2), "{words:?}");
        assert!(result.stdout.is_empty(), "{words:?}");
        assert!(!result.stderr.is_empty());
    }
}

#[test]
fn semantic_conflicts_remain_application_errors() {
    for words in [
        vec!["host:/src", "/dest", "--ignore-times", "--checksum"],
        vec!["host:/src", "/dest", "--size-only", "--checksum"],
        vec!["host:/src", "/dest", "--ignore-times", "--size-only"],
        vec!["host:/src", "/dest", "--diff"],
        vec!["host:/src", "/dest", "--min-size=2M", "--max-size=1M"],
        vec!["host:/src", "/dest", "--verify=only", "--watch"],
        vec!["host:/src", "/dest", "--verify=only", "--dry-run"],
        vec!["host:/src", "/dest", "--delete"],
        vec!["host:/src", "/dest", "--remove-source-files"],
        vec!["host:/src", "/dest", "--links=follow"],
    ] {
        assert!(parse(&words).validate().is_err(), "{words:?}");
    }
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().to_str().unwrap();
    let result = binary(&[source, "/dest", "--verify=only", "--delete"]);
    assert_eq!(result.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&result.stderr).contains("read-only mode"));
}

#[test]
fn help_version_spec_and_completions_share_declarations() {
    let long = harness::help(Arguments::spec(), &[], Page::Long);
    assert!(
        long.contains("--exclude-template")
            && long.contains("--min-size")
            && long.contains("--max-delete")
    );
    assert!(long.contains("--verify=after"));
    assert!(!long.contains("__serve") && !long.contains("__complete"));
    assert!(binary(&["--help", "--delete=false"]).status.success());
    for flag in ["-h", "--help"] {
        let result = binary(&[flag]);
        assert!(result.status.success());
        assert!(result.stderr.is_empty());
        assert!(String::from_utf8_lossy(&result.stdout).contains("--parallel"));
    }
    for flag in ["-V", "--version"] {
        let result = binary(&[flag]);
        assert!(result.status.success());
        assert_eq!(result.stdout, b"sy 0.4.1\n");
        assert!(result.stderr.is_empty());
    }
    let spec = binary(&["__usage_spec__"]);
    assert!(spec.status.success());
    assert_eq!(String::from_utf8(spec.stdout).unwrap(), Arguments::to_kdl());
    assert!(!Arguments::to_kdl().contains("__serve"));
    // No parser environment defaults existed before the migration.
    assert!(!Arguments::to_kdl().contains("env="));
    let completion = binary(&["__complete_word__", "--line", "sy --links "]);
    assert!(completion.status.success());
    assert!(String::from_utf8_lossy(&completion.stdout).contains("follow"));
    assert!(completion.stderr.is_empty());
    assert_eq!(
        harness::candidates(Arguments::spec(), "sy --links "),
        ["follow", "preserve", "skip"]
    );
    assert_eq!(
        harness::completion(Arguments::spec(), "sy --exclude-from ").files,
        Some(usage::test::Files::Any)
    );
    for shell in [
        usage::complete::Shell::Bash,
        usage::complete::Shell::Fish,
        usage::complete::Shell::Zsh,
    ] {
        assert!(Arguments::completion_script(shell).contains("sy"));
    }
}

#[test]
fn double_dash_preserves_hyphen_paths_and_no_public_subcommands() {
    let cli = parse(&["--", "-source/", "-dest"]);
    assert_eq!(cli.source.unwrap().path(), std::path::Path::new("-source/"));
    assert_eq!(
        cli.destination.unwrap().path(),
        std::path::Path::new("-dest")
    );
    assert_eq!(
        parse(&["help", "dest"]).source.unwrap().path(),
        std::path::Path::new("help")
    );
    let result = binary(&["__serve", "--help"]);
    assert_eq!(result.status.code(), Some(2));
    assert!(result.stdout.is_empty());
    assert!(String::from_utf8_lossy(&result.stderr).contains("does not accept"));
}

#[cfg(unix)]
#[test]
fn native_paths_survive_parser() {
    use std::ffi::OsString;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    let temp = tempfile::tempdir().unwrap();
    let source = temp
        .path()
        .join(OsString::from_vec(b"source-\xff".to_vec()));
    let destination = temp.path().join(OsString::from_vec(b"dest-\xfe".to_vec()));
    let cli = Arguments::parse_from(&[source.as_os_str(), destination.as_os_str()])
        .unwrap()
        .into_cli()
        .unwrap();
    assert_eq!(cli.source.unwrap().path(), source);
    assert_eq!(cli.destination.unwrap().path(), destination);
    // Even on filesystems that reject these bytes (e.g. APFS), the real entry
    // point must get past parsing and report the missing native source.
    let output = Command::new(env!("CARGO_BIN_EXE_sy"))
        .args([
            source.as_os_str(),
            destination.as_os_str(),
            OsStr::new("--quiet"),
            OsStr::new("--no-hooks"),
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("Source path does not exist"));

    let root = OsString::from_vec(b"user@host:/root-\xff/".to_vec());
    let backup = OsString::from_vec(b"--backup-dir=backup-\xfe".to_vec());
    let exclude = OsString::from_vec(b"exclude-\xfe".to_vec());
    let cli = Arguments::parse_from(&[
        &root,
        destination.as_os_str(),
        &backup,
        OsStr::new("--exclude-from"),
        &exclude,
    ])
    .unwrap()
    .into_cli()
    .unwrap();
    let remote = cli.source.unwrap();
    assert!(remote.is_remote() && remote.has_trailing_slash());
    assert_eq!(remote.path().as_os_str().as_bytes(), b"/root-\xff/");
    assert_eq!(cli.destination.unwrap().path(), destination);
    assert_eq!(
        cli.backup_dir.unwrap().as_os_str().as_bytes(),
        b"backup-\xfe"
    );
    assert_eq!(
        cli.exclude_from.unwrap().as_os_str().as_bytes(),
        b"exclude-\xfe"
    );
    for bytes in [
        b"s3://bucket/\xff".as_slice(),
        b"gs://bucket/\xff",
        b"user@host-\xff:/root",
    ] {
        let invalid = OsString::from_vec(bytes.to_vec());
        assert!(Arguments::parse_from(&[&invalid])
            .unwrap()
            .into_cli()
            .is_err());
        let result = Command::new(env!("CARGO_BIN_EXE_sy"))
            .arg(invalid)
            .output()
            .unwrap();
        assert_eq!(result.status.code(), Some(2));
        assert!(result.stdout.is_empty());
    }
}

// APFS rejects non-UTF-8 filenames. Linux can exercise byte preservation all
// the way through the real copy entrypoint rather than only the argv boundary.
#[cfg(target_os = "linux")]
#[test]
fn native_paths_survive_real_copy() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    let temp = tempfile::tempdir().unwrap();
    let source = temp
        .path()
        .join(OsString::from_vec(b"source-\xff".to_vec()));
    let destination = temp.path().join(OsString::from_vec(b"dest-\xfe".to_vec()));
    std::fs::write(&source, b"native paths\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_sy"))
        .args([
            source.as_os_str(),
            destination.as_os_str(),
            OsStr::new("--quiet"),
            OsStr::new("--no-hooks"),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(std::fs::read(&destination).unwrap(), b"native paths\n");
}
