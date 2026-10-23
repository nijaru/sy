//! Bounded literal resolution grammar. A ROOT reset is explicit provenance,
//! never a synthetic physical member or a virtual-root path rewrite.
use super::*;

impl ResolutionStep {
    pub(in crate::rooted_fs::source_observations) fn encode(&self, bytes: &mut Vec<u8>) {
        let (tag, name, named, object_key) = match self {
            Self::Entry(entry) => (0, &entry.name, entry.named, entry.object_key),
            Self::Root(root) => (1, &root.address.0, root.named, Some(root.object_key)),
        };
        bytes.push(tag);
        bytes.extend_from_slice(&(name.len() as u32).to_le_bytes());
        bytes.extend_from_slice(name.as_bytes());
        named.encode(bytes);
        bytes.extend_from_slice(&object_key.map_or([0; 32], |key| key.0));
        if let Self::Entry(entry) = self {
            if let Some(target) = &entry.target {
                let target = target.as_os_str().as_bytes();
                bytes.extend_from_slice(&(target.len() as u32).to_le_bytes());
                bytes.extend_from_slice(target);
            } else {
                bytes.extend_from_slice(&u32::MAX.to_le_bytes());
            }
        }
    }

    pub(in crate::rooted_fs::source_observations) fn decode(bytes: &mut &[u8]) -> io::Result<Self> {
        let [tag] = take(bytes)?;
        if tag > 1 {
            return Err(invalid());
        }
        let length = u32::from_le_bytes(take(bytes)?) as usize;
        if length > MAX_TARGET {
            return Err(invalid());
        }
        let name = OsStr::from_bytes(field(bytes, length)?).to_os_string();
        let named = NativeObservation::decode(bytes)?;
        let object = take::<32>(bytes)?;
        let object_key = (object != [0; 32]).then_some(NativeObjectKey(object));
        if tag == 1 {
            if named.kind != libc::S_IFDIR as u64 || !name.as_bytes().starts_with(b"/") {
                return Err(invalid());
            }
            components(name.as_bytes())?;
            return Ok(Self::Root(RootReset {
                address: LinkTarget(name),
                named,
                object_key: object_key.ok_or_else(invalid)?,
            }));
        }
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
        Ok(Self::Entry(ResolutionEntry {
            name,
            named,
            object_key,
            target,
        }))
    }

    pub(in crate::rooted_fs::source_observations) fn entry(&self) -> io::Result<&ResolutionEntry> {
        match self {
            Self::Entry(entry) => Ok(entry),
            Self::Root(_) => Err(invalid()),
        }
    }

    #[cfg(test)]
    pub(in crate::rooted_fs::source_observations) fn entry_mut(
        &mut self,
    ) -> io::Result<&mut ResolutionEntry> {
        match self {
            Self::Entry(entry) => Ok(entry),
            Self::Root(_) => Err(invalid()),
        }
    }

    pub(super) fn matches(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Entry(a), Self::Entry(b)) => {
                a.name == b.name
                    && a.named.matches(b.named, true)
                    && a.object_key == b.object_key
                    && a.target == b.target
            }
            (Self::Root(a), Self::Root(b)) => a.matches(b),
            _ => false,
        }
    }
}

impl RootReset {
    pub(super) fn matches(&self, other: &Self) -> bool {
        self.address == other.address
            && self.named.matches(other.named, true)
            && self.object_key == other.object_key
    }
}

pub(super) struct Target {
    pub(super) absolute: bool,
    pub(super) components: VecDeque<OsString>,
}

// Separator runs are not components; explicit dots and parents are. Authenticate
// LinkTarget separately, never through Path's normalized equality.
fn components(bytes: &[u8]) -> io::Result<VecDeque<OsString>> {
    if bytes.len() > MAX_TARGET {
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

pub(super) fn parse_target(target: &Path) -> io::Result<Target> {
    let bytes = target.as_os_str().as_bytes();
    if bytes.is_empty() || bytes.ends_with(b"/") {
        return Err(invalid());
    }
    Ok(Target {
        absolute: bytes.starts_with(b"/"),
        components: components(bytes)?,
    })
}

/// Select only the original ROOT-address candidate. Component matching is not
/// authority: the caller MUST natively qualify that address before using suffix.
pub(super) fn absolute_suffix(address: &Path, target: &Path) -> io::Result<VecDeque<OsString>> {
    let mut target = parse_target(target)?;
    if !address.as_os_str().as_bytes().starts_with(b"/") || !target.absolute {
        return Err(invalid());
    }
    for name in components(address.as_os_str().as_bytes())? {
        if target.components.pop_front().as_ref() != Some(&name) {
            return Err(invalid());
        }
    }
    if target.components.is_empty() {
        return Err(invalid());
    }
    Ok(target.components)
}

impl Observation {
    pub(in crate::rooted_fs::source_observations) fn validate_resolution(&self) -> io::Result<()> {
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
        let mut target = self.target.as_deref().ok_or_else(invalid)?;
        let parsed = parse_target(target)?;
        let mut absolute = parsed.absolute;
        let mut pending = parsed.components;
        let mut size = target.as_os_str().len();
        let mut parent = [0; 32];
        let mut stack = Vec::new();
        for (index, step) in self.resolution[..self.resolution_start].iter().enumerate() {
            let step = step.entry()?;
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
            if absolute {
                let ResolutionStep::Root(root) = step else {
                    return Err(invalid());
                };
                if root.named.kind != libc::S_IFDIR as u64 || root.object_key.0 == [0; 32] {
                    return Err(invalid());
                }
                pending = absolute_suffix(&root.address, target)?;
                add_bytes(&mut size, root.address.as_os_str().len())?;
                parent = [0; 32];
                stack.clear();
                absolute = false;
                continue;
            }
            let step = step.entry()?;
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
                    target = step.target.as_deref().ok_or_else(invalid)?;
                    add_bytes(&mut size, target.as_os_str().len())?;
                    let parsed = parse_target(target)?;
                    absolute = parsed.absolute;
                    pending = parsed.components;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn components_preserve_literals_and_bound_active_depth() {
        let raw = Path::new(OsStr::from_bytes(b"dir-\xff///file-\xfe"));
        assert_eq!(
            parse_target(raw).unwrap().components,
            VecDeque::from([
                OsString::from(OsStr::from_bytes(b"dir-\xff")),
                OsString::from(OsStr::from_bytes(b"file-\xfe")),
            ])
        );
        assert_eq!(
            parse_target(Path::new(".//dir///..//file"))
                .unwrap()
                .components,
            VecDeque::from([".", "dir", "..", "file"].map(OsString::from))
        );
        for invalid in ["", "dir//file/", "dir//bad\0file"] {
            assert!(parse_target(Path::new(invalid)).is_err());
        }
        let bounded = vec!["d"; MAX_FOLLOW_STEPS].join("//");
        assert_eq!(
            parse_target(Path::new(&bounded)).unwrap().components.len(),
            MAX_FOLLOW_STEPS
        );
        assert!(parse_target(Path::new(&format!("{bounded}/d"))).is_err());
        assert!(parse_target(Path::new(&"d".repeat(MAX_TARGET + 1))).is_err());
    }

    #[test]
    fn absolute_candidates_preserve_native_dots_and_component_boundaries() {
        assert_eq!(
            absolute_suffix(Path::new("/root//."), Path::new("//root/.///file")).unwrap(),
            VecDeque::from([OsString::from("file")])
        );
        assert_eq!(
            absolute_suffix(Path::new("/"), Path::new("/file")).unwrap(),
            VecDeque::from([OsString::from("file")])
        );
        for (root, target) in [
            ("/root", "/root-other/file"),
            ("/root", "/else/root/file"),
            ("/root/.", "/root/file"),
            ("/root", "/root"),
            ("root", "/root/file"),
        ] {
            assert!(absolute_suffix(Path::new(root), Path::new(target)).is_err());
        }
    }
}
