//! Run-owned authenticated fixed-payload Merkle radix. The root and extent are native-owned RAM
//! authority, never recovered from scratch. A compressed 16-way radix needs at
//! most 64 branches per path, one FD, and storage proportional to distinct keys.
//! Updates overwrite authenticated ancestors: failure poisons the owner, keeps
//! the old root, and permits neither rollback nor adoption of partial writes.
//! This is runtime authentication, not a crash-durability/recovery format.

use std::fs::File;
#[cfg(any(not(unix), test))]
use std::io::Read;
use std::io::{self, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::unix::fs::FileExt;

const LEVELS: usize = 64;
const FANOUT: usize = 16;
const BRANCH_HEADER_BYTES: usize = 2 + 32;
const CHILD_BYTES: usize = 8 + 32;
const BRANCH_BYTES: usize = BRANCH_HEADER_BYTES + FANOUT * CHILD_BYTES;
const EMPTY_OFFSET: u64 = u64::MAX;

#[derive(Debug, thiserror::Error)]
enum StateError {
    #[error("authenticated index is terminal")]
    Terminal,
    #[error("authenticated index scratch extent changed")]
    Extent,
    #[error("authenticated index offset outside native-owned extent")]
    Offset,
    #[error("invalid authenticated index node tag: {0}")]
    Tag(u8),
    #[error("authenticated index authentication failed")]
    Authentication,
    #[error("invalid authenticated index radix depth/prefix")]
    Path,
    #[error("invalid authenticated index child locator")]
    Child,
}

fn invalid(error: StateError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Locator {
    offset: u64,
    digest: [u8; 32],
}

// Only the native writer constructs this image. Retaining the current root's
// exact preimage enlarges the RAM authority; it never retains a scratch proof.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WrittenNode {
    locator: Locator,
    bytes: [u8; BRANCH_BYTES],
}

#[derive(Clone, Copy)]
struct Branch {
    depth: u8,
    prefix: [u8; 32],
    children: [Option<Locator>; FANOUT],
}

#[derive(Clone, Copy)]
#[expect(
    clippy::large_enum_variant,
    reason = "fixed-size proof nodes deliberately avoid heap allocation during lookup/update"
)]
enum Node<const N: usize> {
    Branch(Branch),
    Leaf { key: [u8; 32], state: [u8; N] },
}

impl<const N: usize> Node<N> {
    fn key(self) -> [u8; 32] {
        match self {
            Self::Branch(branch) => branch.prefix,
            Self::Leaf { key, .. } => key,
        }
    }
}

#[derive(Clone, Copy)]
struct Step {
    locator: Locator,
    branch: Branch,
}

struct Proof<const N: usize> {
    path: [Option<Step>; LEVELS],
    len: usize,
    terminal: Option<(Locator, Node<N>)>,
}

pub(crate) struct AuthenticatedIndex<const N: usize> {
    // Native owners retain this descriptor; fault tests mutate the same held file.
    pub(crate) file: File,
    root: Option<WrittenNode>,
    end: u64,
    healthy: bool,
}

fn nibble(key: &[u8; 32], depth: usize) -> usize {
    usize::from((key[depth / 2] >> (if depth.is_multiple_of(2) { 4 } else { 0 })) & 15)
}

fn prefix(key: [u8; 32], depth: usize) -> [u8; 32] {
    let mut result = key;
    if !depth.is_multiple_of(2) {
        result[depth / 2] &= 0xf0;
    }
    result[depth.div_ceil(2)..].fill(0);
    result
}

fn digest(bytes: &[u8]) -> io::Result<[u8; 32]> {
    const DOMAIN: &[u8] = b"sy/authenticated-index/v1\0";
    // Every frame transcript fits in one BLAKE3 chunk; avoid incremental
    // hashing state by assembling the same bounded transcript once.
    let mut transcript = [0; DOMAIN.len() + BRANCH_BYTES + 2];
    transcript[..DOMAIN.len()].copy_from_slice(DOMAIN);
    let length = if bytes[0] == 0 {
        // Empty slots have exactly one valid encoding. Authenticate their
        // positions with a bitmap instead of hashing their constant bytes.
        // No child offset is used until the caller verifies this digest.
        let compact = &mut transcript[DOMAIN.len()..];
        compact[..BRANCH_HEADER_BYTES].copy_from_slice(&bytes[..BRANCH_HEADER_BYTES]);
        let mut occupied = 0u16;
        let mut end = BRANCH_HEADER_BYTES + 2;
        for (position, slot) in bytes[BRANCH_HEADER_BYTES..]
            .as_chunks::<CHILD_BYTES>()
            .0
            .iter()
            .enumerate()
        {
            if slot[..8] == EMPTY_OFFSET.to_le_bytes() {
                if slot[8..] != [0; 32] {
                    return Err(invalid(StateError::Child));
                }
            } else {
                occupied |= 1 << position;
                compact[end..end + CHILD_BYTES].copy_from_slice(slot);
                end += CHILD_BYTES;
            }
        }
        compact[BRANCH_HEADER_BYTES..BRANCH_HEADER_BYTES + 2]
            .copy_from_slice(&occupied.to_le_bytes());
        DOMAIN.len() + end
    } else {
        let end = DOMAIN.len() + bytes.len();
        transcript[DOMAIN.len()..end].copy_from_slice(bytes);
        end
    };
    Ok(*blake3::hash(&transcript[..length]).as_bytes())
}

impl<const N: usize> AuthenticatedIndex<N> {
    const RECORD_BYTES: usize = 33 + N;

    pub(crate) fn new() -> io::Result<Self> {
        assert!(Self::RECORD_BYTES <= BRANCH_BYTES);
        Ok(Self {
            file: tempfile::tempfile()?,
            root: None,
            end: 0,
            healthy: true,
        })
    }

    fn begin(&mut self) -> io::Result<()> {
        if !self.healthy {
            return Err(invalid(StateError::Terminal));
        }
        self.healthy = false;
        if self.file.metadata()?.len() != self.end {
            return Err(invalid(StateError::Extent));
        }
        Ok(())
    }

    fn check_offset(&self, offset: u64, length: usize) -> io::Result<()> {
        if offset
            .checked_add(length as u64)
            .is_none_or(|end| end > self.end)
        {
            return Err(invalid(StateError::Offset));
        }
        Ok(())
    }

    fn read(&mut self, locator: Locator) -> io::Result<Node<N>> {
        self.check_offset(locator.offset, 1)?;
        let mut bytes = [0; BRANCH_BYTES];
        // Bound the read by the native-owned extent, not an untrusted tag. A
        // packed leaf may include bytes from its neighbour in this one read;
        // only its own authenticated frame is subsequently interpreted.
        let available = (self.end - locator.offset).min(BRANCH_BYTES as u64);
        let read_length = usize::try_from(available).map_err(|_| invalid(StateError::Offset))?;
        #[cfg(unix)]
        self.file
            .read_exact_at(&mut bytes[..read_length], locator.offset)?;
        #[cfg(not(unix))]
        {
            self.file.seek(SeekFrom::Start(locator.offset))?;
            self.file.read_exact(&mut bytes[..read_length])?;
        }
        let length = match bytes[0] {
            0 => BRANCH_BYTES,
            1 => Self::RECORD_BYTES,
            tag => return Err(invalid(StateError::Tag(tag))),
        };
        self.check_offset(locator.offset, length)?;
        // Read the root freshly too, but authenticate it against its exact
        // native-written preimage. The writer already checked canonical slots.
        // Other nodes must prove their digest before any routing/offset use.
        let authenticated = match self.root.as_ref() {
            Some(root) if root.locator == locator => root.bytes[..length] == bytes[..length],
            _ => digest(&bytes[..length])? == locator.digest,
        };
        if !authenticated {
            return Err(invalid(StateError::Authentication));
        }
        if bytes[0] == 1 {
            let mut key = [0; 32];
            key.copy_from_slice(&bytes[1..33]);
            let mut state = [0; N];
            state.copy_from_slice(&bytes[33..length]);
            return Ok(Node::Leaf { key, state });
        }
        let depth = bytes[1];
        let mut branch = Branch {
            depth,
            prefix: [0; 32],
            children: [None; FANOUT],
        };
        branch
            .prefix
            .copy_from_slice(&bytes[2..BRANCH_HEADER_BYTES]);
        if usize::from(depth) >= LEVELS
            || prefix(branch.prefix, usize::from(depth)) != branch.prefix
        {
            return Err(invalid(StateError::Path));
        }
        for (child, slot) in branch
            .children
            .iter_mut()
            .zip(bytes[BRANCH_HEADER_BYTES..].as_chunks::<CHILD_BYTES>().0)
        {
            let mut offset = [0; 8];
            offset.copy_from_slice(&slot[..8]);
            let offset = u64::from_le_bytes(offset);
            if offset != EMPTY_OFFSET {
                self.check_offset(offset, Self::RECORD_BYTES.min(BRANCH_BYTES))?;
                if offset == locator.offset {
                    return Err(invalid(StateError::Child));
                }
                let mut digest = [0; 32];
                digest.copy_from_slice(&slot[8..]);
                *child = Some(Locator { offset, digest });
            }
        }
        if branch.children.iter().flatten().count() < 2 {
            return Err(invalid(StateError::Child));
        }
        Ok(Node::Branch(branch))
    }

    /// Authenticate every edge, retaining only the previous routing boundary.
    /// Reads need no update path; writers collect the same authenticated steps.
    fn walk(
        &mut self,
        key: [u8; 32],
        mut visit: impl FnMut(Step),
    ) -> io::Result<Option<(Locator, Node<N>)>> {
        let mut parent: Option<(u8, [u8; 32])> = None;
        let mut steps = 0;
        let mut cursor = self.root.as_ref().map(|root| root.locator);
        while let Some(locator) = cursor {
            let node = self.read(locator)?;
            if let Some((depth, expected_prefix)) = parent {
                if prefix(node.key(), usize::from(depth)) != expected_prefix
                    || nibble(&node.key(), usize::from(depth)) != nibble(&key, usize::from(depth))
                    || matches!(node, Node::Branch(branch) if branch.depth <= depth)
                {
                    return Err(invalid(StateError::Path));
                }
            }
            match node {
                Node::Branch(branch) if prefix(key, usize::from(branch.depth)) == branch.prefix => {
                    if steps == LEVELS {
                        return Err(invalid(StateError::Path));
                    }
                    steps += 1;
                    visit(Step { locator, branch });
                    parent = Some((branch.depth, branch.prefix));
                    cursor = branch.children[nibble(&key, usize::from(branch.depth))];
                }
                _ => return Ok(Some((locator, node))),
            }
        }
        Ok(None)
    }

    fn proof(&mut self, key: [u8; 32]) -> io::Result<Proof<N>> {
        let mut proof = Proof {
            path: [None; LEVELS],
            len: 0,
            terminal: None,
        };
        proof.terminal = self.walk(key, |step| {
            proof.path[proof.len] = Some(step);
            proof.len += 1;
        })?;
        Ok(proof)
    }

    pub(crate) fn get_bytes(&mut self, key: [u8; 32]) -> io::Result<Option<[u8; N]>> {
        self.begin()?;
        let terminal = self.walk(key, |_| {})?;
        let state = match terminal {
            Some((_, Node::Leaf { key: found, state })) if key == found => Some(state),
            _ => None,
        };
        self.healthy = true;
        Ok(state)
    }

    // Hash only bytes constructed by this owner. Never reread scratch to mint
    // a new locator/digest, and never infer the append offset from its file size.
    fn write(&mut self, offset: u64, node: Node<N>, end: &mut u64) -> io::Result<WrittenNode> {
        let mut bytes = [0; BRANCH_BYTES];
        let length = match node {
            Node::Leaf { key, state } => {
                bytes[0] = 1;
                bytes[1..33].copy_from_slice(&key);
                bytes[33..Self::RECORD_BYTES].copy_from_slice(&state);
                Self::RECORD_BYTES
            }
            Node::Branch(branch) => {
                bytes[1] = branch.depth;
                bytes[2..BRANCH_HEADER_BYTES].copy_from_slice(&branch.prefix);
                for (child, slot) in branch.children.iter().zip(
                    bytes[BRANCH_HEADER_BYTES..]
                        .as_chunks_mut::<CHILD_BYTES>()
                        .0,
                ) {
                    slot[..8]
                        .copy_from_slice(&child.map_or(EMPTY_OFFSET, |c| c.offset).to_le_bytes());
                    if let Some(child) = child {
                        slot[8..].copy_from_slice(&child.digest);
                    }
                }
                BRANCH_BYTES
            }
        };
        let next = offset
            .checked_add(length as u64)
            .filter(|next| offset == *end || *next <= *end)
            .ok_or_else(|| invalid(StateError::Offset))?;
        let locator = Locator {
            offset,
            digest: digest(&bytes[..length])?,
        };
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(&bytes[..length])?;
        *end = (*end).max(next);
        Ok(WrittenNode { locator, bytes })
    }

    pub(crate) fn set_bytes(&mut self, key: [u8; 32], state: [u8; N]) -> io::Result<()> {
        self.begin()?;
        let proof = self.proof(key)?;
        let mut end = self.end;
        let leaf = Node::Leaf { key, state };
        let mut updated = match proof.terminal {
            Some((locator, Node::Leaf { key: found, .. })) if found == key => {
                self.write(locator.offset, leaf, &mut end)?
            }
            terminal => {
                let offset = end;
                let new_leaf = self.write(offset, leaf, &mut end)?;
                if let Some((old, node)) = terminal {
                    let depth = (0..LEVELS)
                        .find(|&d| nibble(&key, d) != nibble(&node.key(), d))
                        .ok_or_else(|| invalid(StateError::Path))?;
                    let mut branch = Branch {
                        depth: depth as u8,
                        prefix: prefix(key, depth),
                        children: [None; FANOUT],
                    };
                    branch.children[nibble(&key, depth)] = Some(new_leaf.locator);
                    branch.children[nibble(&node.key(), depth)] = Some(old);
                    let offset = end;
                    self.write(offset, Node::Branch(branch), &mut end)?
                } else {
                    new_leaf
                }
            }
        };
        for step in proof.path[..proof.len].iter().rev() {
            let mut step = step.ok_or_else(|| invalid(StateError::Path))?;
            step.branch.children[nibble(&key, usize::from(step.branch.depth))] =
                Some(updated.locator);
            updated = self.write(step.locator.offset, Node::Branch(step.branch), &mut end)?;
        }
        // Publication follows the COMPLETE native-owned write path, including
        // every ancestor. Partial failures leave healthy=false and the old root.
        self.root = Some(updated);
        self.end = end;
        self.healthy = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
