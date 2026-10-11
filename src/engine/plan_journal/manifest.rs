//! Append-time authenticated record locators, with a fixed 64-level frontier.
//! The immutable root stays in memory; no read may adopt a new scratch digest.
use super::reader::{read_exact_at, PlanRecordPosition};
use super::{PlanJournalError, Result};
use std::fs::File;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;

const NODE_BYTES: usize = 113;
const LEVELS: usize = u64::BITS as usize;

#[derive(Clone, Copy)]
struct Node {
    first: u64,
    count: u64,
    offset: u64,
    digest: [u8; 32],
}

pub(super) struct ManifestWriter {
    file: tokio::fs::File,
    frontier: [Option<Node>; LEVELS],
    end: u64,
}

#[derive(Clone)]
pub(super) struct ManifestReader {
    file: Arc<File>,
    root: Option<Node>,
    end: u64,
}

fn invalid(message: &'static str) -> PlanJournalError {
    PlanJournalError::InvalidRecord(message)
}
fn integer(bytes: &[u8], at: usize) -> Result<u64> {
    Ok(u64::from_le_bytes(
        bytes[at..at + 8]
            .try_into()
            .map_err(|_| invalid("invalid manifest integer"))?,
    ))
}

impl ManifestWriter {
    pub(super) async fn new() -> Result<Self> {
        let file = tokio::task::spawn_blocking(tempfile::tempfile)
            .await
            .map_err(|error| PlanJournalError::Worker(error.to_string()))??;
        Ok(Self {
            file: tokio::fs::File::from_std(file),
            frontier: [None; LEVELS],
            end: 0,
        })
    }

    async fn write(&mut self, bytes: [u8; NODE_BYTES], first: u64, count: u64) -> Result<Node> {
        let next = self
            .end
            .checked_add(NODE_BYTES as u64)
            .ok_or_else(|| invalid("manifest extent overflow"))?;
        let node = Node {
            first,
            count,
            offset: self.end,
            digest: *blake3::hash(&bytes).as_bytes(),
        };
        self.file.write_all(&bytes).await?;
        self.end = next;
        Ok(node)
    }

    async fn branch(&mut self, left: Node, right: Node) -> Result<Node> {
        if left.first.checked_add(left.count) != Some(right.first) {
            return Err(invalid("manifest children are not contiguous"));
        }
        let count = left
            .count
            .checked_add(right.count)
            .ok_or_else(|| invalid("manifest record count overflow"))?;
        let mut bytes = [0; NODE_BYTES];
        bytes[0] = 1;
        bytes[1..9].copy_from_slice(&left.first.to_le_bytes());
        bytes[9..17].copy_from_slice(&count.to_le_bytes());
        bytes[17..25].copy_from_slice(&left.offset.to_le_bytes());
        bytes[25..33].copy_from_slice(&left.count.to_le_bytes());
        bytes[33..65].copy_from_slice(&left.digest);
        bytes[65..73].copy_from_slice(&right.offset.to_le_bytes());
        bytes[73..81].copy_from_slice(&right.count.to_le_bytes());
        bytes[81..113].copy_from_slice(&right.digest);
        self.write(bytes, left.first, count).await
    }

    pub(super) async fn append(&mut self, position: PlanRecordPosition) -> Result<()> {
        let mut bytes = [0; NODE_BYTES];
        bytes[1..9].copy_from_slice(&position.ordinal.to_le_bytes());
        bytes[9..17].copy_from_slice(&1_u64.to_le_bytes());
        bytes[17..25].copy_from_slice(&position.offset.to_le_bytes());
        bytes[25..29].copy_from_slice(&position.payload_bytes.to_le_bytes());
        bytes[29..61].copy_from_slice(&position.digest);
        let mut node = self.write(bytes, position.ordinal, 1).await?;
        for level in 0..LEVELS {
            if let Some(left) = self.frontier[level].take() {
                node = self.branch(left, node).await?;
            } else {
                self.frontier[level] = Some(node);
                return Ok(());
            }
        }
        Err(invalid("manifest frontier overflow"))
    }

    pub(super) async fn seal(mut self, records: u64) -> Result<ManifestReader> {
        let mut root = None;
        // Higher ranges precede lower ones. Folding in reverse level order
        // retains append ordering even when the tree is not a power of two.
        for level in (0..LEVELS).rev() {
            if let Some(node) = self.frontier[level].take() {
                root = Some(match root {
                    Some(left) => self.branch(left, node).await?,
                    None => node,
                });
            }
        }
        if root.map_or(0, |node| node.count) != records || root.is_some_and(|node| node.first != 0)
        {
            return Err(invalid("manifest does not cover the complete plan"));
        }
        self.file.flush().await?;
        if self.file.metadata().await?.len() != self.end {
            return Err(invalid("manifest extent changed during append"));
        }
        Ok(ManifestReader {
            file: Arc::new(self.file.into_std().await),
            root,
            end: self.end,
        })
    }
}

impl ManifestReader {
    pub(super) fn position(&self, ordinal: u64) -> Result<PlanRecordPosition> {
        let mut node = self.root.ok_or_else(|| invalid("empty plan manifest"))?;
        if ordinal >= node.count || self.file.metadata()?.len() != self.end {
            return Err(invalid("position outside sealed manifest"));
        }
        // A binary-carry tree plus final frontier folds has at most twice the
        // u64 bit depth. Authenticated forward/backward extents forbid cycles.
        for _ in 0..LEVELS * 2 {
            if !node.offset.is_multiple_of(NODE_BYTES as u64)
                || node
                    .offset
                    .checked_add(NODE_BYTES as u64)
                    .is_none_or(|end| end > self.end)
            {
                return Err(invalid("manifest node outside sealed extent"));
            }
            let mut bytes = [0; NODE_BYTES];
            read_exact_at(&self.file, &mut bytes, node.offset)?;
            if blake3::hash(&bytes).as_bytes() != &node.digest
                || integer(&bytes, 1)? != node.first
                || integer(&bytes, 9)? != node.count
            {
                return Err(invalid("manifest node changed after append"));
            }
            match bytes[0] {
                0 => {
                    if node.count != 1
                        || node.first != ordinal
                        || bytes[61..].iter().any(|byte| *byte != 0)
                    {
                        return Err(invalid("invalid manifest leaf"));
                    }
                    return Ok(PlanRecordPosition {
                        offset: integer(&bytes, 17)?,
                        ordinal,
                        payload_bytes: u32::from_le_bytes(
                            bytes[25..29]
                                .try_into()
                                .map_err(|_| invalid("invalid manifest length"))?,
                        ),
                        digest: bytes[29..61]
                            .try_into()
                            .map_err(|_| invalid("invalid manifest digest"))?,
                    });
                }
                1 => {
                    let left_count = integer(&bytes, 25)?;
                    let right_count = integer(&bytes, 73)?;
                    if left_count == 0
                        || right_count == 0
                        || left_count.checked_add(right_count) != Some(node.count)
                    {
                        return Err(invalid("invalid manifest branch counts"));
                    }
                    let right_first = node
                        .first
                        .checked_add(left_count)
                        .ok_or_else(|| invalid("manifest ordinal overflow"))?;
                    let (first, count, offset, digest) = if ordinal < right_first {
                        (node.first, left_count, integer(&bytes, 17)?, &bytes[33..65])
                    } else {
                        (
                            right_first,
                            right_count,
                            integer(&bytes, 65)?,
                            &bytes[81..113],
                        )
                    };
                    if offset >= node.offset {
                        return Err(invalid("manifest branch is cyclic"));
                    }
                    node = Node {
                        first,
                        count,
                        offset,
                        digest: digest
                            .try_into()
                            .map_err(|_| invalid("invalid child digest"))?,
                    };
                }
                _ => return Err(invalid("invalid manifest node tag")),
            }
        }
        Err(invalid("manifest exceeds bounded proof depth"))
    }

    #[cfg(test)]
    pub(super) fn file(&self) -> &File {
        &self.file
    }
}
