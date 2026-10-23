//! ROOT-confined relative FILE resolution through held no-follow directories. These
//! records are read provenance, never synthetic physical inventory or roots.
use super::*;
use std::collections::VecDeque;

pub(super) struct ResolutionStep {
    pub(super) name: OsString,
    pub(super) named: NativeObservation,
    pub(super) object_key: Option<NativeObjectKey>,
    pub(super) target: Option<LinkTarget>,
}

impl ResolutionStep {
    pub(super) fn encode(&self, bytes: &mut Vec<u8>) {
        let name = self.name.as_bytes();
        bytes.extend_from_slice(&(name.len() as u32).to_le_bytes());
        bytes.extend_from_slice(name);
        self.named.encode(bytes);
        bytes.extend_from_slice(&self.object_key.map_or([0; 32], |key| key.0));
        if let Some(target) = &self.target {
            let target = target.as_os_str().as_bytes();
            bytes.extend_from_slice(&(target.len() as u32).to_le_bytes());
            bytes.extend_from_slice(target);
        } else {
            bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        }
    }

    pub(super) fn decode(bytes: &mut &[u8]) -> io::Result<Self> {
        let length = u32::from_le_bytes(take(bytes)?) as usize;
        if length > MAX_TARGET {
            return Err(invalid());
        }
        let name = OsStr::from_bytes(field(bytes, length)?).to_os_string();
        let named = NativeObservation::decode(bytes)?;
        let object = take::<32>(bytes)?;
        let object_key = (object != [0; 32]).then_some(NativeObjectKey(object));
        let length = u32::from_le_bytes(take(bytes)?);
        let target = if length == u32::MAX {
            None
        } else {
            if length as usize > MAX_TARGET {
                return Err(invalid());
            }
            Some(LinkTarget::new(OsStr::from_bytes(field(
                bytes,
                length as usize,
            )?)))
        };
        Ok(Self {
            name,
            named,
            object_key,
            target,
        })
    }

    fn matches(&self, other: &Self) -> bool {
        self.name == other.name
            && self.named.matches(other.named, true)
            && self.object_key == other.object_key
            && self.target == other.target
    }
}

pub(super) fn same_resolution(left: &[ResolutionStep], right: &[ResolutionStep]) -> bool {
    left.len() == right.len() && left.iter().zip(right).all(|(a, b)| a.matches(b))
}

mod beneath;

// Separator runs are not components; explicit dots and parents are. Authenticate
// the original LinkTarget separately, never through Path's normalized equality.
fn relative_components(target: &Path) -> io::Result<VecDeque<OsString>> {
    let bytes = target.as_os_str().as_bytes();
    if bytes.is_empty()
        || bytes.len() > MAX_TARGET
        || bytes.starts_with(b"/")
        || bytes.ends_with(b"/")
    {
        return Err(invalid());
    }
    let mut components = VecDeque::new();
    for name in bytes
        .split(|byte| *byte == b'/')
        .filter(|name| !name.is_empty())
    {
        if components.len() == MAX_FOLLOW_STEPS
            || (name != b"."
                && name != b".."
                && sibling_name(Path::new(OsStr::from_bytes(name))).is_none())
        {
            return Err(invalid());
        }
        components.push_back(OsStr::from_bytes(name).to_os_string());
    }
    Ok(components)
}

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
    step: &ResolutionStep,
) -> io::Result<()> {
    if step.name == OsStr::new("..") {
        *parent = stack.pop().ok_or_else(invalid)?.1;
    } else if step.name != OsStr::new(".") {
        stack.push((index, *parent));
        *parent = key(*parent, &step.name);
        return Ok(());
    }
    if let Some((index, _)) = stack.last() {
        let expected = steps.get(*index).ok_or_else(invalid)?;
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

impl Observation {
    pub(super) fn validate_resolution(&self) -> io::Result<()> {
        // The original selected exception has its own authenticated operator tape.
        if self.resolution.is_empty() {
            return if self.resolution_start == 0 {
                Ok(())
            } else {
                Err(invalid())
            };
        }
        if !self.followed
            || self.named.kind != libc::S_IFLNK as u64
            || self.effective.kind != libc::S_IFREG as u64
            || self.resolution.len() > MAX_FOLLOW_STEPS
            || self.resolution_start >= self.resolution.len()
        {
            return Err(invalid());
        }
        let target = self.target.as_deref().ok_or_else(invalid)?;
        let mut pending = relative_components(target)?;
        let mut size = target.as_os_str().len();
        let mut parent = [0; 32];
        let mut stack = Vec::new();
        for (index, step) in self.resolution[..self.resolution_start].iter().enumerate() {
            if sibling_name(Path::new(&step.name)).is_none()
                || step.named.kind != libc::S_IFDIR as u64
                || step.target.is_some()
            {
                return Err(invalid());
            }
            add_bytes(&mut size, step.name.len())?;
            enter_directory(&mut stack, &mut parent, &self.resolution, index, step)?;
        }
        if parent != self.parent {
            return Err(invalid());
        }
        let mut visited = vec![(parent, self.named)];
        for (index, step) in self
            .resolution
            .iter()
            .enumerate()
            .skip(self.resolution_start)
        {
            if pending.pop_front().as_ref() != Some(&step.name) {
                return Err(invalid());
            }
            add_bytes(&mut size, step.name.len())?;
            match step.named.kind {
                kind if kind == libc::S_IFDIR as u64
                    && !pending.is_empty()
                    && step.target.is_none() =>
                {
                    enter_directory(&mut stack, &mut parent, &self.resolution, index, step)?;
                }
                kind if kind == libc::S_IFLNK as u64 && pending.is_empty() => {
                    visit_link(&mut visited, parent, step.named)?;
                    let target = step.target.as_deref().ok_or_else(invalid)?;
                    add_bytes(&mut size, target.as_os_str().len())?;
                    pending = relative_components(target)?;
                }
                kind if kind == libc::S_IFREG as u64
                    && pending.is_empty()
                    && step.target.is_none()
                    && index + 1 == self.resolution.len()
                    && step.named == self.effective
                    && step.object_key == self.followed_object_key =>
                {
                    return Ok(())
                }
                _ => return Err(invalid()),
            }
        }
        Err(invalid())
    }
}

pub(super) struct FollowedFile {
    pub(super) resolution_start: usize,
    pub(super) binding: TerminalBinding,
    pub(super) resolution: Vec<ResolutionStep>,
}

pub(super) fn inspect_relative_file(
    root: RawFd,
    parent: RawFd,
    path: &Path,
    source: NativeObservation,
    target: &Path,
) -> Result<FollowedFile> {
    let unsupported = || RootedFsError::SourceFollowUnsupported(path.to_path_buf());
    let mut pending = relative_components(target).map_err(|_| unsupported())?;
    let mut size = target.as_os_str().len();
    let mut resolution = Vec::new();
    let mut cursor = operator::directory_at(root, OsStr::new("."))?;
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
        let step = ResolutionStep {
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
        resolution.push(step);
    }
    if !NativeObservation::from_stat(&stat_fd(cursor.as_raw_fd())?)?
        .same_binding(NativeObservation::from_stat(&stat_fd(parent)?)?)
        || object_key(cursor.as_raw_fd(), None)? != object_key(parent, None)?
    {
        return Err(RootedFsError::SourceBindingChanged(path.to_path_buf()));
    }
    let resolution_start = resolution.len();
    let mut visited = vec![(parent_key, source)];
    while let Some(name) = pending.pop_front() {
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
                root,
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
                pending = relative_components(&target).map_err(|_| unsupported())?;
                Some(target)
            }
            _ => return Err(unsupported()),
        };
        let step = ResolutionStep {
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
        resolution.push(step);
        if named.kind == libc::S_IFREG as u64 {
            drop(cursor);
            let binding = replay_resolution(root, path, &resolution)?;
            return Ok(FollowedFile {
                resolution_start,
                binding,
                resolution,
            });
        }
    }
    Err(unsupported())
}

fn validate_cursor(fd: RawFd, path: &Path, step: &ResolutionStep) -> Result<()> {
    if !step
        .named
        .matches(NativeObservation::from_stat(&stat_fd(fd)?)?, true)
        || object_key(fd, None)? != step.object_key
    {
        return Err(RootedFsError::SourceBindingChanged(path.to_path_buf()));
    }
    Ok(())
}

fn validate_step(parent: RawFd, path: &Path, step: &ResolutionStep) -> Result<libc::stat> {
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

fn open_recorded_directory(parent: RawFd, path: &Path, step: &ResolutionStep) -> Result<OwnedFd> {
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
        let step = &steps[stack[stack.len() - 2].0];
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
    let child = &steps[*child];
    validate_cursor(current, path, child)?;
    validate_step(parent.as_raw_fd(), path, child)?;
    Ok(parent)
}

fn replay_resolution(
    root: RawFd,
    path: &Path,
    resolution: &[ResolutionStep],
) -> Result<TerminalBinding> {
    let mut cursor = operator::directory_at(root, OsStr::new("."))?;
    let mut raw = PathBuf::new();
    let mut stack = Vec::new();
    let mut parent_key = [0; 32];
    for (index, step) in resolution.iter().enumerate() {
        if step.name == OsStr::new("..") {
            cursor = native_parent(root, path, resolution, &stack, cursor.as_raw_fd(), &raw)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn relative_components_preserve_raw_bytes_and_bound_active_depth() {
        let raw = Path::new(OsStr::from_bytes(b"dir-\xff///file-\xfe"));
        assert_eq!(
            relative_components(raw).unwrap(),
            VecDeque::from([
                OsString::from(OsStr::from_bytes(b"dir-\xff")),
                OsString::from(OsStr::from_bytes(b"file-\xfe"))
            ])
        );
        assert_eq!(
            relative_components(Path::new(".//dir///..//file")).unwrap(),
            VecDeque::from([".", "dir", "..", "file"].map(OsString::from))
        );
        for invalid in ["", "/file", "//file", "dir//file/", "dir//bad\0file"] {
            assert!(relative_components(Path::new(invalid)).is_err());
        }
        let bounded = vec!["d"; MAX_FOLLOW_STEPS].join("//");
        assert_eq!(
            relative_components(Path::new(&bounded)).unwrap().len(),
            MAX_FOLLOW_STEPS
        );
        assert!(relative_components(Path::new(&format!("{bounded}/d"))).is_err());
        assert!(relative_components(Path::new(&"d".repeat(MAX_TARGET + 1))).is_err());
    }
}
