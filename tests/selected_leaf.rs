//! Real operand regressions: renamed leaves must obey the shared engine policy.
use std::path::Path;
use std::process::{Command, Output};
use tempfile::tempdir;

fn run(source: &Path, destination: &Path, flags: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sy"))
        .arg(source)
        .arg(destination)
        .args(flags)
        .env("NO_COLOR", "1")
        .output()
        .unwrap()
}
fn success(output: Output) -> Output {
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}
fn event(output: &Output, kind: &str) -> serde_json::Value {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|event| event["type"] == kind)
        .unwrap()
}

#[cfg(unix)]
#[test]
fn preserved_symlink_loops_do_not_require_target_resolution() {
    for (source_loop, existing_parent, destination_loop) in [
        (true, false, false),
        (true, true, false),
        (false, true, true),
    ] {
        let root = tempdir().unwrap();
        let source = root.path().join("loop");
        let destination = root.path().join("destination/copy");
        std::fs::write(root.path().join("payload"), b"untouched target").unwrap();
        let target = if source_loop { "loop" } else { "payload" };
        std::os::unix::fs::symlink(target, &source).unwrap();
        if existing_parent {
            std::fs::create_dir(destination.parent().unwrap()).unwrap();
        }
        if destination_loop {
            std::os::unix::fs::symlink("copy", &destination).unwrap();
        }
        let before = std::fs::symlink_metadata(&source).unwrap();
        success(run(&source, &destination, &[]));
        assert_eq!(std::fs::read_link(&destination).unwrap(), Path::new(target));
        assert_eq!(std::fs::read_link(&source).unwrap(), Path::new(target));
        use std::os::unix::fs::MetadataExt;
        let after = std::fs::symlink_metadata(&source).unwrap();
        assert_eq!(
            (before.ino(), before.ctime(), before.ctime_nsec()),
            (after.ino(), after.ctime(), after.ctime_nsec())
        );
        assert_eq!(
            std::fs::read(root.path().join("payload")).unwrap(),
            b"untouched target"
        );
    }
}

#[test]
fn renamed_leaf_obeys_selection_and_comparison() {
    for flags in [
        vec!["--ignore-existing"],
        vec!["--update"],
        vec!["--size-only"],
        vec!["--exclude=original"],
        vec!["--max-size=3"],
    ] {
        let root = tempdir().unwrap();
        let source = root.path().join("original");
        let destination = root.path().join("renamed");
        std::fs::write(&source, b"aaaa").unwrap();
        std::fs::write(&destination, b"bbbb").unwrap();
        filetime::set_file_mtime(&source, filetime::FileTime::from_unix_time(100, 0)).unwrap();
        filetime::set_file_mtime(&destination, filetime::FileTime::from_unix_time(200, 0)).unwrap();
        success(run(&source, &destination, &flags));
        assert_eq!(std::fs::read(&destination).unwrap(), b"bbbb", "{flags:?}");
        assert_eq!(std::fs::read(&source).unwrap(), b"aaaa");
    }
    let root = tempdir().unwrap();
    let source = root.path().join("original");
    let destination = root.path().join("renamed");
    std::fs::write(&source, b"aaaa").unwrap();
    success(run(&source, &destination, &["--existing"]));
    assert!(!destination.exists());
    let nested = root.path().join("absent-parent/renamed");
    success(run(&source, &nested, &["--exclude=original"]));
    assert!(!nested.parent().unwrap().exists());
    success(run(&source, &nested, &[]));
    assert_eq!(std::fs::read(&nested).unwrap(), b"aaaa");
    std::fs::write(&destination, b"bbbb").unwrap();
    let time = filetime::FileTime::from_unix_time(100, 0);
    filetime::set_file_mtime(&source, time).unwrap();
    filetime::set_file_mtime(&destination, time).unwrap();
    let output = success(run(
        &source,
        &destination,
        &["--checksum", "--json", "--verify=after"],
    ));
    assert_eq!(std::fs::read(&destination).unwrap(), b"aaaa");
    let summary = event(&output, "summary");
    assert_eq!(summary["files_updated"], 1);
    assert_eq!(summary["files_verified"], 1);
    assert_eq!(event(&output, "update")["path"], "renamed");
}

#[test]
fn leaf_scope_preserves_parent_siblings_and_excluded_target() {
    let source_root = tempdir().unwrap();
    let destination_root = tempdir().unwrap();
    let source = source_root.path().join("original");
    let destination = destination_root.path().join("renamed");
    #[cfg(unix)]
    let _source_socket =
        std::os::unix::net::UnixListener::bind(source_root.path().join("unselected-socket"))
            .unwrap();
    #[cfg(unix)]
    let _destination_socket =
        std::os::unix::net::UnixListener::bind(destination_root.path().join("unselected-socket"))
            .unwrap();
    std::fs::write(&source, b"selected").unwrap();
    std::fs::write(source_root.path().join("source-sibling"), b"not selected").unwrap();
    std::fs::write(&destination, b"old").unwrap();
    std::fs::write(destination_root.path().join("sibling"), b"keep").unwrap();
    success(run(
        &source,
        &destination,
        &["--exclude=original", "--delete", "--force-delete"],
    ));
    assert_eq!(std::fs::read(&destination).unwrap(), b"old");
    success(run(&source, &destination, &["--delete", "--force-delete"]));
    assert_eq!(std::fs::read(&destination).unwrap(), b"selected");
    assert_eq!(
        std::fs::read(destination_root.path().join("sibling")).unwrap(),
        b"keep"
    );
    assert!(!destination_root.path().join("source-sibling").exists());
    let directory_target = tempdir().unwrap();
    std::fs::create_dir(directory_target.path().join("original")).unwrap();
    std::fs::write(directory_target.path().join("original/child"), b"protected").unwrap();
    success(run(
        &source,
        directory_target.path(),
        &["--exclude=original", "--delete", "--force-delete"],
    ));
    assert!(!run(&source, directory_target.path(), &[]).status.success());
    assert_eq!(
        std::fs::read(directory_target.path().join("original/child")).unwrap(),
        b"protected"
    );
    let output = success(run(&source, &destination, &["--verify=only", "--json"]));
    assert_eq!(event(&output, "verification_result")["files_matched"], 1);
    std::fs::write(&destination, b"mismatch").unwrap();
    let output = run(&source, &destination, &["--verify=only", "--json"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        event(&output, "verification_result")["files_mismatched"][0],
        source.to_str().unwrap()
    );
}

#[test]
fn missing_selected_parent_is_created_only_for_authorized_work() {
    let root = tempdir().unwrap();
    let source = root.path().join("original");
    let destination = root.path().join("missing/parents/renamed");
    std::fs::write(&source, b"selected bytes").unwrap();
    let output = success(run(&source, &destination, &["--dry-run", "--json"]));
    assert_eq!(event(&output, "summary")["files_created"], 1);
    assert!(!root.path().join("missing").exists());
    for flags in [
        vec!["--exclude=original"],
        vec!["--existing"],
        vec!["--max-size=3"],
        vec![
            "--existing",
            "--preserve-hardlinks",
            "--remove-source-files",
        ],
    ] {
        success(run(&source, &destination, &flags));
        assert!(!root.path().join("missing").exists(), "{flags:?}");
    }
    // Prospective parent creation must not turn an initially absent spelling
    // into an effect at the selected source's own name.
    let alias = root.path().join("missing/../original");
    assert!(!run(&source, &alias, &[]).status.success());
    assert!(!root.path().join("missing").exists());
    assert_eq!(std::fs::read(&source).unwrap(), b"selected bytes");
    success(run(&source, &destination, &[]));
    assert_eq!(std::fs::read(&destination).unwrap(), b"selected bytes");
}

#[test]
fn directory_operand_binds_basename_and_dry_run_accounts_without_writes() {
    let source_root = tempdir().unwrap();
    let destination_root = tempdir().unwrap();
    let source = source_root.path().join("selected");
    std::fs::write(&source, b"selected bytes").unwrap();
    let output = success(run(
        &source,
        destination_root.path(),
        &["--dry-run", "--json"],
    ));
    assert_eq!(event(&output, "summary")["files_created"], 1);
    assert!(!destination_root.path().join("selected").exists());
    success(run(
        &source,
        destination_root.path(),
        &["--verify=after", "--remove-source-files"],
    ));
    assert!(!source.exists());
    assert_eq!(
        std::fs::read(destination_root.path().join("selected")).unwrap(),
        b"selected bytes"
    );
}

#[cfg(unix)]
#[test]
fn renamed_metadata_backup_and_verified_existing_source_removal() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let root = tempdir().unwrap();
    let source = root.path().join("original");
    let destination = root.path().join("renamed");
    std::fs::write(&source, b"same").unwrap();
    std::fs::write(&destination, b"same").unwrap();
    std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(0o644)).unwrap();
    let time = filetime::FileTime::from_unix_time(100, 123);
    filetime::set_file_mtime(&source, time).unwrap();
    filetime::set_file_mtime(&destination, time).unwrap();
    success(run(
        &source,
        &destination,
        &["--preserve-permissions", "--preserve-times"],
    ));
    assert_eq!(
        std::fs::metadata(&destination).unwrap().mode() & 0o777,
        0o600
    );
    assert_eq!(std::fs::metadata(&source).unwrap().mode() & 0o777, 0o600);
    std::fs::write(&source, b"new content").unwrap();
    xattr::set(&source, "user.sy-selected", b"source attribute").unwrap();
    success(run(
        &source,
        &destination,
        &[
            "--backup",
            "--suffix=.bak",
            "--verify=after",
            "--preserve-xattrs",
        ],
    ));
    assert_eq!(
        xattr::get(&destination, "user.sy-selected").unwrap(),
        Some(b"source attribute".to_vec())
    );
    assert_eq!(
        std::fs::read(root.path().join("renamed.bak")).unwrap(),
        b"same"
    );
    assert!(!root.path().join("original.bak").exists());
    success(run(
        &source,
        &destination,
        &["--checksum", "--remove-source-files"],
    ));
    assert!(!source.exists());
    assert_eq!(std::fs::read(&destination).unwrap(), b"new content");
}

#[cfg(unix)]
#[test]
fn backup_effect_aliasing_selected_source_is_refused_before_mutation() {
    let root = tempdir().unwrap();
    let source = root.path().join("renamed.bak");
    let destination = root.path().join("renamed");
    std::fs::write(&source, b"source content").unwrap();
    std::fs::write(&destination, b"old destination").unwrap();
    let output = run(
        &source,
        &destination,
        &["--backup", "--suffix=.bak", "--ignore-times"],
    );
    assert!(!output.status.success());
    assert_eq!(std::fs::read(&source).unwrap(), b"source content");
    assert_eq!(std::fs::read(&destination).unwrap(), b"old destination");
    let alias = root.path().join("alias");
    std::fs::hard_link(&source, &alias).unwrap();
    let output = run(&source, &alias, &["--ignore-times"]);
    assert!(!output.status.success());
    assert_eq!(std::fs::read(&source).unwrap(), b"source content");
}

#[cfg(unix)]
#[test]
fn top_level_links_preserve_skip_follow_file_and_follow_directory() {
    let root = tempdir().unwrap();
    let destination_root = tempdir().unwrap();
    let source = root.path().join("link");
    std::os::unix::fs::symlink("missing", &source).unwrap();
    let destination = destination_root.path().join("renamed");
    success(run(&source, &destination, &[]));
    assert_eq!(
        std::fs::read_link(&destination).unwrap(),
        Path::new("missing")
    );
    std::fs::remove_file(&destination).unwrap();
    success(run(
        &source,
        &destination,
        &["--links=skip", "--delete", "--force-delete"],
    ));
    assert!(!destination.exists());
    std::fs::write(root.path().join("missing"), b"followed bytes").unwrap();
    success(run(
        &source,
        &destination,
        &["--copy-links", "--checksum", "--verify=after"],
    ));
    assert_eq!(std::fs::read(&destination).unwrap(), b"followed bytes");
    success(run(
        &source,
        &destination,
        &["--copy-links", "--verify=only"],
    ));
    std::fs::remove_file(&source).unwrap();
    let tree = root.path().join("tree");
    std::fs::create_dir(&tree).unwrap();
    std::fs::write(tree.join("child"), b"tree bytes").unwrap();
    std::os::unix::fs::symlink(&tree, &source).unwrap();
    success(run(&source, destination_root.path(), &["--copy-links"]));
    assert_eq!(
        std::fs::read(destination_root.path().join("link/child")).unwrap(),
        b"tree bytes"
    );
}

#[cfg(unix)]
#[test]
fn native_names_and_source_ignore_rules_survive_selected_binding() {
    use std::os::unix::ffi::OsStringExt;
    fn native_name(prefix: &[u8], byte: u8) -> Vec<u8> {
        let mut name = prefix.to_vec();
        // Linux filesystems accept non-UTF-8 native bytes; APFS refuses them
        // with EILSEQ, so exercise its supported Unicode native names instead.
        if cfg!(target_os = "linux") {
            name.push(byte);
        } else {
            name.extend_from_slice("雪".as_bytes());
        }
        name
    }
    let root = tempdir().unwrap();
    let destination_root = tempdir().unwrap();
    let source = root
        .path()
        .join(std::ffi::OsString::from_vec(native_name(b"source", 0xff)));
    let destination = destination_root
        .path()
        .join(std::ffi::OsString::from_vec(native_name(b"dest", 0xfe)));
    std::fs::write(&source, b"native").unwrap();
    success(run(&source, &destination, &["--verify=after"]));
    assert_eq!(std::fs::read(&destination).unwrap(), b"native");
    let ignored = root.path().join("ignored");
    std::fs::create_dir(root.path().join(".git")).unwrap();
    std::fs::write(root.path().join(".gitignore"), "ignored\n").unwrap();
    std::fs::write(&ignored, b"excluded").unwrap();
    success(run(
        &ignored,
        &destination,
        &["--gitignore", "--delete", "--force-delete"],
    ));
    assert_eq!(std::fs::read(&destination).unwrap(), b"native");
}
