//! Source-derived ignore evaluation for deletion scope.
//!
//! The destination scan stays complete; whether a destination-only entry
//! may be deleted is decided by asking whether the SOURCE tree's ignore
//! rules would exclude that path. This module answers that question for
//! arbitrary destination paths — including paths the source walk never
//! emitted — with parity to the source walk's own ignore decisions, biased
//! toward protection: where evaluation and the walk could disagree, this
//! module over-ignores (protects) rather than under-ignores (deletes).
//!
//! Chaining mirrors the `ignore` crate's internal semantics (source-verified
//! contract in `ai/research/gitignore-delete-scope.md`): per-directory
//! `.ignore` and `.gitignore` files consulted deepest-first with the first
//! matching file deciding, the last matching pattern winning within one
//! file, `.ignore` ahead of `.gitignore` at the same level, then
//! `.git/info/exclude`, then global excludes, with git rules gated on
//! repository presence (`require_git` default). An ignored directory
//! prunes all descendants: negations in deeper files cannot re-include
//! them, matching `gitignore(5)`.
//!
//! The crate's own chaining (`ignore::dir::Ignore`) is private, so the
//! per-file `Gitignore` matchers are chained here by hand. One deliberate
//! divergence: the walker anchors the explicit non-repository root
//! fallback and global excludes to the process working directory; this
//! scope anchors them to the source root, which is the semantic a sync
//! tool wants regardless of invocation directory.

use crate::engine::domain::Entry;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const GITIGNORE_FILE: &str = ".gitignore";
const DOT_IGNORE_FILE: &str = ".ignore";
const EXCLUDE_PATH: &str = "info/exclude";

/// Compiled rule files for one directory, loaded on first visit.
#[derive(Clone, Default)]
struct DirectoryRules {
    dot_ignore: Option<Gitignore>,
    gitignore: Option<Gitignore>,
}

/// Evaluator answering "would the source tree's ignore rules exclude this
/// path?" for destination-side deletion scope.
///
/// Matchers are compiled lazily per directory and cached; rule files are
/// rare and read once. I/O and parse failures degrade to an empty matcher
/// with a warning, matching the walker's policy of ignoring unreadable
/// ignore files rather than failing the sync.
pub struct SourceIgnoreScope {
    root: PathBuf,
    /// When false, only the always-active `.ignore` chain is consulted,
    /// mirroring the walker's unconditional `.ignore` support.
    respect_gitignore: bool,
    /// Repo presence, probed once: `.git` (directory or worktree pointer
    /// file) or `.jj` at the source root, mirroring the walker's
    /// `require_git` discovery.
    has_git: Option<bool>,
    git_dir: Option<PathBuf>,
    /// Explicit root `.gitignore` fallback, applied only for non-repository
    /// trees (the walker's `add_ignore` of the root file).
    explicit_root: Option<Gitignore>,
    /// Cached per-directory rule files, keyed by directory path.
    dir_rules: HashMap<PathBuf, DirectoryRules>,
    exclude_matcher: Option<Option<Gitignore>>,
    global_matcher: Option<Gitignore>,
}

impl SourceIgnoreScope {
    pub fn new(root: &Path, respect_gitignore: bool) -> Self {
        Self {
            root: root.to_path_buf(),
            respect_gitignore,
            has_git: None,
            git_dir: None,
            explicit_root: None,
            dir_rules: HashMap::new(),
            exclude_matcher: None,
            global_matcher: None,
        }
    }

    /// Would the source's ignore rules exclude this path?
    ///
    /// `path` is relative to the source root; `is_dir` reflects the entry
    /// being evaluated. An ignored ancestor directory prunes the path
    /// regardless of deeper negations.
    pub fn is_ignored(&mut self, path: &Path, is_dir: bool) -> bool {
        let absolute = self.root.join(path);

        // Pruning: any ignored directory between the root and the path's
        // parent makes the whole subtree ignored. Ancestors are checked
        // shallowest-first so each check itself sees only outer context.
        for ancestor in ancestors_between(&self.root, &absolute) {
            if self.chain_decision(&ancestor, true) == Some(true) {
                return true;
            }
        }

        self.chain_decision(&absolute, is_dir).unwrap_or(false)
            || self.fallback_decision(&absolute, is_dir).unwrap_or(false)
    }

    /// Whether a destination-only entry is out of deletion scope.
    pub fn protects(&mut self, entry: &Entry) -> bool {
        self.is_ignored(entry.path.as_path(), entry.is_directory())
    }

    /// Consult per-directory rule files deepest-first. The first file
    /// yielding a match decides; `.ignore` precedes `.gitignore` at the
    /// same level, mirroring the walker's per-directory matcher order.
    fn chain_decision(&mut self, absolute: &Path, is_dir: bool) -> Option<bool> {
        for directory in ancestor_chain(&self.root, absolute) {
            let rules = self.rules_for(&directory);
            let mut files = [rules.dot_ignore.as_ref(), rules.gitignore.as_ref()];
            if !self.respect_gitignore {
                files[1] = None;
            }
            for matcher in files.into_iter().flatten() {
                if let Some(decision) = self.match_one(matcher, absolute, is_dir) {
                    return Some(decision);
                }
            }
        }
        None
    }

    /// Repository-gated fallbacks: `.git/info/exclude`, then global
    /// excludes. For non-repository trees, the explicit root `.gitignore`
    /// takes their place.
    fn fallback_decision(&mut self, absolute: &Path, is_dir: bool) -> Option<bool> {
        if !self.respect_gitignore {
            return None;
        }
        if self.git_rules_active() {
            if let Some(exclude) = self.exclude_matcher() {
                if let Some(decision) = self.match_one(&exclude, absolute, is_dir) {
                    return Some(decision);
                }
            }
            if let Some(global) = self.global_matcher() {
                if let Some(decision) = self.match_one(&global, absolute, is_dir) {
                    return Some(decision);
                }
            }
            None
        } else {
            let explicit = self.explicit_root_matcher()?;
            self.match_one(&explicit, absolute, is_dir)
        }
    }

    fn match_one(&self, matcher: &Gitignore, absolute: &Path, is_dir: bool) -> Option<bool> {
        match matcher.matched(absolute, is_dir) {
            ignore::Match::None => None,
            ignore::Match::Ignore(_) => Some(true),
            ignore::Match::Whitelist(_) => Some(false),
        }
    }

    fn rules_for(&mut self, directory: &Path) -> DirectoryRules {
        if let Some(rules) = self.dir_rules.get(directory) {
            return rules.clone();
        }
        let rules = DirectoryRules {
            dot_ignore: compile_matcher(directory, DOT_IGNORE_FILE),
            gitignore: compile_matcher(directory, GITIGNORE_FILE),
        };
        self.dir_rules
            .insert(directory.to_path_buf(), rules.clone());
        rules
    }

    fn git_rules_active(&mut self) -> bool {
        if self.has_git.is_none() {
            let dot_git = self.root.join(".git");
            let has_git = dot_git.exists() || self.root.join(".jj").exists();
            if has_git {
                self.git_dir = Some(resolve_git_dir(&dot_git));
            }
            self.has_git = Some(has_git);
        }
        self.has_git.unwrap_or(false)
    }

    fn exclude_matcher(&mut self) -> Option<Gitignore> {
        if self.exclude_matcher.is_none() {
            self.exclude_matcher = Some(self.git_dir.as_ref().and_then(|git_dir| {
                let exclude = git_dir.join(EXCLUDE_PATH);
                if exclude.exists() {
                    Some(compile_matcher_rooted(&self.root, &exclude))
                } else {
                    None
                }
            }));
        }
        self.exclude_matcher.clone().flatten()
    }

    fn global_matcher(&mut self) -> Option<Gitignore> {
        if self.global_matcher.is_none() {
            let (matcher, error) = GitignoreBuilder::new(&self.root).build_global();
            if let Some(error) = error {
                tracing::warn!("source ignore scope: global excludes: {}", error);
            }
            self.global_matcher = Some(matcher);
        }
        self.global_matcher.clone()
    }

    fn explicit_root_matcher(&mut self) -> Option<Gitignore> {
        if self.explicit_root.is_none() {
            let root_gitignore = self.root.join(GITIGNORE_FILE);
            if root_gitignore.exists() {
                self.explicit_root = Some(compile_matcher_rooted(&self.root, &root_gitignore));
            }
        }
        self.explicit_root.clone()
    }
}

/// Directories from `path.parent()` down to and including `root`, or the
/// file's own directory chain when `path` sits directly in `root`.
fn compile_matcher(directory: &Path, file_name: &str) -> Option<Gitignore> {
    let rule_file = directory.join(file_name);
    if !rule_file.exists() {
        return None;
    }
    Some(compile_matcher_rooted(directory, &rule_file))
}

/// Compile one rule file anchored at `root`. Failures degrade to an empty
/// matcher; the walker likewise continues past unreadable ignore files.
fn compile_matcher_rooted(root: &Path, rule_file: &Path) -> Gitignore {
    let mut builder = GitignoreBuilder::new(root);
    if let Some(error) = builder.add(rule_file) {
        tracing::warn!(
            "source ignore scope: failed to read {}: {}",
            rule_file.display(),
            error
        );
        return Gitignore::empty();
    }
    match builder.build() {
        Ok(matcher) => matcher,
        Err(error) => {
            tracing::warn!(
                "source ignore scope: failed to compile {}: {}",
                rule_file.display(),
                error
            );
            Gitignore::empty()
        }
    }
}

/// Directories governing `path` (its parent chain), ordered deepest-first
/// and ending at `root` itself.
fn ancestor_chain(root: &Path, path: &Path) -> Vec<PathBuf> {
    let mut chain = Vec::new();
    let mut current = path.parent();
    while let Some(directory) = current {
        if directory == root || directory == Path::new("") {
            break;
        }
        chain.push(directory.to_path_buf());
        current = directory.parent();
    }
    chain.push(root.to_path_buf());
    chain
}

/// Ancestor directories between `root` and `path`, shallowest-first, for
/// pruning checks. Excludes `root` (its own ignore status is not a
/// deletion-scope question) and `path` itself.
fn ancestors_between(root: &Path, path: &Path) -> Vec<PathBuf> {
    let mut ancestors = ancestor_chain(root, path);
    ancestors.pop(); // drop root
    ancestors.reverse(); // shallowest-first
    ancestors
}

/// Resolve the repository directory, following the `gitdir:` indirection
/// where `.git` is a file (linked worktrees, submodules), mirroring the
/// walker's `resolve_git_commondir`.
fn resolve_git_dir(dot_git: &Path) -> PathBuf {
    let Ok(metadata) = std::fs::metadata(dot_git) else {
        return dot_git.to_path_buf();
    };
    if !metadata.is_file() {
        return dot_git.to_path_buf();
    }
    let Ok(contents) = std::fs::read_to_string(dot_git) else {
        return dot_git.to_path_buf();
    };
    let Some(gitdir) = contents.strip_prefix("gitdir: ") else {
        return dot_git.to_path_buf();
    };
    let gitdir = PathBuf::from(gitdir.trim());
    let commondir = gitdir.join("commondir");
    if let Ok(common) = std::fs::read_to_string(&commondir) {
        let common = common.trim();
        if !common.is_empty() {
            let candidate = if common.starts_with('.') {
                gitdir.join(common)
            } else {
                PathBuf::from(common)
            };
            if let Ok(resolved) = candidate.canonicalize() {
                return resolved;
            }
        }
    }
    gitdir
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn repo_root() -> tempfile::TempDir {
        let root = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(root.path().join(".git")).unwrap();
        root
    }

    fn scope(root: &Path) -> SourceIgnoreScope {
        SourceIgnoreScope::new(root, true)
    }

    #[test]
    fn root_level_pattern_ignores_nested_paths() {
        let root = repo_root();
        std::fs::write(root.path().join(".gitignore"), b"*.log\nbuild/\n").unwrap();

        let mut scope = scope(root.path());
        assert!(scope.is_ignored(Path::new("a.log"), false));
        assert!(scope.is_ignored(Path::new("nested/b.log"), false));
        assert!(scope.is_ignored(Path::new("build"), true));
        assert!(scope.is_ignored(Path::new("build/inner"), false));
        assert!(!scope.is_ignored(Path::new("keep.txt"), false));
        assert!(!scope.is_ignored(Path::new("buildx"), false));
    }

    #[test]
    fn non_repository_root_gitignore_applies_as_explicit_fallback() {
        let root = tempfile::TempDir::new().unwrap();
        std::fs::write(root.path().join(".gitignore"), b"*.log\n").unwrap();

        let mut scope = scope(root.path());
        assert!(scope.is_ignored(Path::new("x.log"), false));
        assert!(!scope.is_ignored(Path::new("x.txt"), false));
    }

    #[test]
    fn deeper_gitignore_overrides_shallower() {
        let root = repo_root();
        std::fs::write(root.path().join(".gitignore"), b"*.log\n").unwrap();
        std::fs::create_dir(root.path().join("nested")).unwrap();
        std::fs::write(root.path().join("nested/.gitignore"), b"!*.log\n").unwrap();

        let mut scope = scope(root.path());
        assert!(!scope.is_ignored(Path::new("nested/a.log"), false));
        assert!(scope.is_ignored(Path::new("a.log"), false));
    }

    #[test]
    fn deeper_file_decides_without_fallthrough() {
        let root = repo_root();
        std::fs::write(root.path().join(".gitignore"), b"*.log\n").unwrap();
        std::fs::create_dir(root.path().join("sub")).unwrap();
        std::fs::write(root.path().join("sub/.gitignore"), b"!a.log\n").unwrap();

        let mut scope = scope(root.path());
        assert!(!scope.is_ignored(Path::new("sub/a.log"), false));
        // The deeper file decided for a.log; its decision (whitelist) blocks
        // the shallower *.log rule. Other logs still ignored by the root file.
        assert!(scope.is_ignored(Path::new("sub/b.log"), false));
    }

    #[test]
    fn ignored_directory_prunes_descendants() {
        let root = repo_root();
        std::fs::write(root.path().join(".gitignore"), b"build/\n").unwrap();
        std::fs::create_dir_all(root.path().join("build/sub")).unwrap();
        std::fs::write(root.path().join("build/sub/.gitignore"), b"!keep\n").unwrap();

        let mut scope = scope(root.path());
        assert!(scope.is_ignored(Path::new("build"), true));
        // Negations in deeper files under an ignored directory have no
        // effect, matching gitignore(5)'s pruning rule.
        assert!(scope.is_ignored(Path::new("build/sub/keep"), false));
    }

    #[test]
    fn trailing_slash_only_matches_directories() {
        let root = repo_root();
        std::fs::write(root.path().join(".gitignore"), b"build/\n").unwrap();

        let mut scope = scope(root.path());
        assert!(!scope.is_ignored(Path::new("build"), false));
        assert!(scope.is_ignored(Path::new("build"), true));
    }

    #[test]
    fn info_exclude_is_honored() {
        let root = repo_root();
        std::fs::create_dir_all(root.path().join(".git/info")).unwrap();
        std::fs::write(root.path().join(".git/info/exclude"), b"local-only\n").unwrap();

        let mut scope = scope(root.path());
        assert!(scope.is_ignored(Path::new("local-only"), false));
        assert!(!scope.is_ignored(Path::new("other"), false));
    }

    #[test]
    fn dot_ignore_honored_without_gitignore_flag() {
        let root = repo_root();
        std::fs::write(root.path().join(".ignore"), b"secret\n").unwrap();

        let mut scope = SourceIgnoreScope::new(root.path(), false);
        assert!(scope.is_ignored(Path::new("secret"), false));
        assert!(scope.is_ignored(Path::new("dir/secret"), false));
        // With the gitignore flag off, .gitignore files are not consulted.
        std::fs::write(root.path().join(".gitignore"), b"other\n").unwrap();
        assert!(!scope.is_ignored(Path::new("other"), false));
    }

    #[test]
    fn dot_ignore_takes_precedence_within_directory() {
        let root = repo_root();
        std::fs::write(root.path().join(".ignore"), b"!keep.log\n").unwrap();
        std::fs::write(root.path().join(".gitignore"), b"*.log\n").unwrap();

        let mut scope = scope(root.path());
        // .ignore's whitelist decides before .gitignore's ignore at the same
        // level, mirroring the walker's matcher order.
        assert!(!scope.is_ignored(Path::new("keep.log"), false));
        assert!(scope.is_ignored(Path::new("drop.log"), false));
    }

    #[test]
    fn symlinked_or_file_git_pointer_resolves_exclude() {
        let root = tempfile::TempDir::new().unwrap();
        let real_git = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(real_git.path().join("info")).unwrap();
        std::fs::write(real_git.path().join("info/exclude"), b"worktree-only\n").unwrap();
        std::fs::write(
            root.path().join(".git"),
            format!("gitdir: {}", real_git.path().display()),
        )
        .unwrap();

        let mut scope = scope(root.path());
        assert!(scope.is_ignored(Path::new("worktree-only"), false));
        assert!(!scope.is_ignored(Path::new("anything-else"), false));
    }

    #[test]
    fn anchored_pattern_only_matches_at_root_level() {
        let root = repo_root();
        std::fs::write(root.path().join(".gitignore"), b"/top.txt\n").unwrap();

        let mut scope = scope(root.path());
        assert!(scope.is_ignored(Path::new("top.txt"), false));
        assert!(!scope.is_ignored(Path::new("sub/top.txt"), false));
    }

    proptest! {
        /// Walker/scope parity: every path the walker emits after ignore
        /// filtering must evaluate not-ignored. This is the invariant that
        /// keeps deletion scope from deleting a file the source walk would
        /// have shielded.
        #[test]
        fn proptest_matches_walk_selection(
            root_rules in proptest::collection::vec("[!\\n\\r]{1,8}", 0..4),
            nested_rules in proptest::collection::vec("[!\\n\\r]{1,8}", 0..4),
            names in proptest::collection::vec("[a-c]{1,3}", 2..6),
        ) {
            let root = repo_root();
            std::fs::write(
                root.path().join(".gitignore"),
                format!("{}\n", root_rules.join("\n")),
            )
            .unwrap();
            let nested = root.path().join("sub");
            std::fs::create_dir_all(&nested).unwrap();
            std::fs::write(
                nested.join(".gitignore"),
                format!("{}\n", nested_rules.join("\n")),
            )
            .unwrap();
            for name in &names {
                std::fs::write(nested.join(name), b"x").unwrap();
                std::fs::write(root.path().join(name), b"x").unwrap();
            }

            // Walker with sy's exact scan_worker configuration, except
            // global excludes: a machine-dependent global file would make
            // parity machine-specific. The scope mirrors this by construction
            // only when the test environment has no global excludes; the
            // generated names ([a-c]) avoid realistic collision risk.
            let mut builder = ignore::WalkBuilder::new(root.path());
            builder
                .hidden(false)
                .git_ignore(true)
                .git_global(true)
                .git_exclude(true)
                .follow_links(false);
            builder.filter_entry(|entry| entry.file_name() != ".git");
            let mut walker_paths = std::collections::HashSet::new();
            for entry in builder.build() {
                let entry = entry.unwrap();
                if entry.path() == root.path() {
                    continue;
                }
                walker_paths.insert(
                    entry
                        .path()
                        .strip_prefix(root.path())
                        .unwrap()
                        .to_path_buf(),
                );
            }

            let mut scope = scope(root.path());
            for path in &walker_paths {
                let is_dir = root.path().join(path).is_dir();
                prop_assert!(
                    !scope.is_ignored(path, is_dir),
                    "walker emitted {} but scope ignores it",
                    path.display()
                );
            }
        }
    }
}
