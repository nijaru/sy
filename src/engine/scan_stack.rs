//! Shared authenticated continuation owner. Only the current lexical path is
//! resident; inactive frames use one fixed-payload index FD and a native root.
use super::{invalid, Health, RunCursor};
use crate::authenticated_index::AuthenticatedIndex;
use std::io;

#[derive(Clone, Copy)]
pub(crate) struct Frame {
    pub run: RunCursor,
    pub device: u64,
    pub inode: u64,
}

pub(crate) struct Stack {
    index: AuthenticatedIndex<72>,
    count: usize,
    health: Health,
}
impl Stack {
    pub fn new(health: Health) -> io::Result<Self> {
        health.check()?;
        let result = AuthenticatedIndex::new();
        Ok(Self {
            index: health.finish(result)?,
            count: 0,
            health,
        })
    }
    fn key(depth: usize) -> io::Result<[u8; 32]> {
        let mut key = [0; 32];
        key[..8].copy_from_slice(&u64::try_from(depth).map_err(|_| invalid())?.to_be_bytes());
        Ok(key)
    }
    pub fn write(&mut self, depth: usize, frame: Frame) -> io::Result<()> {
        self.health.check()?;
        let result = (|| {
            if depth > self.count
                || (depth < self.count && depth.checked_add(1) != Some(self.count))
            {
                return Err(invalid());
            }
            let mut values = [0; 9];
            values[..7].copy_from_slice(&frame.run.values());
            values[7] = frame.device;
            values[8] = frame.inode;
            let mut bytes = [0; 72];
            for (slot, value) in bytes.as_chunks_mut::<8>().0.iter_mut().zip(values) {
                *slot = value.to_le_bytes();
            }
            self.index.set_bytes(Self::key(depth)?, bytes)?;
            self.count = self.count.max(depth.checked_add(1).ok_or_else(invalid)?);
            Ok(())
        })();
        self.health.finish(result)
    }
    pub fn read(&mut self, depth: usize) -> io::Result<Frame> {
        self.health.check()?;
        let result = (|| {
            if depth >= self.count {
                return Err(invalid());
            }
            let bytes = self
                .index
                .get_bytes(Self::key(depth)?)?
                .ok_or_else(invalid)?;
            let mut values = [0; 9];
            for (slot, value) in bytes.as_chunks::<8>().0.iter().zip(&mut values) {
                *value = u64::from_le_bytes(*slot);
            }
            Ok(Frame {
                run: RunCursor::from_values(values[..7].try_into().map_err(|_| invalid())?),
                device: values[7],
                inode: values[8],
            })
        })();
        self.health.finish(result)
    }
    pub fn truncate(&mut self, frames: usize) -> io::Result<()> {
        self.health.check()?;
        // Prove the retained top and index extent before changing native count.
        // Stale deeper slots cannot be read and are overwritten before reuse.
        let result = (|| {
            if frames == 0 || frames > self.count {
                return Err(invalid());
            }
            self.read(frames - 1)?;
            self.count = frames;
            Ok(())
        })();
        self.health.finish(result)
    }
    pub fn check_top(&mut self, depth: usize) -> io::Result<()> {
        self.health.check()?;
        if depth.checked_add(1) != Some(self.count) {
            return self.health.finish(Err(invalid()));
        }
        self.read(depth).map(|_| ())
    }
    pub fn contains(&mut self, depth: usize, candidate: (u64, u64)) -> io::Result<bool> {
        self.check_top(depth)?;
        for index in 0..=depth {
            let frame = self.read(index)?;
            if (frame.device, frame.inode) == candidate {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::scan_sort::{NameSpool, SortBudget};
    use std::io::{Seek, SeekFrom, Write};

    #[test]
    fn missing_and_substituted_continuations_poison_the_scan_owner() {
        for truncate in [true, false] {
            let mut spool = NameSpool::new(SortBudget::default()).unwrap();
            let run = spool.sort([Ok("original".into())], || false).unwrap();
            let mut stack = Stack::new(spool.health()).unwrap();
            stack
                .write(
                    0,
                    Frame {
                        run,
                        device: 1,
                        inode: 2,
                    },
                )
                .unwrap();
            if truncate {
                stack.index.file.set_len(0).unwrap();
            } else {
                stack.index.file.seek(SeekFrom::Start(33 + 7 * 8)).unwrap();
                stack.index.file.write_all(&99_u64.to_le_bytes()).unwrap();
            }
            assert!(stack.read(0).is_err());
            assert!(stack.read(0).is_err());
            assert!(spool.open(run).is_err());
            assert!(spool.sort([Ok("later".into())], || false).is_err());
        }
    }

    #[test]
    fn old_frame_replay_and_truncated_depth_cannot_resume() {
        use std::io::Read;
        let mut spool = NameSpool::new(SortBudget::default()).unwrap();
        let run = spool.sort([Ok("name".into())], || false).unwrap();
        let mut stack = Stack::new(spool.health()).unwrap();
        let frame = Frame {
            run,
            device: 1,
            inode: 2,
        };
        stack.write(0, frame).unwrap();
        stack.index.file.seek(SeekFrom::Start(0)).unwrap();
        let mut old = Vec::new();
        stack.index.file.read_to_end(&mut old).unwrap();
        stack.write(0, Frame { inode: 3, ..frame }).unwrap();
        stack.index.file.seek(SeekFrom::Start(0)).unwrap();
        stack.index.file.write_all(&old).unwrap();
        assert!(stack.read(0).is_err());

        let mut stack = Stack::new(Health::new()).unwrap();
        stack.write(0, frame).unwrap();
        stack.write(1, frame).unwrap();
        stack.truncate(1).unwrap();
        assert!(stack.read(1).is_err());
    }
}
