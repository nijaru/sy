use super::*;
use futures::TryStreamExt;
use ignore::WalkBuilder;

fn original(root: &Path, request: ScanRequest) -> Result<Vec<Entry>, BoxError> {
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(false)
        .git_ignore(request.respect_gitignore)
        .git_global(request.respect_gitignore)
        .git_exclude(request.respect_gitignore)
        .follow_links(request.follow_symlinks)
        .sort_by_file_path(|left, right| left.cmp(right));
    if !request.include_git_dir {
        builder.filter_entry(|entry| entry.file_name() != ".git");
    }
    if request.respect_gitignore && root.join(".gitignore").exists() {
        builder.add_ignore(root.join(".gitignore"));
    }
    builder.max_depth(request.max_depth);
    let mut entries = Vec::new();
    for entry in builder.build() {
        let entry = entry?;
        if entry.path() != root {
            let metadata = entry_metadata(entry.path(), request)?;
            entries.push(engine_entry(root, entry.path(), &metadata, request)?);
        }
    }
    Ok(entries)
}

async fn parity(root: &Path, request: ScanRequest) -> Vec<Entry> {
    let expected = original(root, request).unwrap();
    let actual: Vec<Entry> = local_entry_stream(root.to_path_buf(), request)
        .try_collect()
        .await
        .unwrap();
    assert_eq!(
        actual,
        expected,
        "root={} request={request:?}",
        root.display()
    );
    actual
}

fn file(root: &Path, path: &str, bytes: &[u8]) {
    let path = root.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

#[tokio::test]
async fn category_negation_repo_boundaries_and_depth_match_original() {
    for repository in [false, true] {
        let tree = tempfile::TempDir::new().unwrap();
        let root = tree.path().join("source");
        std::fs::create_dir(&root).unwrap();
        if repository {
            std::fs::create_dir(root.join(".git")).unwrap();
        }
        file(&root, ".ignore", b"category\n!keep.log\n");
        file(&root, ".gitignore", b"*.log\nsub/repo/outer-only\n");
        file(&root, "sub/.gitignore", b"!category\n!*.log\n");
        file(&root, "sub/.ignore", b"drop\n");
        file(&root, "sub/repo/.gitignore", b"nested-only\n");
        std::fs::create_dir(root.join("sub/repo/.git")).unwrap();
        for name in [
            "category",
            "keep.log",
            "drop.log",
            "drop",
            "outer-only",
            "nested-only",
            "keep",
        ] {
            file(&root, name, b"x");
            file(&root, &format!("sub/{name}"), b"x");
            file(&root, &format!("sub/repo/{name}"), b"x");
        }
        for respect_gitignore in [false, true] {
            for include_git_dir in [false, true] {
                for max_depth in [None, Some(0), Some(1), Some(2)] {
                    let request = ScanRequest {
                        respect_gitignore,
                        include_git_dir,
                        max_depth,
                        ..Default::default()
                    };
                    parity(&root, request).await;
                }
            }
        }
        let entries = parity(
            &root,
            ScanRequest {
                respect_gitignore: true,
                ..Default::default()
            },
        )
        .await;
        assert!(!entries
            .iter()
            .any(|entry| entry.path.as_path() == Path::new("sub/category")));
        assert!(entries
            .iter()
            .any(|entry| entry.path.as_path() == Path::new("sub/repo/outer-only")));
    }
}

#[cfg(unix)]
#[tokio::test]
async fn canonical_root_parents_and_external_copy_links_keep_lexical_ancestry() {
    let canonical = tempfile::TempDir::new().unwrap();
    let aliases = tempfile::TempDir::new().unwrap();
    let external = tempfile::TempDir::new().unwrap();
    let root = canonical.path().join("source");
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir(canonical.path().join(".git")).unwrap();
    file(
        canonical.path(),
        ".ignore",
        b"source/parent-drop\nsource/outside/canonical-parent-drop\n",
    );
    file(canonical.path(), ".gitignore", b"source/parent-git-drop\n");
    file(aliases.path(), ".ignore", b"lexical-parent-only\n");
    file(
        &root,
        ".ignore",
        b"/outside/root-drop\n/outside/nested/root-drop\n",
    );
    file(&root, "parent-drop", b"x");
    file(&root, "parent-git-drop", b"x");
    file(&root, "lexical-parent-only", b"x");
    file(external.path(), ".gitignore", b"!root-drop\n");
    file(external.path(), ".ignore", b"target-drop\n");
    for name in [
        "root-drop",
        "canonical-parent-drop",
        "target-drop",
        "keep",
        "nested/root-drop",
        "nested/keep",
    ] {
        file(external.path(), name, b"x");
    }
    // A nested repository changes only git ancestry, never .ignore ancestry.
    std::fs::create_dir(external.path().join("nested/.jj")).unwrap();
    std::os::unix::fs::symlink(external.path(), root.join("outside")).unwrap();
    // Repeated aliases are not ancestor cycles and must both be traversed.
    std::os::unix::fs::symlink(external.path(), root.join("other-alias")).unwrap();
    let alias = aliases.path().join("alias");
    std::os::unix::fs::symlink(&root, &alias).unwrap();
    for source in [&root, &alias] {
        for follow_symlinks in [false, true] {
            let entries = parity(
                source,
                ScanRequest {
                    respect_gitignore: true,
                    follow_symlinks,
                    ..Default::default()
                },
            )
            .await;
            assert!(!entries
                .iter()
                .any(|entry| entry.path.as_path() == Path::new("parent-drop")));
            assert!(!entries
                .iter()
                .any(|entry| entry.path.as_path() == Path::new("parent-git-drop")));
            assert!(entries
                .iter()
                .any(|entry| entry.path.as_path() == Path::new("lexical-parent-only")));
            if follow_symlinks {
                assert!(!entries
                    .iter()
                    .any(|entry| entry.path.as_path() == Path::new("outside/root-drop")));
                assert!(entries
                    .iter()
                    .any(|entry| entry.path.as_path() == Path::new("outside/keep")));
            }
        }
    }
}

#[tokio::test]
async fn partial_rules_and_git_indirection_match_original() {
    let root = tempfile::TempDir::new().unwrap();
    let git = tempfile::TempDir::new().unwrap();
    file(
        root.path(),
        ".git",
        format!("gitdir: {}\nignored second line\n", git.path().display()).as_bytes(),
    );
    file(git.path(), "commondir", b".\n");
    file(git.path(), "info/exclude", b"exclude-drop\n");
    file(root.path(), ".ignore", b"before\n{bad\nafter\n");
    for name in ["before", "after", "exclude-drop", "keep"] {
        file(root.path(), name, b"x");
    }
    let entries = parity(
        root.path(),
        ScanRequest {
            respect_gitignore: true,
            ..Default::default()
        },
    )
    .await;
    for name in ["before", "after", "exclude-drop"] {
        assert!(!entries
            .iter()
            .any(|entry| entry.path.as_path() == Path::new(name)));
    }
    assert!(entries
        .iter()
        .any(|entry| entry.path.as_path() == Path::new("keep")));
}

#[cfg(unix)]
#[tokio::test]
async fn dangling_links_and_cycles_are_scan_errors_not_silent_pruning() {
    for cycle in [false, true] {
        let root = tempfile::TempDir::new().unwrap();
        file(root.path(), "a", b"x");
        std::os::unix::fs::symlink(
            if cycle { "." } else { "missing" },
            root.path().join("z-link"),
        )
        .unwrap();
        parity(root.path(), ScanRequest::default()).await;
        for max_depth in [None, Some(1)] {
            let request = ScanRequest {
                follow_symlinks: true,
                max_depth,
                ..Default::default()
            };
            let expected = original(root.path(), request);
            let actual = local_entry_stream(root.path().to_path_buf(), request)
                .try_collect::<Vec<_>>()
                .await;
            assert_eq!(
                actual.is_err(),
                expected.is_err(),
                "cycle={cycle} max_depth={max_depth:?}"
            );
        }
        file(root.path(), ".ignore", b"z-link\n");
        let request = ScanRequest {
            follow_symlinks: true,
            ..Default::default()
        };
        let expected = original(root.path(), request);
        let actual = local_entry_stream(root.path().to_path_buf(), request)
            .try_collect::<Vec<_>>()
            .await;
        assert_eq!(actual.is_err(), expected.is_err(), "ignored cycle={cycle}");
    }
}

#[tokio::test]
async fn global_excludes_precede_explicit_and_are_cwd_anchored() {
    if std::env::var_os("SY_TEST_IGNORE_HOME").is_some() {
        let root = tempfile::TempDir::new().unwrap();
        file(root.path(), ".gitignore", b"!global-drop\n/root-only\n");
        std::fs::create_dir_all(root.path().join("sub/.git")).unwrap();
        file(root.path(), "sub/global-drop", b"x");
        file(root.path(), "sub/root-only", b"x");
        let request = ScanRequest {
            respect_gitignore: true,
            ..Default::default()
        };
        let entries = parity(root.path(), request).await;
        assert!(!entries
            .iter()
            .any(|entry| entry.path.as_path() == Path::new("sub/global-drop")));
        assert!(entries
            .iter()
            .any(|entry| entry.path.as_path() == Path::new("sub/root-only")));
        file(root.path(), ".ignore", b"!global-drop\n");
        let entries = parity(root.path(), request).await;
        assert!(entries
            .iter()
            .any(|entry| entry.path.as_path() == Path::new("sub/global-drop")));
        return;
    }
    let home = tempfile::TempDir::new().unwrap();
    file(home.path(), "excludes", b"global-drop\n/root-only\n");
    file(
        home.path(),
        ".gitconfig",
        format!(
            "[core]\nexcludesFile = {}\n",
            home.path().join("excludes").display()
        )
        .as_bytes(),
    );
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "endpoint::local_entry_scan::differential::global_excludes_precede_explicit_and_are_cwd_anchored"])
        .env("HOME", home.path()).env("SY_TEST_IGNORE_HOME", home.path()).output().unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn non_utf8_names_match_original_without_loss() {
    use std::os::unix::ffi::OsStringExt;
    let root = tempfile::TempDir::new().unwrap();
    for bytes in [
        vec![0xff],
        vec![b'a', 0xfe],
        b"a.ext".to_vec(),
        b"a".to_vec(),
    ] {
        std::fs::write(root.path().join(std::ffi::OsString::from_vec(bytes)), b"x").unwrap();
    }
    parity(root.path(), ScanRequest::default()).await;
}
