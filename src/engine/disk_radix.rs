//! Exact fixed-depth disk index shared by hardlink planning and execution.
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};

const BRANCH_BYTES: usize = 19;

pub(super) struct DiskRadix {
    file: File,
    root: Option<u64>,
    healthy: bool,
}

enum Node {
    Branch { bit: u16, children: [u64; 2] },
    Leaf { key: [u8; 32], value: u64 },
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn side(key: &[u8; 32], bit: u16) -> usize {
    usize::from((key[usize::from(bit / 8)] >> (7 - bit % 8)) & 1)
}

impl DiskRadix {
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            file: tempfile::tempfile()?,
            root: None,
            healthy: true,
        })
    }

    fn node(&mut self, offset: u64) -> io::Result<Node> {
        self.file.seek(SeekFrom::Start(offset))?;
        let mut tag = [0];
        self.file.read_exact(&mut tag)?;
        match tag[0] {
            0 => {
                let mut bytes = [0; BRANCH_BYTES - 1];
                self.file.read_exact(&mut bytes)?;
                let bit = u16::from_le_bytes([bytes[0], bytes[1]]);
                if bit >= 256 {
                    return Err(invalid("invalid radix branch bit"));
                }
                let mut children = [0; 2];
                for (child, bytes) in children.iter_mut().zip(bytes[2..].as_chunks::<8>().0) {
                    *child = u64::from_le_bytes(*bytes);
                }
                Ok(Node::Branch { bit, children })
            }
            1 => {
                let mut key = [0; 32];
                let mut value = [0; 8];
                self.file.read_exact(&mut key)?;
                self.file.read_exact(&mut value)?;
                Ok(Node::Leaf {
                    key,
                    value: u64::from_le_bytes(value),
                })
            }
            _ => Err(invalid("invalid radix node tag")),
        }
    }

    pub fn get(&mut self, key: [u8; 32]) -> io::Result<Option<u64>> {
        if !self.healthy {
            return Err(invalid("radix scratch index is incomplete"));
        }
        let Some(mut cursor) = self.root else {
            return Ok(None);
        };
        let mut previous_bit = None;
        loop {
            match self.node(cursor)? {
                Node::Branch { bit, children } => {
                    if previous_bit.is_some_and(|previous| bit <= previous) {
                        return Err(invalid("unordered radix branch bits"));
                    }
                    previous_bit = Some(bit);
                    cursor = children[side(&key, bit)];
                }
                Node::Leaf { key: found, value } => return Ok((found == key).then_some(value)),
            }
        }
    }

    pub fn insert(&mut self, key: [u8; 32], value: u64) -> io::Result<()> {
        if self.get(key)?.is_some() {
            return Err(invalid("radix key already present"));
        }
        self.healthy = false;
        let leaf = self.file.seek(SeekFrom::End(0))?;
        self.file.write_all(&[1])?;
        self.file.write_all(&key)?;
        self.file.write_all(&value.to_le_bytes())?;
        if let Some(root) = self.root {
            let mut cursor = root;
            let found = loop {
                match self.node(cursor)? {
                    Node::Branch { bit, children } => cursor = children[side(&key, bit)],
                    Node::Leaf { key, .. } => break key,
                }
            };
            let bit = key
                .iter()
                .zip(found)
                .enumerate()
                .find_map(|(index, (a, b))| {
                    let difference = a ^ b;
                    (difference != 0)
                        .then(|| (index * 8 + difference.leading_zeros() as usize) as u16)
                })
                .ok_or_else(|| invalid("duplicate radix key"))?;
            cursor = root;
            let mut parent_slot = None;
            loop {
                match self.node(cursor)? {
                    Node::Branch {
                        bit: branch_bit,
                        children,
                    } if branch_bit < bit => {
                        let child = side(&key, branch_bit);
                        parent_slot = Some(cursor + 3 + child as u64 * 8);
                        cursor = children[child];
                    }
                    _ => break,
                }
            }
            let branch = self.file.seek(SeekFrom::End(0))?;
            let mut children = [cursor; 2];
            children[side(&key, bit)] = leaf;
            self.file.write_all(&[0])?;
            self.file.write_all(&bit.to_le_bytes())?;
            for child in children {
                self.file.write_all(&child.to_le_bytes())?;
            }
            if let Some(slot) = parent_slot {
                self.file.seek(SeekFrom::Start(slot))?;
                self.file.write_all(&branch.to_le_bytes())?;
            } else {
                self.root = Some(branch);
            }
        } else {
            self.root = Some(leaf);
        }
        self.healthy = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_lookup_survives_long_common_prefixes() {
        let mut index = DiskRadix::new().unwrap();
        for value in 0..10_000_u64 {
            let mut key = [0; 32];
            key[24..].copy_from_slice(&value.to_be_bytes());
            index.insert(key, value).unwrap();
        }
        for value in (0..10_000_u64).rev() {
            let mut key = [0; 32];
            key[24..].copy_from_slice(&value.to_be_bytes());
            assert_eq!(index.get(key).unwrap(), Some(value));
        }
        assert_eq!(index.get([255; 32]).unwrap(), None);
        assert!(index.insert([0; 32], 123).is_err());
        assert_eq!(index.get([0; 32]).unwrap(), Some(0));
    }

    #[test]
    fn cyclic_or_invalid_branches_fail_in_bounded_steps() {
        for invalid_bit in [false, true] {
            let mut index = DiskRadix::new().unwrap();
            index.insert([0; 32], 1).unwrap();
            index.insert([255; 32], 2).unwrap();
            let root = index.root.unwrap();
            if invalid_bit {
                index.file.seek(SeekFrom::Start(root + 1)).unwrap();
                index.file.write_all(&256_u16.to_le_bytes()).unwrap();
            } else {
                index.file.seek(SeekFrom::Start(root + 3)).unwrap();
                index.file.write_all(&root.to_le_bytes()).unwrap();
            }
            assert_eq!(
                index.get([0; 32]).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
    }
}
