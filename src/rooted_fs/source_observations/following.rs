//! ROOT-confined FILE resolution through held no-follow directories. These
//! records are read provenance, never synthetic physical inventory or roots.
use super::*;
use std::collections::VecDeque;

pub(super) enum ResolutionStep {
    Entry(ResolutionEntry),
    Root(RootReset),
}

pub(super) struct ResolutionEntry {
    pub(super) name: OsString,
    pub(super) named: NativeObservation,
    pub(super) object_key: Option<NativeObjectKey>,
    pub(super) target: Option<LinkTarget>,
}

pub(super) struct RootReset {
    pub(super) address: LinkTarget,
    pub(super) named: NativeObservation,
    pub(super) object_key: NativeObjectKey,
}

/// Relative resolution needs only a genuine held ROOT. Absolute resets also
/// need the original catalogue and a natively qualified ROOT address.
pub(super) enum ResolutionRoot<'a> {
    Held(RawFd),
    Catalogued {
        rooted: &'a RootedFs,
        catalogue: &'a mut Catalogue,
    },
}

impl ResolutionRoot<'_> {
    fn fd(&self) -> RawFd {
        match self {
            Self::Held(fd) => *fd,
            Self::Catalogued { rooted, .. } => rooted.root_fd.as_raw_fd(),
        }
    }

    fn qualify(&mut self, address: &LinkTarget, path: &Path) -> Result<RootReset> {
        match self {
            Self::Catalogued { rooted, catalogue } => {
                catalogue.qualify_absolute_root(rooted, address, path)
            }
            Self::Held(_) => Err(RootedFsError::SourceFollowUnsupported(path.to_path_buf())),
        }
    }

    fn absolute(&mut self, target: &Path, path: &Path) -> Result<(VecDeque<OsString>, RootReset)> {
        let Self::Catalogued { rooted, catalogue } = self else {
            return Err(RootedFsError::SourceFollowUnsupported(path.to_path_buf()));
        };
        // Select only an original operator address or FD-derived candidate;
        // native replay qualifies it. Never probe target-derived outside paths.
        let has_operator = catalogue.operator.is_some();
        let operator_address = LinkTarget::new(rooted.root_path.as_os_str());
        if let Ok(suffix) = absolute_suffix(&operator_address, target) {
            if has_operator {
                return Ok((suffix, self.qualify(&operator_address, path)?));
            }
        }
        let address = native_root_address(self.fd())?
            .ok_or_else(|| RootedFsError::SourceFollowUnsupported(path.to_path_buf()))?;
        let suffix = absolute_suffix(&address, target)
            .map_err(|_| RootedFsError::SourceFollowUnsupported(path.to_path_buf()))?;
        Ok((suffix, self.qualify(&address, path)?))
    }
}

pub(super) fn same_resolution(left: &[ResolutionStep], right: &[ResolutionStep]) -> bool {
    left.len() == right.len() && left.iter().zip(right).all(|(a, b)| a.matches(b))
}

/// A pathname from a held FD selects a candidate only. It never establishes
/// binding or view equality; qualification must reopen that exact prefix.
pub(super) fn native_root_address(fd: RawFd) -> Result<Option<LinkTarget>> {
    #[cfg(target_os = "macos")]
    {
        let mut bytes = [0u8; libc::PATH_MAX as usize];
        // SAFETY: fd is held; F_GETPATH writes at most PATH_MAX bytes to bytes.
        if unsafe { libc::fcntl(fd, libc::F_GETPATH, bytes.as_mut_ptr()) } == -1 {
            return Err(io::Error::last_os_error().into());
        }
        let bytes = std::ffi::CStr::from_bytes_until_nul(&bytes).map_err(|_| invalid())?;
        Ok(Some(LinkTarget::new(OsStr::from_bytes(bytes.to_bytes()))))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = fd;
        Ok(None)
    }
}

mod beneath;
mod record;
use record::{absolute_suffix, parse_target};

type DirectoryStack = Vec<(usize, [u8; 32])>;

fn add_bytes(size: &mut usize, count: usize) -> io::Result<()> {
    *size = size
        .checked_add(count)
        .filter(|size| *size <= MAX_TARGET)
        .ok_or_else(invalid)?;
    Ok(())
}

// Indices identify authenticated edges, not FDs or native-parent certificates.
fn enter_directory(
    stack: &mut DirectoryStack,
    parent: &mut [u8; 32],
    steps: &[ResolutionStep],
    index: usize,
    step: &ResolutionEntry,
) -> io::Result<()> {
    if step.name == OsStr::new("..") {
        *parent = stack.pop().ok_or_else(invalid)?.1;
    } else if step.name != OsStr::new(".") {
        stack.push((index, *parent));
        *parent = key(*parent, &step.name);
        return Ok(());
    }
    if let Some((index, _)) = stack.last() {
        let expected = steps.get(*index).ok_or_else(invalid)?.entry()?;
        if !expected.named.matches(step.named, true) || expected.object_key != step.object_key {
            return Err(invalid());
        }
    }
    Ok(())
}

fn visit_link(
    visited: &mut Vec<([u8; 32], NativeObservation)>,
    parent: [u8; 32],
    named: NativeObservation,
) -> io::Result<()> {
    if visited.len() == MAX_FOLLOW_LINKS
        || visited
            .iter()
            .any(|(scope, previous)| *scope == parent && previous.same_binding(named))
    {
        return Err(invalid());
    }
    visited.push((parent, named));
    Ok(())
}

pub(super) struct FollowedFile {
    pub(super) resolution_start: usize,
    pub(super) binding: TerminalBinding,
    pub(super) resolution: Vec<ResolutionStep>,
}

pub(super) fn inspect_file(
    mut root: ResolutionRoot<'_>,
    parent: RawFd,
    path: &Path,
    source: NativeObservation,
    target: &Path,
) -> Result<FollowedFile> {
    let unsupported = || RootedFsError::SourceFollowUnsupported(path.to_path_buf());
    let parsed = parse_target(target).map_err(|_| unsupported())?;
    let mut pending = parsed.components;
    let mut absolute = parsed.absolute.then(|| LinkTarget::new(target.as_os_str()));
    let root_fd = root.fd();
    let mut size = target.as_os_str().len();
    let mut resolution = Vec::new();
    let mut cursor = operator::directory_at(root_fd, OsStr::new("."))?;
    let mut relative = PathBuf::new();
    let mut raw = PathBuf::new();
    let mut stack = Vec::new();
    let mut parent_key = [0; 32];
    for component in path.parent().unwrap_or(Path::new("")).components() {
        let Component::Normal(name) = component else {
            return Err(RootedFsError::InvalidRelativePath);
        };
        if resolution.len() == MAX_FOLLOW_STEPS {
            return Err(unsupported());
        }
        add_bytes(&mut size, name.len()).map_err(|_| unsupported())?;
        let stat = stat_at_no_follow(cursor.as_raw_fd(), name)
            .map_err(|error| source_read_race(error, path))?;
        let named = NativeObservation::from_stat(&stat)?;
        if named.kind != libc::S_IFDIR as u64 {
            return Err(unsupported());
        }
        let step = ResolutionEntry {
            name: name.to_os_string(),
            named,
            object_key: object_key(cursor.as_raw_fd(), Some(name))
                .map_err(|error| source_read_race(error, path))?,
            target: None,
        };
        cursor = open_recorded_directory(cursor.as_raw_fd(), path, &step)?;
        relative.push(name);
        raw.push(name);
        enter_directory(
            &mut stack,
            &mut parent_key,
            &resolution,
            resolution.len(),
            &step,
        )?;
        resolution.push(ResolutionStep::Entry(step));
    }
    if !NativeObservation::from_stat(&stat_fd(cursor.as_raw_fd())?)?
        .same_binding(NativeObservation::from_stat(&stat_fd(parent)?)?)
        || object_key(cursor.as_raw_fd(), None)? != object_key(parent, None)?
    {
        return Err(RootedFsError::SourceBindingChanged(path.to_path_buf()));
    }
    let resolution_start = resolution.len();
    let mut visited = vec![(parent_key, source)];
    loop {
        if let Some(target) = absolute.take() {
            if resolution.len() == MAX_FOLLOW_STEPS {
                return Err(unsupported());
            }
            let (suffix, reset) = root.absolute(&target, path)?;
            add_bytes(&mut size, reset.address.as_os_str().len()).map_err(|_| unsupported())?;
            pending = suffix;
            cursor = operator::directory_at(root_fd, OsStr::new("."))?;
            relative.clear();
            raw.clear();
            stack.clear();
            parent_key = [0; 32];
            resolution.push(ResolutionStep::Root(reset));
        }
        let Some(name) = pending.pop_front() else {
            break;
        };
        if resolution.len() == MAX_FOLLOW_STEPS {
            return Err(unsupported());
        }
        add_bytes(&mut size, name.len()).map_err(|_| unsupported())?;
        let ascending = name == OsStr::new("..");
        if ascending && stack.is_empty() {
            return Err(unsupported());
        }
        if name != OsStr::new(".") && !ascending {
            inventory::validate_original_path(&relative.join(&name))?;
        }
        let returned_parent = if ascending {
            Some(native_parent(
                root_fd,
                path,
                &resolution,
                &stack,
                cursor.as_raw_fd(),
                &raw,
            )?)
        } else {
            None
        };
        let (stat, object_key) = if let Some(parent) = &returned_parent {
            (
                stat_fd(parent.as_raw_fd())?,
                object_key(parent.as_raw_fd(), None)?,
            )
        } else {
            let stat =
                stat_at_no_follow(cursor.as_raw_fd(), &name).map_err(|error| match error {
                    RootedFsError::Io(error) if error.raw_os_error() == Some(libc::ENOENT) => {
                        unsupported()
                    }
                    error => error,
                })?;
            let object_key = object_key(cursor.as_raw_fd(), Some(&name))
                .map_err(|error| source_read_race(error, path))?;
            (stat, object_key)
        };
        let named = NativeObservation::from_stat(&stat)?;
        let target = match stat.st_mode & libc::S_IFMT {
            libc::S_IFDIR if !pending.is_empty() => None,
            libc::S_IFREG if pending.is_empty() => None,
            libc::S_IFLNK if pending.is_empty() => {
                visit_link(&mut visited, parent_key, named).map_err(|_| unsupported())?;
                let target = observed_target_at(cursor.as_raw_fd(), &name, path)?;
                add_bytes(&mut size, target.as_os_str().len()).map_err(|_| unsupported())?;
                let parsed = parse_target(&target).map_err(|_| unsupported())?;
                pending = parsed.components;
                absolute = parsed.absolute.then(|| target.clone());
                Some(target)
            }
            _ => return Err(unsupported()),
        };
        let step = ResolutionEntry {
            name,
            named,
            object_key,
            target,
        };
        if let Some(parent) = returned_parent {
            // Keep the actual native BENEATH result, never the stack's guess.
            validate_cursor(parent.as_raw_fd(), path, &step)?;
            cursor = parent;
        } else {
            validate_step(cursor.as_raw_fd(), path, &step)?;
            if named.kind == libc::S_IFDIR as u64 {
                cursor = open_recorded_directory(cursor.as_raw_fd(), path, &step)?;
            }
        }
        if named.kind == libc::S_IFDIR as u64 {
            enter_directory(
                &mut stack,
                &mut parent_key,
                &resolution,
                resolution.len(),
                &step,
            )
            .map_err(|_| unsupported())?;
            raw.push(&step.name); // Includes every literal dot/parent; never cancel.
            if ascending {
                relative.pop();
            } else if step.name != OsStr::new(".") {
                relative.push(&step.name);
            }
        }
        resolution.push(ResolutionStep::Entry(step));
        if named.kind == libc::S_IFREG as u64 {
            drop(cursor);
            let binding = replay_resolution(&mut root, path, &resolution)?;
            return Ok(FollowedFile {
                resolution_start,
                binding,
                resolution,
            });
        }
    }
    Err(unsupported())
}

fn validate_cursor(fd: RawFd, path: &Path, step: &ResolutionEntry) -> Result<()> {
    if !step
        .named
        .matches(NativeObservation::from_stat(&stat_fd(fd)?)?, true)
        || object_key(fd, None)? != step.object_key
    {
        return Err(RootedFsError::SourceBindingChanged(path.to_path_buf()));
    }
    Ok(())
}

fn validate_step(parent: RawFd, path: &Path, step: &ResolutionEntry) -> Result<libc::stat> {
    let stat =
        stat_at_no_follow(parent, &step.name).map_err(|error| source_read_race(error, path))?;
    if step.name == OsStr::new(".") {
        validate_cursor(parent, path, step)?;
    }
    if !step
        .named
        .matches(NativeObservation::from_stat(&stat)?, true)
        || object_key(parent, Some(&step.name)).map_err(|error| source_read_race(error, path))?
            != step.object_key
    {
        return Err(RootedFsError::SourceBindingChanged(path.to_path_buf()));
    }
    if let Some(target) = &step.target {
        if &observed_target_at(parent, &step.name, path)? != target {
            return Err(RootedFsError::SourceBindingChanged(path.to_path_buf()));
        }
    }
    Ok(stat)
}

fn open_recorded_directory(parent: RawFd, path: &Path, step: &ResolutionEntry) -> Result<OwnedFd> {
    let child = operator::directory_at(parent, &step.name)
        .map_err(|error| source_read_race(error, path))?;
    validate_cursor(child.as_raw_fd(), path, step)?;
    validate_step(parent, path, step)?;
    Ok(child)
}

fn native_parent(
    root: RawFd,
    path: &Path,
    steps: &[ResolutionStep],
    stack: &DirectoryStack,
    current: RawFd,
    raw: &Path,
) -> Result<OwnedFd> {
    let (child, _) = stack
        .last()
        .ok_or_else(|| RootedFsError::SourceFollowUnsupported(path.to_path_buf()))?;
    let parent = beneath::directory(root, &raw.join(".."), path)?;
    let (expected, expected_key) = if stack.len() == 1 {
        (
            NativeObservation::from_stat(&stat_fd(root)?)?,
            object_key(root, None)?,
        )
    } else {
        let step = steps[stack[stack.len() - 2].0].entry()?;
        (step.named, step.object_key)
    };
    if !expected.matches(
        NativeObservation::from_stat(&stat_fd(parent.as_raw_fd())?)?,
        true,
    ) || expected_key != object_key(parent.as_raw_fd(), None)?
    {
        return Err(RootedFsError::SourceFollowUnsupported(path.to_path_buf()));
    }
    // Do not infer an inverse edge from object equality. The ROOT-bounded
    // native replay above supplies parent semantics for THAT traversal.
    let child = steps[*child].entry()?;
    validate_cursor(current, path, child)?;
    validate_step(parent.as_raw_fd(), path, child)?;
    Ok(parent)
}

fn replay_resolution(
    root: &mut ResolutionRoot<'_>,
    path: &Path,
    resolution: &[ResolutionStep],
) -> Result<TerminalBinding> {
    let mut cursor = operator::directory_at(root.fd(), OsStr::new("."))?;
    let mut raw = PathBuf::new();
    let mut stack = Vec::new();
    let mut parent_key = [0; 32];
    for (index, step) in resolution.iter().enumerate() {
        let step = match step {
            ResolutionStep::Root(expected) => {
                if !expected.matches(&root.qualify(&expected.address, path)?) {
                    return Err(RootedFsError::SourceBindingChanged(path.to_path_buf()));
                }
                cursor = operator::directory_at(root.fd(), OsStr::new("."))?;
                raw.clear();
                stack.clear();
                parent_key = [0; 32];
                continue;
            }
            ResolutionStep::Entry(step) => step,
        };
        if step.name == OsStr::new("..") {
            cursor = native_parent(
                root.fd(),
                path,
                resolution,
                &stack,
                cursor.as_raw_fd(),
                &raw,
            )?;
            validate_cursor(cursor.as_raw_fd(), path, step)?;
            enter_directory(&mut stack, &mut parent_key, resolution, index, step)?;
            raw.push(&step.name);
            continue;
        }
        let stat = validate_step(cursor.as_raw_fd(), path, step)?;
        if step.named.kind == libc::S_IFDIR as u64 {
            cursor = open_recorded_directory(cursor.as_raw_fd(), path, step)?;
            enter_directory(&mut stack, &mut parent_key, resolution, index, step)?;
            raw.push(&step.name);
        } else if step.named.kind == libc::S_IFREG as u64 {
            return Ok(TerminalBinding {
                parent: cursor,
                name: step.name.clone(),
                stat,
                object_key: step.object_key,
            });
        }
    }
    Err(invalid().into())
}

#[cfg(test)]
mod parent_tests;

#[cfg(all(test, target_os = "macos"))]
mod absolute_tests;
