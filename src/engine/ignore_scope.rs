//! One owner for source-walk ignore semantics and conservative deletion scope.
//!
//! Source matching follows ignore 0.4's category precedence, not a per-directory
//! interleaving: all `.ignore`, then `.gitignore`, exclude, global, explicit.
//! Only the original root's external parents are canonical; internal ancestry
//! stays lexical even when a followed symlink points outside the source tree.
use crate::engine::domain::Entry;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use std::collections::VecDeque;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

const DIRECTORY_RULE_CACHE_CAPACITY: usize = 64;
// These are configuration limits, not tree-size/depth limits. Both byte count
// and pattern count matter: millions of short globs also consume compiler RAM.
const MAX_CONFIG_BYTES: usize = 256 * 1024;
const MAX_RULES: usize = 4096;

struct RuleFile {
    matcher: Gitignore,
    malformed: bool,
}

type RuleCache = VecDeque<(PathBuf, PathBuf, RuleFile)>;

pub struct SourceIgnoreScope {
    root: PathBuf,
    authority: Option<crate::rooted_fs::RootedFs>,
    respect_gitignore: bool,
    canonical_root: Option<PathBuf>,
    initialized: bool,
    source_initialized: bool,
    cwd: Option<PathBuf>,
    global_file: Option<PathBuf>,
    dir_rules: RuleCache,
}

impl SourceIgnoreScope {
    pub fn new(root: &Path, respect_gitignore: bool) -> Self {
        Self {
            root: root.to_path_buf(),
            authority: None,
            respect_gitignore,
            canonical_root: None,
            initialized: false,
            source_initialized: false,
            cwd: std::env::current_dir().ok(),
            global_file: None,
            dir_rules: VecDeque::new(),
        }
    }

    /// Internal rules use the original held root; lexical matcher anchors and
    /// explicitly external parent/global/gitdir configuration keep their meaning.
    pub fn with_rooted_authority(
        rooted: crate::rooted_fs::RootedFs,
        respect_gitignore: bool,
    ) -> Self {
        let mut scope = Self::new(rooted.root_path(), respect_gitignore);
        scope.authority = Some(rooted);
        scope
    }

    fn internal_relative<'a>(&self, file: &'a Path) -> Option<&'a Path> {
        self.authority.as_ref()?;
        file.strip_prefix(&self.root)
            .ok()
            .or_else(|| {
                self.canonical_root
                    .as_ref()
                    .and_then(|root| file.strip_prefix(root).ok())
            })
            .filter(|relative| !relative.as_os_str().is_empty())
    }

    fn read_config(&self, path: &Path) -> io::Result<Option<Vec<u8>>> {
        if let (Some(rooted), Some(relative)) = (&self.authority, self.internal_relative(path)) {
            let file = match rooted.open_source_configuration_blocking(relative) {
                Ok(file) => file,
                Err(crate::rooted_fs::RootedFsError::Io(error))
                    if matches!(
                        error.kind(),
                        io::ErrorKind::NotFound
                            | io::ErrorKind::NotADirectory
                            | io::ErrorKind::PermissionDenied
                    ) =>
                {
                    return Ok(None)
                }
                Err(error) => return Err(io::Error::other(error)),
            };
            return read_config_file(file, path);
        }
        read_config(path)
    }

    fn is_file(&self, path: &Path) -> io::Result<Option<bool>> {
        if let (Some(rooted), Some(relative)) = (&self.authority, self.internal_relative(path)) {
            return rooted
                .source_configuration_is_file_blocking(relative)
                .map_err(io::Error::other);
        }
        Ok(std::fs::metadata(path)
            .ok()
            .map(|metadata| metadata.is_file()))
    }

    fn has_git(&self, directory: &Path) -> io::Result<bool> {
        Ok(self.is_file(&directory.join(".git"))?.is_some()
            || self.is_file(&directory.join(".jj"))?.is_some())
    }

    fn canonical_config_path(&self, path: &Path) -> io::Result<Option<PathBuf>> {
        if self.internal_relative(path).is_some() {
            return Ok(self.is_file(path)?.is_some().then(|| path.to_path_buf()));
        }
        Ok(path.canonicalize().ok())
    }

    fn initialize(&mut self) -> io::Result<()> {
        if let Some(rooted) = &self.authority {
            rooted
                .verify_root_path_blocking()
                .map_err(io::Error::other)?;
        }
        if !self.initialized {
            self.canonical_root = match &self.authority {
                Some(rooted) => Some(
                    rooted
                        .source_configuration_root_path_blocking()
                        .map_err(io::Error::other)?,
                ),
                None => self.root.canonicalize().ok(),
            };
            if self.respect_gitignore {
                self.global_file = global_excludes()?;
            }
            self.initialized = true;
        }
        Ok(())
    }

    /// Load root/parent policy before enumeration, including empty roots.
    /// Upstream reports malformed external-parent rules as walk errors, but
    /// attaches malformed internal rules to otherwise usable directory entries.
    pub(crate) fn prepare_source(&mut self) -> io::Result<()> {
        self.initialize()?;
        if self.source_initialized {
            return Ok(());
        }
        if let Some(root) = self.canonical_root.clone() {
            for directory in root.ancestors().skip(1) {
                for name in [".ignore", ".gitignore"] {
                    if name == ".gitignore" && !self.respect_gitignore {
                        continue;
                    }
                    let file = directory.join(name);
                    if self.load_file(directory, &file)?.malformed {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("malformed parent ignore configuration: {}", file.display()),
                        ));
                    }
                }
                if self.respect_gitignore {
                    if let Some(git) = resolve_git_dir(directory, self)? {
                        let file = git.join("info/exclude");
                        if self.load_file(directory, &file)?.malformed {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                format!(
                                    "malformed parent ignore configuration: {}",
                                    file.display()
                                ),
                            ));
                        }
                    }
                }
            }
        }
        let root = self.root.clone();
        self.load_file(&root, &root.join(".ignore"))?;
        if self.respect_gitignore {
            self.load_file(&root, &root.join(".gitignore"))?;
            if let Some(git) = resolve_git_dir(&root, self)? {
                self.load_file(&root, &git.join("info/exclude"))?;
            }
        }
        self.source_initialized = true;
        Ok(())
    }

    /// Match one entry exactly as the original WalkBuilder does. The caller
    /// owns directory pruning; rules inside an entry's own directory are not
    /// used to decide whether to visit that directory.
    pub(crate) fn source_match(&mut self, relative: &Path, is_dir: bool) -> io::Result<bool> {
        self.prepare_source()?;
        let lexical = self.root.join(relative);
        let canonical = self.canonical_root.as_ref().map(|root| root.join(relative));
        let mut any_git = false;
        if self.respect_gitignore {
            for directory in lexical.ancestors().skip(1) {
                if !directory.starts_with(&self.root) {
                    break;
                }
                any_git |= self.has_git(directory)?;
            }
            if let Some(root) = &self.canonical_root {
                for directory in root.ancestors().skip(1) {
                    any_git |= self.has_git(directory)?;
                }
            }
        }
        let mut decisions = [None; 3];
        let mut saw_git = false;
        for directory in lexical.ancestors().skip(1) {
            if !directory.starts_with(&self.root) {
                break;
            }
            self.match_directory(
                directory,
                &lexical,
                is_dir,
                any_git && !saw_git,
                &mut decisions,
            )?;
            saw_git |= self.respect_gitignore && self.has_git(directory)?;
        }
        // Do not canonicalize a followed descendant: its lexical source rules
        // must remain in the chain. Canonical parents are appended once only.
        if let (Some(root), Some(candidate)) = (self.canonical_root.clone(), canonical) {
            for directory in root.ancestors().skip(1) {
                self.match_directory(
                    directory,
                    &candidate,
                    is_dir,
                    any_git && !saw_git,
                    &mut decisions,
                )?;
                saw_git |= self.respect_gitignore && self.has_git(directory)?;
            }
        }
        if let Some(decision) = decisions.into_iter().flatten().next() {
            return Ok(decision);
        }
        if self.respect_gitignore {
            if let Some(cwd) = self.cwd.clone() {
                if any_git {
                    if let Some(file) = self.global_file.clone() {
                        if let Some(decision) = self.match_file(&cwd, &file, &lexical, is_dir)? {
                            return Ok(decision);
                        }
                    }
                }
                // add_ignore(root/.gitignore) is always explicit, including in
                // repositories; upstream anchors it at CWD, not source root.
                let file = self.root.join(".gitignore");
                if let Some(decision) = self.match_file(&cwd, &file, &lexical, is_dir)? {
                    return Ok(decision);
                }
            }
        }
        Ok(false)
    }

    fn match_directory(
        &mut self,
        directory: &Path,
        candidate: &Path,
        is_dir: bool,
        git_active: bool,
        decisions: &mut [Option<bool>; 3],
    ) -> io::Result<()> {
        if decisions[0].is_none() {
            decisions[0] =
                self.match_file(directory, &directory.join(".ignore"), candidate, is_dir)?;
        }
        if git_active {
            if decisions[1].is_none() {
                decisions[1] =
                    self.match_file(directory, &directory.join(".gitignore"), candidate, is_dir)?;
            }
            if decisions[2].is_none() {
                if let Some(git_dir) = resolve_git_dir(directory, self)? {
                    decisions[2] = self.match_file(
                        directory,
                        &git_dir.join("info/exclude"),
                        candidate,
                        is_dir,
                    )?;
                }
            }
        }
        Ok(())
    }

    /// Deletion also prunes ignored ancestors and retains the earlier
    /// source-root-anchored fallback and conservative per-directory checks.
    /// Exact source matching must not reduce established deletion protection.
    pub fn is_ignored(&mut self, relative: &Path, is_dir: bool) -> bool {
        match self.deletion_ignored(relative, is_dir) {
            Ok(ignored) => ignored,
            Err(error) => {
                tracing::warn!("source ignore scope protects on failure: {error}");
                true
            }
        }
    }

    fn deletion_ignored(&mut self, relative: &Path, is_dir: bool) -> io::Result<bool> {
        for ancestor in relative
            .ancestors()
            .skip(1)
            .filter(|path| !path.as_os_str().is_empty())
        {
            if self.source_match(ancestor, true)? || self.legacy_match(ancestor, true)? {
                return Ok(true);
            }
        }
        Ok(self.source_match(relative, is_dir)? || self.legacy_match(relative, is_dir)?)
    }

    pub fn protects(&mut self, entry: &Entry) -> bool {
        self.is_ignored(entry.path.as_path(), entry.is_directory())
    }

    fn legacy_match(&mut self, relative: &Path, is_dir: bool) -> io::Result<bool> {
        let absolute = self.root.join(relative);
        let mut chain = None;
        for directory in absolute.ancestors().skip(1) {
            if !directory.starts_with(&self.root) {
                break;
            }
            for name in [".ignore", ".gitignore"] {
                if name == ".gitignore" && !self.respect_gitignore {
                    continue;
                }
                chain = self.match_file(directory, &directory.join(name), &absolute, is_dir)?;
                if chain.is_some() {
                    break;
                }
            }
            if chain.is_some() {
                break;
            }
        }
        if chain == Some(true) {
            return Ok(true);
        }
        if !self.respect_gitignore {
            return Ok(false);
        }
        let root = self.root.clone();
        if self.has_git(&root)? {
            if let Some(git_dir) = resolve_legacy_git_dir(&root, self)? {
                if let Some(decision) =
                    self.match_file(&root, &git_dir.join("info/exclude"), &absolute, is_dir)?
                {
                    return Ok(decision);
                }
            }
            if let Some(file) = self.global_file.clone() {
                return Ok(self
                    .match_file(&root, &file, &absolute, is_dir)?
                    .unwrap_or(false));
            }
            Ok(false)
        } else {
            Ok(self
                .match_file(&root, &root.join(".gitignore"), &absolute, is_dir)?
                .unwrap_or(false))
        }
    }

    fn match_file(
        &mut self,
        root: &Path,
        file: &Path,
        candidate: &Path,
        is_dir: bool,
    ) -> io::Result<Option<bool>> {
        Ok(
            match self
                .load_file(root, file)?
                .matcher
                .matched(candidate, is_dir)
            {
                ignore::Match::None => None,
                ignore::Match::Ignore(_) => Some(true),
                ignore::Match::Whitelist(_) => Some(false),
            },
        )
    }

    fn load_file(&mut self, root: &Path, file: &Path) -> io::Result<&RuleFile> {
        let index = self
            .dir_rules
            .iter()
            .position(|(anchor, path, _)| anchor == root && path == file);
        if let Some(index) = index {
            if let Some(cached) = self.dir_rules.remove(index) {
                self.dir_rules.push_front(cached);
            }
        } else {
            let matcher = compile_matcher(root, file, self.read_config(file)?)?;
            if self.dir_rules.len() == DIRECTORY_RULE_CACHE_CAPACITY {
                self.dir_rules.pop_back();
            }
            // Missing files are cached too. There are no tree-sized maps.
            self.dir_rules
                .push_front((root.to_path_buf(), file.to_path_buf(), matcher));
        }
        Ok(&self.dir_rules[0].2)
    }
}

fn read_config(path: &Path) -> io::Result<Option<Vec<u8>>> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // A config path may be an adversarial FIFO; inspect without blocking.
        options.custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        // Upstream ignores unreadable rule/config files.
        Err(_) => return Ok(None),
    };
    read_config_file(file, path)
}

fn read_config_file(file: std::fs::File, path: &Path) -> io::Result<Option<Vec<u8>>> {
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "ignore configuration is not a regular file: {}",
                path.display()
            ),
        ));
    }
    let mut bytes = Vec::new();
    file.take(MAX_CONFIG_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_CONFIG_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "ignore configuration exceeds {MAX_CONFIG_BYTES} bytes: {}",
                path.display()
            ),
        ));
    }
    Ok(Some(bytes))
}

fn compile_matcher(root: &Path, file: &Path, bytes: Option<Vec<u8>>) -> io::Result<RuleFile> {
    let Some(bytes) = bytes else {
        return Ok(RuleFile {
            matcher: Gitignore::empty(),
            malformed: false,
        });
    };
    let mut builder = GitignoreBuilder::new(root);
    let mut malformed = false;
    // split_inclusive reproduces BufRead::lines (including CRLF and no final
    // newline), but cannot allocate an unbounded individual line.
    for (index, bytes) in bytes.split_inclusive(|byte| *byte == b'\n').enumerate() {
        if index >= MAX_RULES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "ignore configuration exceeds {MAX_RULES} lines: {}",
                    file.display()
                ),
            ));
        }
        let Ok(line) = std::str::from_utf8(bytes) else {
            malformed = true;
            break;
        };
        let line = line
            .strip_suffix('\n')
            .map(|line| line.strip_suffix('\r').unwrap_or(line))
            .unwrap_or(line);
        let line = if index == 0 {
            line.trim_start_matches('\u{feff}')
        } else {
            line
        };
        if let Err(error) = builder.add_line(Some(file.to_path_buf()), line) {
            // A partial parse error must not discard valid preceding/following
            // rules. WalkBuilder attaches these errors to the directory entry.
            tracing::warn!("source ignore scope: {}: {error}", file.display());
            malformed = true;
        }
    }
    match builder.build() {
        Ok(matcher) => Ok(RuleFile { matcher, malformed }),
        Err(error) => {
            tracing::warn!("source ignore scope: {}: {error}", file.display());
            Ok(RuleFile {
                matcher: Gitignore::empty(),
                malformed: true,
            })
        }
    }
}

// Keep upstream's first-line gitdir/commondir interpretation, including its
// CWD-relative gitdir behavior. It intentionally does not use git's env vars.
fn resolve_git_dir(directory: &Path, scope: &SourceIgnoreScope) -> io::Result<Option<PathBuf>> {
    let dot_git = directory.join(".git");
    if scope.is_file(&dot_git)? != Some(true) {
        return Ok(Some(dot_git));
    }
    let Some(bytes) = scope.read_config(&dot_git)? else {
        return Ok(None);
    };
    let Some(line) = first_line(&bytes) else {
        return Ok(None);
    };
    let Some(gitdir) = line.strip_prefix("gitdir: ") else {
        return Ok(None);
    };
    let gitdir = PathBuf::from(gitdir);
    let Some(bytes) = scope.read_config(&gitdir.join("commondir"))? else {
        return Ok(None);
    };
    let Some(common) = first_line(&bytes) else {
        return Ok(None);
    };
    Ok(Some(if common.starts_with('.') {
        gitdir.join(common)
    } else {
        PathBuf::from(common)
    }))
}

fn resolve_legacy_git_dir(
    directory: &Path,
    scope: &SourceIgnoreScope,
) -> io::Result<Option<PathBuf>> {
    let dot_git = directory.join(".git");
    if scope.is_file(&dot_git)? != Some(true) {
        return Ok(Some(dot_git));
    }
    let Some(bytes) = scope.read_config(&dot_git)? else {
        return Ok(Some(dot_git));
    };
    let Ok(contents) = std::str::from_utf8(&bytes) else {
        return Ok(Some(dot_git));
    };
    let Some(gitdir) = contents.strip_prefix("gitdir: ") else {
        return Ok(Some(dot_git));
    };
    let gitdir = PathBuf::from(gitdir.trim());
    if let Some(bytes) = scope.read_config(&gitdir.join("commondir"))? {
        if let Ok(contents) = std::str::from_utf8(&bytes) {
            let common = contents.trim();
            if !common.is_empty() {
                let candidate = if common.starts_with('.') {
                    gitdir.join(common)
                } else {
                    PathBuf::from(common)
                };
                if let Some(resolved) = scope.canonical_config_path(&candidate)? {
                    return Ok(Some(resolved));
                }
            }
        }
    }
    // The earlier deletion owner fell back to gitdir when commondir was
    // missing/unresolvable. Retain that extra protection, unlike source matching.
    Ok(Some(gitdir))
}

fn first_line(bytes: &[u8]) -> Option<&str> {
    if bytes.is_empty() {
        return None;
    }
    let line = bytes.split_inclusive(|byte| *byte == b'\n').next()?;
    let line = std::str::from_utf8(line).ok()?;
    Some(
        line.strip_suffix('\n')
            .map(|line| line.strip_suffix('\r').unwrap_or(line))
            .unwrap_or(line),
    )
}

fn global_excludes() -> io::Result<Option<PathBuf>> {
    // This deliberately duplicates the public ignore crate resolver's small
    // policy, not its unbounded reads. Feed the exact upstream regex bounded
    // bytes; HOME config takes precedence over XDG config, then XDG ignore.
    #[allow(deprecated)]
    let home = std::env::home_dir();
    let xdg = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|path| path.join(".config")));
    let re = regex::bytes::Regex::new(r#"(?im-u)^\s*excludesfile\s*=\s*"?\s*(\S+?)\s*"?\s*$"#)
        .map_err(io::Error::other)?;
    for path in [
        home.as_ref().map(|path| path.join(".gitconfig")),
        xdg.as_ref().map(|path| path.join("git/config")),
    ]
    .into_iter()
    .flatten()
    {
        if let Some(bytes) = read_config(&path)? {
            if let Some(capture) = re.captures(&bytes).and_then(|captures| captures.get(1)) {
                if let Ok(path) = std::str::from_utf8(capture.as_bytes()) {
                    return Ok(Some(PathBuf::from(match &home {
                        Some(home) => path.replace('~', &home.to_string_lossy()),
                        None => path.to_owned(),
                    })));
                }
            }
        }
    }
    Ok(xdg.map(|path| path.join("git/ignore")))
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
        #[cfg(unix)]
        {
            let root = crate::endpoint::source_root::SourceRoot::open_blocking(root.to_path_buf())
                .unwrap();
            SourceIgnoreScope::with_rooted_authority(root.rooted(), true)
        }
        #[cfg(not(unix))]
        {
            SourceIgnoreScope::new(root, true)
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn canonical_ignore_ancestry_comes_from_held_root_not_retargeted_operand() {
        let parent = tempfile::tempdir().unwrap();
        let original = parent.path().join("original");
        let replacement = parent.path().join("replacement");
        let operand = parent.path().join("operand");
        std::fs::create_dir(&original).unwrap();
        std::fs::create_dir(&replacement).unwrap();
        std::os::unix::fs::symlink(&original, &operand).unwrap();
        let root =
            crate::endpoint::source_root::SourceRoot::open_blocking(operand.clone()).unwrap();
        let expected = original.canonicalize().unwrap();
        std::fs::remove_file(&operand).unwrap();
        std::os::unix::fs::symlink(&replacement, &operand).unwrap();
        // A transient retarget between validation and ancestry initialization
        // cannot supply another tree's external parent rules.
        assert_eq!(
            root.rooted()
                .source_configuration_root_path_blocking()
                .unwrap(),
            expected
        );
    }

    #[cfg(unix)]
    #[test]
    fn replacement_root_rules_cannot_be_cached_as_original_deletion_protection() {
        let parent = tempfile::tempdir().unwrap();
        let original = parent.path().join("source");
        let replacement = parent.path().join("replacement");
        let held = parent.path().join("held");
        for root in [&original, &replacement] {
            std::fs::create_dir(root).unwrap();
            std::fs::create_dir(root.join("sub")).unwrap();
        }
        std::fs::write(original.join("sub/.ignore"), b"victim\n").unwrap();
        let mut scope = scope(&original);
        // The source producer has finished, but this destination-only rule
        // lookup has not occurred yet. Missing replacement rules must not grant
        // delete authority or poison the cache after the original root returns.
        std::fs::rename(&original, &held).unwrap();
        std::fs::rename(&replacement, &original).unwrap();
        assert!(scope.is_ignored(Path::new("sub/victim"), false));
        std::fs::rename(&original, &replacement).unwrap();
        std::fs::rename(&held, &original).unwrap();
        assert!(scope.is_ignored(Path::new("sub/victim"), false));
        assert!(!scope.is_ignored(Path::new("sub/ordinary"), false));
    }

    #[test]
    fn source_precedence_does_not_reduce_conservative_deletion_protection() {
        let root = repo_root();
        std::fs::create_dir(root.path().join("sub")).unwrap();
        std::fs::write(root.path().join(".ignore"), b"hidden\n!kept\n").unwrap();
        std::fs::write(root.path().join("sub/.gitignore"), b"!hidden\nkept\n").unwrap();
        let mut scope = scope(root.path());
        assert!(scope.source_match(Path::new("sub/hidden"), false).unwrap());
        assert!(!scope.source_match(Path::new("sub/kept"), false).unwrap());
        // Previous deletion scope protected the deeper .gitignore match;
        // source parity is not authorization to delete previously shielded data.
        assert!(scope.is_ignored(Path::new("sub/kept"), false));

        let root = tempfile::TempDir::new().unwrap();
        std::fs::write(root.path().join(".gitignore"), b"/root-only\n").unwrap();
        let mut scope = SourceIgnoreScope::new(root.path(), true);
        assert!(!scope.source_match(Path::new("root-only"), false).unwrap());
        assert!(scope.is_ignored(Path::new("root-only"), false));
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
    fn rule_cache_stays_bounded_and_eviction_preserves_ignore_decisions() {
        let root = repo_root();
        std::fs::write(root.path().join(".gitignore"), b"root-only\n").unwrap();
        let mut scope = scope(root.path());
        for index in 0..DIRECTORY_RULE_CACHE_CAPACITY * 3 {
            let name = format!("dir-{index}");
            let directory = root.path().join(&name);
            std::fs::create_dir(&directory).unwrap();
            std::fs::write(directory.join(".gitignore"), b"*.private\n!keep.private\n").unwrap();
            for (file, ignored) in [
                ("drop.private", true),
                ("keep.private", false),
                ("root-only", true),
            ] {
                assert_eq!(
                    scope.is_ignored(&Path::new(&name).join(file), false),
                    ignored
                );
                assert!(scope.dir_rules.len() <= DIRECTORY_RULE_CACHE_CAPACITY);
            }
        }
        assert!(!scope
            .dir_rules
            .iter()
            .any(|(path, _, _)| path == &root.path().join("dir-0")));
        assert!(scope.is_ignored(Path::new("dir-0/drop.private"), false));
        assert!(!scope.is_ignored(Path::new("dir-0/keep.private"), false));
        assert!(scope.is_ignored(Path::new("dir-0/root-only"), false));
        assert!(scope.dir_rules.len() <= DIRECTORY_RULE_CACHE_CAPACITY);
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
        /// Exercise real wildcard/negation/pruning rules, not just parser
        /// errors. Exact source matching must agree with emitted paths;
        /// conservative deletion protection is tested separately.
        #[test]
        fn proptest_matches_walk_selection(
            root_rules in proptest::collection::vec(
                prop::sample::select(vec!["a", "b", "a*", "!ab", "sub/*", "!sub/a", "**/b", "sub/"]), 0..4),
            nested_rules in proptest::collection::vec(
                prop::sample::select(vec!["a", "a*", "!a", "b", "!b", "*", "!*", "c/"]), 0..4),
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
                    !scope.source_match(path, is_dir).unwrap(),
                    "walker emitted {} but source matcher ignores it",
                    path.display()
                );
            }
        }
    }
}
