use crate::engine::reconcile::BoxError;
use crate::engine::rolling::WeakChecksum;
use bytes::Bytes;
use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read};

pub const STRONG_SIGNATURE_LEN: usize = 16;
pub const SOURCE_DIGEST_LEN: usize = 32;
pub const DEFAULT_MAX_BASIS_BLOCKS: usize = 65_536;
pub const MAX_LITERAL_BYTES: usize = 64 * 1024;
const READ_BUFFER_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BasisBlock {
    pub index: u64,
    pub size: u32,
    pub weak: u32,
    pub strong: [u8; STRONG_SIGNATURE_LEN],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BasisIndexLimits {
    pub max_blocks: usize,
}

impl Default for BasisIndexLimits {
    fn default() -> Self {
        Self {
            max_blocks: DEFAULT_MAX_BASIS_BLOCKS,
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BasisIndexError {
    #[error("delta block size must be non-zero")]
    ZeroBlockSize,

    #[error("basis signature limit must be non-zero")]
    ZeroBlockLimit,

    #[error("basis signature count exceeds configured maximum of {max}")]
    TooManyBlocks { max: usize },

    #[error("basis signature index mismatch: expected {expected}, got {actual}")]
    IndexMismatch { expected: u64, actual: u64 },

    #[error("basis signature block {index} has zero length")]
    EmptyBlock { index: u64 },

    #[error("basis signature block {index} is {size} bytes, larger than block size {block_size}")]
    BlockTooLarge {
        index: u64,
        size: u32,
        block_size: u32,
    },

    #[error("basis signature block {index} follows a short final block")]
    BlockAfterShort { index: u64 },

    #[error("basis block offset overflow at index {index}")]
    OffsetOverflow { index: u64 },
}

#[derive(Debug)]
pub struct BasisIndex {
    block_size: u32,
    block_count: usize,
    by_weak: HashMap<u32, Vec<BasisBlock>>,
}

/// Incrementally builds a bounded signature index as remote signatures arrive.
/// No temporary whole-file signature vector is required.
#[derive(Debug)]
pub struct BasisIndexBuilder {
    block_size: u32,
    limits: BasisIndexLimits,
    block_count: usize,
    expected_index: u64,
    short_seen: bool,
    by_weak: HashMap<u32, Vec<BasisBlock>>,
}

impl BasisIndexBuilder {
    pub fn new(
        block_size: u32,
        limits: BasisIndexLimits,
    ) -> std::result::Result<Self, BasisIndexError> {
        if block_size == 0 {
            return Err(BasisIndexError::ZeroBlockSize);
        }
        if limits.max_blocks == 0 {
            return Err(BasisIndexError::ZeroBlockLimit);
        }
        Ok(Self {
            block_size,
            limits,
            block_count: 0,
            expected_index: 0,
            short_seen: false,
            by_weak: HashMap::new(),
        })
    }

    pub fn push(&mut self, block: BasisBlock) -> std::result::Result<(), BasisIndexError> {
        if self.block_count == self.limits.max_blocks {
            return Err(BasisIndexError::TooManyBlocks {
                max: self.limits.max_blocks,
            });
        }
        if block.index != self.expected_index {
            return Err(BasisIndexError::IndexMismatch {
                expected: self.expected_index,
                actual: block.index,
            });
        }
        if block.size == 0 {
            return Err(BasisIndexError::EmptyBlock { index: block.index });
        }
        if block.size > self.block_size {
            return Err(BasisIndexError::BlockTooLarge {
                index: block.index,
                size: block.size,
                block_size: self.block_size,
            });
        }
        if self.short_seen {
            return Err(BasisIndexError::BlockAfterShort { index: block.index });
        }

        self.short_seen = block.size < self.block_size;
        self.by_weak.entry(block.weak).or_default().push(block);
        self.block_count += 1;
        self.expected_index = self
            .expected_index
            .checked_add(1)
            .ok_or(BasisIndexError::OffsetOverflow { index: block.index })?;
        Ok(())
    }

    pub const fn block_count(&self) -> usize {
        self.block_count
    }

    pub fn finish(self) -> BasisIndex {
        BasisIndex {
            block_size: self.block_size,
            block_count: self.block_count,
            by_weak: self.by_weak,
        }
    }
}

impl BasisIndex {
    pub fn new<I>(
        block_size: u32,
        blocks: I,
        limits: BasisIndexLimits,
    ) -> std::result::Result<Self, BasisIndexError>
    where
        I: IntoIterator<Item = BasisBlock>,
    {
        let mut builder = BasisIndexBuilder::new(block_size, limits)?;
        for block in blocks {
            builder.push(block)?;
        }
        Ok(builder.finish())
    }

    pub const fn block_size(&self) -> u32 {
        self.block_size
    }

    pub const fn block_count(&self) -> usize {
        self.block_count
    }

    fn find_slice(&self, bytes: &[u8]) -> Option<BasisBlock> {
        let size = u32::try_from(bytes.len()).ok()?;
        let weak = WeakChecksum::hash(bytes);
        let candidates = self.by_weak.get(&weak)?;
        let strong = strong_signature(bytes);
        candidates
            .iter()
            .copied()
            .find(|candidate| candidate.size == size && candidate.strong == strong)
    }

    fn find_window(&self, weak: u32, window: &RollingWindow) -> Option<BasisBlock> {
        let size = u32::try_from(window.len()).ok()?;
        let candidates = self.by_weak.get(&weak)?;
        let strong = window.strong_signature();
        candidates
            .iter()
            .copied()
            .find(|candidate| candidate.size == size && candidate.strong == strong)
    }

    fn offset(&self, block: BasisBlock) -> std::result::Result<u64, BasisIndexError> {
        block
            .index
            .checked_mul(u64::from(self.block_size))
            .ok_or(BasisIndexError::OffsetOverflow { index: block.index })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeltaOp {
    Literal(Bytes),
    Copy { basis_offset: u64, len: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeltaSummary {
    pub source_bytes: u64,
    pub literal_bytes: u64,
    pub reused_bytes: u64,
    pub operation_count: u64,
    pub source_digest: [u8; SOURCE_DIGEST_LEN],
}

#[derive(Debug, thiserror::Error)]
pub enum DeltaMatchError {
    #[error(transparent)]
    Io(#[from] io::Error),

    #[error(transparent)]
    Basis(#[from] BasisIndexError),

    #[error("delta sink failed")]
    Sink(#[source] BoxError),

    #[error("delta byte count overflow")]
    ByteCountOverflow,

    #[error("delta operation count overflow")]
    OperationCountOverflow,
}

pub type Result<T> = std::result::Result<T, DeltaMatchError>;

/// Match one source stream against a bounded destination signature index.
///
/// Memory is bounded by the signature index, one adaptive-size rolling window,
/// the reader buffer, and at most `MAX_LITERAL_BYTES` of pending literal data.
/// Operations are emitted directly to `sink`; no whole-file delta plan exists.
/// The full source BLAKE3 digest is accumulated in output order during the same
/// pass, so a successful transfer does not need to reopen or reread the source.
pub fn match_delta<R, F>(reader: R, basis: &BasisIndex, mut sink: F) -> Result<DeltaSummary>
where
    R: Read,
    F: FnMut(DeltaOp) -> std::result::Result<(), BoxError>,
{
    let block_size = usize::try_from(basis.block_size()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "delta block size does not fit usize",
        )
    })?;
    let mut reader = BufReader::with_capacity(READ_BUFFER_BYTES, reader);
    let first = read_up_to(&mut reader, block_size)?;
    let mut emitter = DeltaEmitter::new(&mut sink);

    if first.is_empty() {
        return emitter.finish();
    }
    if first.len() < block_size {
        emit_final_slice(first, basis, &mut emitter)?;
        return emitter.finish();
    }

    let mut window = RollingWindow::new(first);
    let mut weak = WeakChecksum::from_block(window.as_contiguous());

    loop {
        if let Some(block) = basis.find_window(weak.digest(), &window) {
            emitter.copy_slices(basis.offset(block)?, block.size, window.slices())?;
            let next = read_up_to(&mut reader, block_size)?;
            if next.is_empty() {
                return emitter.finish();
            }
            if next.len() < block_size {
                emit_final_slice(next, basis, &mut emitter)?;
                return emitter.finish();
            }
            window = RollingWindow::new(next);
            weak = WeakChecksum::from_block(window.as_contiguous());
            continue;
        }

        let Some(incoming) = read_byte(&mut reader)? else {
            emitter.literal_slices(window.slices())?;
            return emitter.finish();
        };
        let outgoing = window.roll(incoming);
        emitter.literal_byte(outgoing)?;
        weak.roll(outgoing, incoming);
    }
}

fn emit_final_slice<F>(
    bytes: Vec<u8>,
    basis: &BasisIndex,
    emitter: &mut DeltaEmitter<'_, F>,
) -> Result<()>
where
    F: FnMut(DeltaOp) -> std::result::Result<(), BoxError>,
{
    if let Some(block) = basis.find_slice(&bytes) {
        emitter.copy_slice(basis.offset(block)?, block.size, &bytes)
    } else {
        emitter.literal_slice(&bytes)
    }
}

fn read_up_to<R: Read>(reader: &mut R, target: usize) -> io::Result<Vec<u8>> {
    let mut bytes = vec![0_u8; target];
    let mut read = 0_usize;
    while read < target {
        match reader.read(&mut bytes[read..]) {
            Ok(0) => break,
            Ok(count) => read += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    bytes.truncate(read);
    Ok(bytes)
}

fn read_byte<R: Read>(reader: &mut BufReader<R>) -> io::Result<Option<u8>> {
    let available = reader.fill_buf()?;
    if available.is_empty() {
        return Ok(None);
    }
    let byte = available[0];
    reader.consume(1);
    Ok(Some(byte))
}

fn strong_signature(bytes: &[u8]) -> [u8; STRONG_SIGNATURE_LEN] {
    let digest = blake3::hash(bytes);
    let mut strong = [0_u8; STRONG_SIGNATURE_LEN];
    strong.copy_from_slice(&digest.as_bytes()[..STRONG_SIGNATURE_LEN]);
    strong
}

struct RollingWindow {
    bytes: Vec<u8>,
    head: usize,
}

impl RollingWindow {
    fn new(bytes: Vec<u8>) -> Self {
        debug_assert!(!bytes.is_empty());
        Self { bytes, head: 0 }
    }

    fn len(&self) -> usize {
        self.bytes.len()
    }

    fn as_contiguous(&self) -> &[u8] {
        debug_assert_eq!(self.head, 0);
        &self.bytes
    }

    fn slices(&self) -> (&[u8], &[u8]) {
        let (before, after) = self.bytes.split_at(self.head);
        (after, before)
    }

    fn strong_signature(&self) -> [u8; STRONG_SIGNATURE_LEN] {
        let (first, second) = self.slices();
        let mut hasher = blake3::Hasher::new();
        hasher.update(first);
        hasher.update(second);
        let digest = hasher.finalize();
        let mut strong = [0_u8; STRONG_SIGNATURE_LEN];
        strong.copy_from_slice(&digest.as_bytes()[..STRONG_SIGNATURE_LEN]);
        strong
    }

    fn roll(&mut self, incoming: u8) -> u8 {
        let outgoing = self.bytes[self.head];
        self.bytes[self.head] = incoming;
        self.head += 1;
        if self.head == self.bytes.len() {
            self.head = 0;
        }
        outgoing
    }
}

struct DeltaEmitter<'a, F> {
    sink: &'a mut F,
    literal: Vec<u8>,
    source_hasher: blake3::Hasher,
    source_bytes: u64,
    literal_bytes: u64,
    reused_bytes: u64,
    operation_count: u64,
}

impl<'a, F> DeltaEmitter<'a, F>
where
    F: FnMut(DeltaOp) -> std::result::Result<(), BoxError>,
{
    fn new(sink: &'a mut F) -> Self {
        Self {
            sink,
            literal: Vec::with_capacity(MAX_LITERAL_BYTES),
            source_hasher: blake3::Hasher::new(),
            source_bytes: 0,
            literal_bytes: 0,
            reused_bytes: 0,
            operation_count: 0,
        }
    }

    fn literal_byte(&mut self, byte: u8) -> Result<()> {
        self.source_hasher.update(&[byte]);
        self.literal.push(byte);
        if self.literal.len() == MAX_LITERAL_BYTES {
            self.flush_literal()?;
        }
        Ok(())
    }

    fn literal_slice(&mut self, mut bytes: &[u8]) -> Result<()> {
        self.source_hasher.update(bytes);
        while !bytes.is_empty() {
            let available = MAX_LITERAL_BYTES - self.literal.len();
            let take = available.min(bytes.len());
            self.literal.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
            if self.literal.len() == MAX_LITERAL_BYTES {
                self.flush_literal()?;
            }
        }
        Ok(())
    }

    fn literal_slices(&mut self, slices: (&[u8], &[u8])) -> Result<()> {
        self.literal_slice(slices.0)?;
        self.literal_slice(slices.1)
    }

    fn copy_slice(&mut self, basis_offset: u64, len: u32, source: &[u8]) -> Result<()> {
        debug_assert_eq!(source.len(), len as usize);
        self.copy_slices(basis_offset, len, (source, &[]))
    }

    fn copy_slices(&mut self, basis_offset: u64, len: u32, source: (&[u8], &[u8])) -> Result<()> {
        debug_assert_eq!(source.0.len() + source.1.len(), len as usize);
        self.flush_literal()?;
        self.source_hasher.update(source.0);
        self.source_hasher.update(source.1);
        (self.sink)(DeltaOp::Copy { basis_offset, len }).map_err(DeltaMatchError::Sink)?;
        self.reused_bytes = self
            .reused_bytes
            .checked_add(u64::from(len))
            .ok_or(DeltaMatchError::ByteCountOverflow)?;
        self.source_bytes = self
            .source_bytes
            .checked_add(u64::from(len))
            .ok_or(DeltaMatchError::ByteCountOverflow)?;
        self.increment_ops()
    }

    fn flush_literal(&mut self) -> Result<()> {
        if self.literal.is_empty() {
            return Ok(());
        }
        let bytes = Bytes::from(std::mem::take(&mut self.literal));
        let len = u64::try_from(bytes.len()).map_err(|_| DeltaMatchError::ByteCountOverflow)?;
        (self.sink)(DeltaOp::Literal(bytes)).map_err(DeltaMatchError::Sink)?;
        self.literal_bytes = self
            .literal_bytes
            .checked_add(len)
            .ok_or(DeltaMatchError::ByteCountOverflow)?;
        self.source_bytes = self
            .source_bytes
            .checked_add(len)
            .ok_or(DeltaMatchError::ByteCountOverflow)?;
        self.increment_ops()?;
        self.literal = Vec::with_capacity(MAX_LITERAL_BYTES);
        Ok(())
    }

    fn increment_ops(&mut self) -> Result<()> {
        self.operation_count = self
            .operation_count
            .checked_add(1)
            .ok_or(DeltaMatchError::OperationCountOverflow)?;
        Ok(())
    }

    fn finish(mut self) -> Result<DeltaSummary> {
        self.flush_literal()?;
        Ok(DeltaSummary {
            source_bytes: self.source_bytes,
            literal_bytes: self.literal_bytes,
            reused_bytes: self.reused_bytes,
            operation_count: self.operation_count,
            source_digest: *self.source_hasher.finalize().as_bytes(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(index: u64, bytes: &[u8]) -> BasisBlock {
        BasisBlock {
            index,
            size: bytes.len() as u32,
            weak: WeakChecksum::hash(bytes),
            strong: strong_signature(bytes),
        }
    }

    fn basis(data: &[u8], block_size: usize) -> BasisIndex {
        let blocks = data
            .chunks(block_size)
            .enumerate()
            .map(|(index, bytes)| block(index as u64, bytes))
            .collect::<Vec<_>>();
        BasisIndex::new(block_size as u32, blocks, BasisIndexLimits::default()).unwrap()
    }

    fn collect(source: &[u8], basis: &BasisIndex) -> (Vec<DeltaOp>, DeltaSummary) {
        let mut ops = Vec::new();
        let summary = match_delta(source, basis, |op| {
            ops.push(op);
            Ok(())
        })
        .unwrap();
        assert_eq!(summary.source_digest, *blake3::hash(source).as_bytes());
        (ops, summary)
    }

    fn reconstruct(ops: &[DeltaOp], basis_data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        for op in ops {
            match op {
                DeltaOp::Literal(bytes) => out.extend_from_slice(bytes),
                DeltaOp::Copy { basis_offset, len } => {
                    let start = *basis_offset as usize;
                    let end = start + *len as usize;
                    out.extend_from_slice(&basis_data[start..end]);
                }
            }
        }
        out
    }

    #[test]
    fn empty_source_reports_blake3_digest_without_ops() {
        let index = BasisIndex::new(4096, [], BasisIndexLimits::default()).unwrap();
        let (ops, summary) = collect(&[], &index);
        assert!(ops.is_empty());
        assert_eq!(summary.source_bytes, 0);
        assert_eq!(summary.operation_count, 0);
    }

    #[test]
    fn aligned_content_emits_only_copy_ops() {
        let destination = b"abcdefghijkl";
        let index = basis(destination, 4);
        let (ops, summary) = collect(destination, &index);

        assert_eq!(
            ops,
            vec![
                DeltaOp::Copy {
                    basis_offset: 0,
                    len: 4
                },
                DeltaOp::Copy {
                    basis_offset: 4,
                    len: 4
                },
                DeltaOp::Copy {
                    basis_offset: 8,
                    len: 4
                },
            ]
        );
        assert_eq!(summary.literal_bytes, 0);
        assert_eq!(summary.reused_bytes, 12);
        assert_eq!(reconstruct(&ops, destination), destination);
    }

    #[test]
    fn inserted_prefix_rolls_to_existing_blocks() {
        let destination = b"abcdefghijkl";
        let source = b"Xabcdefghijkl";
        let index = basis(destination, 4);
        let (ops, summary) = collect(source, &index);

        assert_eq!(reconstruct(&ops, destination), source);
        assert_eq!(summary.literal_bytes, 1);
        assert_eq!(summary.reused_bytes, 12);
        assert!(matches!(&ops[0], DeltaOp::Literal(bytes) if bytes.as_ref() == b"X"));
    }

    #[test]
    fn changed_middle_stays_literal_between_reused_blocks() {
        let destination = b"abcdefghijkl";
        let source = b"abcdZZZZijkl";
        let index = basis(destination, 4);
        let (ops, summary) = collect(source, &index);

        assert_eq!(reconstruct(&ops, destination), source);
        assert_eq!(summary.literal_bytes, 4);
        assert_eq!(summary.reused_bytes, 8);
    }

    #[test]
    fn weak_collision_requires_blake3_match() {
        let source = b"abcd";
        let mut wrong = block(0, source);
        wrong.strong[0] ^= 0xff;
        let index = BasisIndex::new(4, [wrong], BasisIndexLimits::default()).unwrap();
        let (ops, summary) = collect(source, &index);

        assert_eq!(reconstruct(&ops, b"zzzz"), source);
        assert_eq!(summary.literal_bytes, 4);
        assert_eq!(summary.reused_bytes, 0);
    }

    #[test]
    fn short_final_block_can_be_reused() {
        let destination = b"abcdef";
        let index = basis(destination, 4);
        let (ops, summary) = collect(destination, &index);

        assert_eq!(reconstruct(&ops, destination), destination);
        assert_eq!(summary.reused_bytes, 6);
        assert_eq!(summary.literal_bytes, 0);
    }

    #[test]
    fn literal_emission_is_bounded() {
        let source = vec![7_u8; MAX_LITERAL_BYTES * 2 + 17];
        let index = BasisIndex::new(4096, [], BasisIndexLimits::default()).unwrap();
        let (ops, summary) = collect(&source, &index);

        assert!(ops.iter().all(|op| match op {
            DeltaOp::Literal(bytes) => bytes.len() <= MAX_LITERAL_BYTES,
            DeltaOp::Copy { .. } => true,
        }));
        assert_eq!(summary.literal_bytes, source.len() as u64);
        assert_eq!(reconstruct(&ops, &[]), source);
    }

    #[test]
    fn incremental_builder_matches_constructor_shape() {
        let mut builder = BasisIndexBuilder::new(4, BasisIndexLimits::default()).unwrap();
        builder.push(block(0, b"abcd")).unwrap();
        builder.push(block(1, b"efgh")).unwrap();
        assert_eq!(builder.block_count(), 2);
        let index = builder.finish();
        assert_eq!(index.block_size(), 4);
        assert_eq!(index.block_count(), 2);
    }

    #[test]
    fn signature_index_enforces_bounds_and_order() {
        let first = block(0, b"abcd");
        let second = block(1, b"efgh");
        assert_eq!(
            BasisIndex::new(4, [first, second], BasisIndexLimits { max_blocks: 1 }).unwrap_err(),
            BasisIndexError::TooManyBlocks { max: 1 }
        );

        let skipped = BasisBlock { index: 2, ..first };
        assert_eq!(
            BasisIndex::new(4, [skipped], BasisIndexLimits::default()).unwrap_err(),
            BasisIndexError::IndexMismatch {
                expected: 0,
                actual: 2
            }
        );

        let short = block(0, b"ab");
        assert_eq!(
            BasisIndex::new(4, [short, second], BasisIndexLimits::default()).unwrap_err(),
            BasisIndexError::BlockAfterShort { index: 1 }
        );
    }
}
