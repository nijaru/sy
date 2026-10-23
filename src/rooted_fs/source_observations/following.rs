//! Forward relative FILE resolution through held no-follow directories. These
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

// Separators are not native components, but dots are: never use Path::components
// to erase them. LinkTarget independently authenticates the complete literal.
// Trailing separators require a DIR terminal, outside this FILE-only lane.
fn forward_components(target: &Path) -> io::Result<VecDeque<OsString>> {
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
            || (name != b"." && sibling_name(Path::new(OsStr::from_bytes(name))).is_none())
        {
            return Err(invalid());
        }
        components.push_back(OsStr::from_bytes(name).to_os_string());
    }
    Ok(components)
}

impl Observation {
    pub(super) fn validate_resolution(&self) -> io::Result<()> {
        // Original operator-selected following has its own authenticated tape.
        // It does not acquire descendant authority through an empty transcript.
        if self.resolution.is_empty() {
            return Ok(());
        }
        if !self.followed
            || self.named.kind != libc::S_IFLNK as u64
            || self.effective.kind != libc::S_IFREG as u64
            || self.resolution.len() > MAX_FOLLOW_STEPS
        {
            return Err(invalid());
        }
        let target = self.target.as_deref().ok_or_else(invalid)?;
        let mut pending = forward_components(target)?;
        let mut size = target.as_os_str().len();
        let mut links = 1;
        let mut parent_start = 0;
        for (index, step) in self.resolution.iter().enumerate() {
            if pending.pop_front().as_ref() != Some(&step.name) {
                return Err(invalid());
            }
            size = size
                .checked_add(step.name.len())
                .filter(|size| *size <= MAX_TARGET)
                .ok_or_else(invalid)?;
            match step.named.kind {
                kind if kind == libc::S_IFDIR as u64
                    && !pending.is_empty()
                    && step.target.is_none() =>
                {
                    if step.name != OsStr::new(".") {
                        parent_start = index + 1;
                    }
                }
                kind if kind == libc::S_IFLNK as u64 && pending.is_empty() => {
                    links += 1;
                    if links > MAX_FOLLOW_LINKS
                        || (parent_start == 0 && step.name == self.name)
                        || self.resolution[parent_start..index]
                            .iter()
                            .any(|previous| previous.named.same_binding(step.named))
                    {
                        return Err(invalid());
                    }
                    let target = step.target.as_deref().ok_or_else(invalid)?;
                    size = size
                        .checked_add(target.as_os_str().len())
                        .filter(|size| *size <= MAX_TARGET)
                        .ok_or_else(invalid)?;
                    pending = forward_components(target)?;
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
    pub(super) binding: TerminalBinding,
    pub(super) resolution: Vec<ResolutionStep>,
}

pub(super) fn inspect_forward_file(
    parent: RawFd,
    path: &Path,
    target: &Path,
) -> Result<FollowedFile> {
    let unsupported = || RootedFsError::SourceFollowUnsupported(path.to_path_buf());
    let mut pending = forward_components(target).map_err(|_| unsupported())?;
    let mut size = target.as_os_str().len();
    let mut links = 1;
    let mut resolution: Vec<ResolutionStep> = Vec::new();
    let mut cursor = operator::directory_at(parent, OsStr::new("."))?;
    let mut relative = path.parent().unwrap_or(Path::new("")).to_path_buf();
    let mut parent_start = 0;
    while let Some(name) = pending.pop_front() {
        if resolution.len() == MAX_FOLLOW_STEPS {
            return Err(unsupported());
        }
        size = size
            .checked_add(name.len())
            .filter(|size| *size <= MAX_TARGET)
            .ok_or_else(unsupported)?;
        if name != OsStr::new(".") {
            inventory::validate_original_path(&relative.join(&name))?;
        }
        let stat = stat_at_no_follow(cursor.as_raw_fd(), &name).map_err(|error| match error {
            RootedFsError::Io(error) if error.raw_os_error() == Some(libc::ENOENT) => unsupported(),
            error => error,
        })?;
        let named = NativeObservation::from_stat(&stat)?;
        let object_key = object_key(cursor.as_raw_fd(), Some(&name))
            .map_err(|error| source_read_race(error, path))?;
        let target = match stat.st_mode & libc::S_IFMT {
            libc::S_IFDIR if !pending.is_empty() => None,
            libc::S_IFREG if pending.is_empty() => None,
            libc::S_IFLNK if pending.is_empty() && links < MAX_FOLLOW_LINKS => {
                if (parent_start == 0 && path.file_name() == Some(name.as_os_str()))
                    || resolution[parent_start..]
                        .iter()
                        .any(|previous| previous.named.same_binding(named))
                {
                    return Err(unsupported());
                }
                let target = observed_target_at(cursor.as_raw_fd(), &name, path)?;
                size = size
                    .checked_add(target.as_os_str().len())
                    .filter(|size| *size <= MAX_TARGET)
                    .ok_or_else(unsupported)?;
                pending = forward_components(&target).map_err(|_| unsupported())?;
                links += 1;
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
        validate_step(cursor.as_raw_fd(), path, &step)?;
        if named.kind == libc::S_IFDIR as u64 {
            let child = open_recorded_directory(cursor.as_raw_fd(), path, &step)?;
            if step.name != OsStr::new(".") {
                relative.push(&step.name);
                parent_start = resolution.len() + 1;
            }
            cursor = child;
        }
        resolution.push(step);
        if named.kind == libc::S_IFREG as u64 {
            // Replay authentic original bindings and return THAT actual cursor.
            // Never discard the proof and reopen the terminal parent lexically.
            drop(cursor);
            let binding = replay_resolution(parent, path, &resolution)?;
            return Ok(FollowedFile {
                binding,
                resolution,
            });
        }
    }
    Err(unsupported())
}

fn validate_step(parent: RawFd, path: &Path, step: &ResolutionStep) -> Result<libc::stat> {
    let stat =
        stat_at_no_follow(parent, &step.name).map_err(|error| source_read_race(error, path))?;
    // A literal dot must name this actual held directory, not advance a
    // projected path or acquire another parent/root authority.
    if step.name == OsStr::new(".")
        && (step.named.kind != libc::S_IFDIR as u64
            || !step
                .named
                .matches(NativeObservation::from_stat(&stat_fd(parent)?)?, true)
            || object_key(parent, None)? != step.object_key)
    {
        return Err(RootedFsError::SourceBindingChanged(path.to_path_buf()));
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
    if !step.named.matches(
        NativeObservation::from_stat(&stat_fd(child.as_raw_fd())?)?,
        true,
    ) || object_key(child.as_raw_fd(), None)? != step.object_key
    {
        return Err(RootedFsError::SourceBindingChanged(path.to_path_buf()));
    }
    validate_step(parent, path, step)?;
    Ok(child)
}

fn replay_resolution(
    parent: RawFd,
    path: &Path,
    resolution: &[ResolutionStep],
) -> Result<TerminalBinding> {
    let mut cursor = operator::directory_at(parent, OsStr::new("."))?;
    for step in resolution {
        let stat = validate_step(cursor.as_raw_fd(), path, step)?;
        if step.named.kind == libc::S_IFDIR as u64 {
            cursor = open_recorded_directory(cursor.as_raw_fd(), path, step)?;
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
mod tests {
    use super::*;

    #[test]
    fn forward_components_preserve_raw_bytes_and_bound_active_depth() {
        let raw = Path::new(OsStr::from_bytes(b"dir-\xff///file-\xfe"));
        assert_eq!(
            forward_components(raw).unwrap(),
            VecDeque::from([
                OsString::from(OsStr::from_bytes(b"dir-\xff")),
                OsString::from(OsStr::from_bytes(b"file-\xfe")),
            ])
        );
        assert_eq!(
            forward_components(Path::new(".//dir///.//file")).unwrap(),
            VecDeque::from([".", "dir", ".", "file"].map(OsString::from))
        );
        for invalid in [
            "",
            "/file",
            "//file",
            "dir//file/",
            "dir//../file",
            "dir//bad\0file",
        ] {
            assert!(forward_components(Path::new(invalid)).is_err());
        }
        let bounded = vec!["d"; MAX_FOLLOW_STEPS].join("//");
        assert_eq!(
            forward_components(Path::new(&bounded)).unwrap().len(),
            MAX_FOLLOW_STEPS
        );
        assert!(forward_components(Path::new(&format!("{bounded}/d"))).is_err());
        assert!(forward_components(Path::new(&"d".repeat(MAX_TARGET + 1))).is_err());
    }
}
