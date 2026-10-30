//! Run-owned authenticated fixed-payload Merkle radix. The root and extent are native-owned RAM
//! authority, never recovered from scratch. A compressed 64-way radix needs at
//! most 43 branches per path, one FD, and storage proportional to distinct keys.
//! Updates overwrite authenticated ancestors: failure poisons the owner, keeps
//! the old root, and permits neither rollback nor adoption of partial writes.
//! This is runtime authentication, not a crash-durability/recovery format.

use std::fs::File;
use std::io;
#[cfg(any(not(unix), test))]
use std::io::{Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::unix::fs::FileExt;

const DIGIT_BITS: usize = 6;
const LEVELS: usize = 256usize.div_ceil(DIGIT_BITS);
const FANOUT: usize = 1 << DIGIT_BITS;
const BITMAP_BYTES: usize = FANOUT / 8;
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

// This view borrows only the current freshly authenticated and validated frame.
// Writers copy its children into their bounded proof; lookups select just one.
#[derive(Clone, Copy)]
struct BranchView<'a> {
    depth: u8,
    prefix: [u8; 32],
    slots: &'a [[u8; CHILD_BYTES]],
}

impl BranchView<'_> {
    fn child(self, position: usize) -> Option<Locator> {
        let slot = &self.slots[position];
        let mut offset = [0; 8];
        offset.copy_from_slice(&slot[..8]);
        let offset = u64::from_le_bytes(offset);
        if offset == EMPTY_OFFSET {
            return None;
        }
        let mut digest = [0; 32];
        digest.copy_from_slice(&slot[8..]);
        Some(Locator { offset, digest })
    }

    fn into_owned(self) -> Branch {
        Branch {
            depth: self.depth,
            prefix: self.prefix,
            children: std::array::from_fn(|position| self.child(position)),
        }
    }
}

#[derive(Clone, Copy)]
enum ReadNode<'a, const N: usize> {
    Branch(BranchView<'a>),
    Leaf { key: [u8; 32], state: [u8; N] },
}

impl<const N: usize> ReadNode<'_, N> {
    fn key(&self) -> [u8; 32] {
        match self {
            Self::Branch(branch) => branch.prefix,
            Self::Leaf { key, .. } => *key,
        }
    }

    fn into_owned(self) -> Node<N> {
        match self {
            Self::Branch(branch) => Node::Branch(branch.into_owned()),
            Self::Leaf { key, state } => Node::Leaf { key, state },
        }
    }
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
    fn key(&self) -> [u8; 32] {
        match self {
            Self::Branch(branch) => branch.prefix,
            Self::Leaf { key, .. } => *key,
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

fn digit(key: &[u8; 32], depth: usize) -> usize {
    let bit = depth * DIGIT_BITS;
    let byte = bit / 8;
    // The final digit has four key bits followed by two canonical zero bits.
    let next = if byte + 1 < key.len() {
        usize::from(key[byte + 1])
    } else {
        0
    };
    let pair = (usize::from(key[byte]) << 8) | next;
    (pair >> (16 - DIGIT_BITS - bit % 8)) & (FANOUT - 1)
}

fn prefix(key: [u8; 32], depth: usize) -> [u8; 32] {
    let mut result = key;
    let bits = (depth * DIGIT_BITS).min(key.len() * 8);
    if !bits.is_multiple_of(8) {
        result[bits / 8] &= 0xff << (8 - bits % 8);
    }
    result[bits.div_ceil(8)..].fill(0);
    result
}

fn digest(bytes: &[u8]) -> io::Result<[u8; 32]> {
    const DOMAIN: &[u8] = b"sy/authenticated-index/v1\0";
    // Avoid incremental hashing state by assembling the bounded transcript once.
    let mut transcript = [0; DOMAIN.len() + BRANCH_BYTES + BITMAP_BYTES];
    transcript[..DOMAIN.len()].copy_from_slice(DOMAIN);
    let length = if bytes[0] == 0 {
        // Empty slots have exactly one valid encoding. Authenticate their
        // positions with a bitmap instead of hashing their constant bytes.
        // No child offset is used until the caller verifies this digest.
        let compact = &mut transcript[DOMAIN.len()..];
        compact[..BRANCH_HEADER_BYTES].copy_from_slice(&bytes[..BRANCH_HEADER_BYTES]);
        let mut occupied = 0u64;
        let mut end = BRANCH_HEADER_BYTES + BITMAP_BYTES;
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
        compact[BRANCH_HEADER_BYTES..BRANCH_HEADER_BYTES + BITMAP_BYTES]
            .copy_from_slice(&occupied.to_le_bytes()[..BITMAP_BYTES]);
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

    fn read<'a>(
        &mut self,
        locator: Locator,
        bytes: &'a mut [u8; BRANCH_BYTES],
    ) -> io::Result<ReadNode<'a, N>> {
        self.check_offset(locator.offset, 1)?;
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
            return Ok(ReadNode::Leaf { key, state });
        }
        let depth = bytes[1];
        let mut branch = BranchView {
            depth,
            prefix: [0; 32],
            slots: bytes[BRANCH_HEADER_BYTES..].as_chunks::<CHILD_BYTES>().0,
        };
        branch
            .prefix
            .copy_from_slice(&bytes[2..BRANCH_HEADER_BYTES]);
        if usize::from(depth) >= LEVELS
            || prefix(branch.prefix, usize::from(depth)) != branch.prefix
        {
            return Err(invalid(StateError::Path));
        }
        let mut count = 0;
        for (position, slot) in branch.slots.iter().enumerate() {
            let mut offset = [0; 8];
            offset.copy_from_slice(&slot[..8]);
            let offset = u64::from_le_bytes(offset);
            if offset != EMPTY_OFFSET {
                self.check_offset(offset, Self::RECORD_BYTES.min(BRANCH_BYTES))?;
                let padding = LEVELS * DIGIT_BITS - 256;
                if offset == locator.offset
                    || (usize::from(depth) == LEVELS - 1 && position & ((1 << padding) - 1) != 0)
                {
                    return Err(invalid(StateError::Child));
                }
                count += 1;
            }
        }
        if count < 2 {
            return Err(invalid(StateError::Child));
        }
        Ok(ReadNode::Branch(branch))
    }

    /// Authenticate every edge, retaining only the previous routing boundary.
    /// Reads need no update path; writers collect the same authenticated steps.
    fn walk(
        &mut self,
        key: [u8; 32],
        mut visit: impl FnMut(Locator, BranchView<'_>),
    ) -> io::Result<Option<(Locator, Node<N>)>> {
        let mut parent: Option<(u8, [u8; 32])> = None;
        let mut steps = 0;
        let mut cursor = self.root.as_ref().map(|root| root.locator);
        let mut bytes = [0; BRANCH_BYTES];
        while let Some(locator) = cursor {
            let node = self.read(locator, &mut bytes)?;
            if let Some((depth, expected_prefix)) = parent {
                if prefix(node.key(), usize::from(depth)) != expected_prefix
                    || digit(&node.key(), usize::from(depth)) != digit(&key, usize::from(depth))
                    || matches!(node, ReadNode::Branch(branch) if branch.depth <= depth)
                {
                    return Err(invalid(StateError::Path));
                }
            }
            match node {
                ReadNode::Branch(branch)
                    if prefix(key, usize::from(branch.depth)) == branch.prefix =>
                {
                    if steps == LEVELS {
                        return Err(invalid(StateError::Path));
                    }
                    steps += 1;
                    visit(locator, branch);
                    parent = Some((branch.depth, branch.prefix));
                    cursor = branch.child(digit(&key, usize::from(branch.depth)));
                }
                _ => return Ok(Some((locator, node.into_owned()))),
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
        proof.terminal = self.walk(key, |locator, branch| {
            proof.path[proof.len] = Some(Step {
                locator,
                branch: branch.into_owned(),
            });
            proof.len += 1;
        })?;
        Ok(proof)
    }

    pub(crate) fn get_bytes(&mut self, key: [u8; 32]) -> io::Result<Option<[u8; N]>> {
        self.begin()?;
        let terminal = self.walk(key, |_, _| {})?;
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
        #[cfg(unix)]
        self.file.write_all_at(&bytes[..length], offset)?;
        #[cfg(not(unix))]
        {
            self.file.seek(SeekFrom::Start(offset))?;
            self.file.write_all(&bytes[..length])?;
        }
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
                        .find(|&d| digit(&key, d) != digit(&node.key(), d))
                        .ok_or_else(|| invalid(StateError::Path))?;
                    let mut branch = Branch {
                        depth: depth as u8,
                        prefix: prefix(key, depth),
                        children: [None; FANOUT],
                    };
                    branch.children[digit(&key, depth)] = Some(new_leaf.locator);
                    branch.children[digit(&node.key(), depth)] = Some(old);
                    let offset = end;
                    self.write(offset, Node::Branch(branch), &mut end)?
                } else {
                    new_leaf
                }
            }
        };
        for step in proof.path[..proof.len].iter().rev() {
            let mut step = step.ok_or_else(|| invalid(StateError::Path))?;
            step.branch.children[digit(&key, usize::from(step.branch.depth))] =
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
