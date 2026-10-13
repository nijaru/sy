//! Run-owned authenticated fixed-payload Merkle radix. The root and extent are native-owned RAM
//! authority, never recovered from scratch. A compressed 16-way radix needs at
//! most 64 branches per path, one FD, and storage proportional to distinct keys.
//! Updates overwrite authenticated ancestors: failure poisons the owner, keeps
//! the old root, and permits neither rollback nor adoption of partial writes.
//! This is runtime authentication, not a crash-durability/recovery format.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};

const LEVELS: usize = 64;
const FANOUT: usize = 16;
const BRANCH_BYTES: usize = 2 + 32 + FANOUT * 40;
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
    root: Option<Locator>,
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

fn digest(bytes: &[u8]) -> [u8; 32] {
    let mut hash = blake3::Hasher::new();
    hash.update(b"sy/authenticated-index/v1\0");
    hash.update(bytes);
    *hash.finalize().as_bytes()
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
        self.file.seek(SeekFrom::Start(locator.offset))?;
        let mut bytes = [0; BRANCH_BYTES];
        self.file.read_exact(&mut bytes[..1])?;
        let length = match bytes[0] {
            0 => BRANCH_BYTES,
            1 => Self::RECORD_BYTES,
            tag => return Err(invalid(StateError::Tag(tag))),
        };
        self.check_offset(locator.offset, length)?;
        self.file.read_exact(&mut bytes[1..length])?;
        // Only the bounded framing tag/length is inspected before authentication.
        // Both mapping and payload must prove ancestry to the retained RAM root.
        if digest(&bytes[..length]) != locator.digest {
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
        branch.prefix.copy_from_slice(&bytes[2..34]);
        if usize::from(depth) >= LEVELS
            || prefix(branch.prefix, usize::from(depth)) != branch.prefix
        {
            return Err(invalid(StateError::Path));
        }
        for (child, slot) in branch
            .children
            .iter_mut()
            .zip(bytes[34..].as_chunks::<40>().0)
        {
            let mut offset = [0; 8];
            offset.copy_from_slice(&slot[..8]);
            let offset = u64::from_le_bytes(offset);
            let mut digest = [0; 32];
            digest.copy_from_slice(&slot[8..]);
            if offset == EMPTY_OFFSET {
                if digest != [0; 32] {
                    return Err(invalid(StateError::Child));
                }
            } else {
                self.check_offset(offset, Self::RECORD_BYTES.min(BRANCH_BYTES))?;
                if offset == locator.offset {
                    return Err(invalid(StateError::Child));
                }
                *child = Some(Locator { offset, digest });
            }
        }
        if branch.children.iter().flatten().count() < 2 {
            return Err(invalid(StateError::Child));
        }
        Ok(Node::Branch(branch))
    }

    fn proof(&mut self, key: [u8; 32]) -> io::Result<Proof<N>> {
        let mut proof = Proof {
            path: [None; LEVELS],
            len: 0,
            terminal: None,
        };
        let mut cursor = self.root;
        while let Some(locator) = cursor {
            let node = self.read(locator)?;
            if proof.len > 0 {
                let parent = proof.path[proof.len - 1]
                    .ok_or_else(|| invalid(StateError::Path))?
                    .branch;
                let depth = usize::from(parent.depth);
                if prefix(node.key(), depth) != parent.prefix
                    || nibble(&node.key(), depth) != nibble(&key, depth)
                    || matches!(node, Node::Branch(branch) if branch.depth <= parent.depth)
                {
                    return Err(invalid(StateError::Path));
                }
            }
            match node {
                Node::Branch(branch) if prefix(key, usize::from(branch.depth)) == branch.prefix => {
                    if proof.len == LEVELS {
                        return Err(invalid(StateError::Path));
                    }
                    proof.path[proof.len] = Some(Step { locator, branch });
                    proof.len += 1;
                    cursor = branch.children[nibble(&key, usize::from(branch.depth))];
                }
                _ => {
                    proof.terminal = Some((locator, node));
                    break;
                }
            }
        }
        Ok(proof)
    }

    pub(crate) fn get_bytes(&mut self, key: [u8; 32]) -> io::Result<Option<[u8; N]>> {
        self.begin()?;
        let proof = self.proof(key)?;
        let state = match proof.terminal {
            Some((_, Node::Leaf { key: found, state })) if key == found => Some(state),
            _ => None,
        };
        self.healthy = true;
        Ok(state)
    }

    // Hash only bytes constructed by this owner. Never reread scratch to mint
    // a new locator/digest, and never infer the append offset from its file size.
    fn write(&mut self, offset: u64, node: Node<N>, end: &mut u64) -> io::Result<Locator> {
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
                bytes[2..34].copy_from_slice(&branch.prefix);
                for (child, slot) in branch
                    .children
                    .iter()
                    .zip(bytes[34..].as_chunks_mut::<40>().0)
                {
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
            digest: digest(&bytes[..length]),
        };
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(&bytes[..length])?;
        *end = (*end).max(next);
        Ok(locator)
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
                    branch.children[nibble(&key, depth)] = Some(new_leaf);
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
            step.branch.children[nibble(&key, usize::from(step.branch.depth))] = Some(updated);
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
