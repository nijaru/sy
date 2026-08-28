from pathlib import Path


def replace_required(path: str, old: str, new: str) -> None:
    file = Path(path)
    text = file.read_text()
    if new in text:
        return
    if old not in text:
        raise SystemExit(f"required edit anchor missing in {path}")
    file.write_text(text.replace(old, new, 1))


replace_required(
    "src/transfer/delta.rs",
    '''impl BasisIndex {
    pub fn new<I>(
        block_size: u32,
        blocks: I,
        limits: BasisIndexLimits,
    ) -> std::result::Result<Self, BasisIndexError>
    where
        I: IntoIterator<Item = BasisBlock>,
    {
        if block_size == 0 {
            return Err(BasisIndexError::ZeroBlockSize);
        }
        if limits.max_blocks == 0 {
            return Err(BasisIndexError::ZeroBlockLimit);
        }

        let mut by_weak: HashMap<u32, Vec<BasisBlock>> = HashMap::new();
        let mut block_count = 0_usize;
        let mut expected_index = 0_u64;
        let mut short_seen = false;

        for block in blocks {
            if block_count == limits.max_blocks {
                return Err(BasisIndexError::TooManyBlocks {
                    max: limits.max_blocks,
                });
            }
            if block.index != expected_index {
                return Err(BasisIndexError::IndexMismatch {
                    expected: expected_index,
                    actual: block.index,
                });
            }
            if block.size == 0 {
                return Err(BasisIndexError::EmptyBlock { index: block.index });
            }
            if block.size > block_size {
                return Err(BasisIndexError::BlockTooLarge {
                    index: block.index,
                    size: block.size,
                    block_size,
                });
            }
            if short_seen {
                return Err(BasisIndexError::BlockAfterShort { index: block.index });
            }

            short_seen = block.size < block_size;
            by_weak.entry(block.weak).or_default().push(block);
            block_count += 1;
            expected_index = expected_index
                .checked_add(1)
                .ok_or(BasisIndexError::OffsetOverflow { index: block.index })?;
        }

        Ok(Self {
            block_size,
            block_count,
            by_weak,
        })
    }

''',
    '''impl BasisIndex {
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

''',
)

replace_required(
    "src/transfer/delta.rs",
    '''#[derive(Debug)]
pub struct BasisIndex {
    block_size: u32,
    block_count: usize,
    by_weak: HashMap<u32, Vec<BasisBlock>>,
}

''',
    '''#[derive(Debug)]
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

''',
)

replace_required(
    "src/transfer/delta.rs",
    '''    #[test]
    fn signature_index_enforces_bounds_and_order() {
''',
    '''    #[test]
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
''',
)

runtime = Path("src/remote/runtime.rs")
text = runtime.read_text()
text = text.replace(
    '''use crate::remote::signature::{
    request_signatures, serve_incoming_signatures, RemoteSignatureError, SignatureStream,
};
''',
    '''use crate::remote::signature::{
    choose_signature_block_size, request_signatures, serve_incoming_signatures, RemoteSignatureError,
    SignatureEvent, SignatureStream,
};
use crate::transfer::delta::{
    BasisBlock, BasisIndex, BasisIndexBuilder, BasisIndexError, BasisIndexLimits,
};
''',
    1,
)
text = text.replace(
    '''use std::path::{Path, PathBuf};
''',
    '''use futures::StreamExt;
use std::path::{Path, PathBuf};
''',
    1,
)
text = text.replace(
    '''    #[error("unsupported v3 request opener: {0:?}")]
    UnsupportedRequest(FrameKind),
''',
    '''    #[error("unsupported v3 request opener: {0:?}")]
    UnsupportedRequest(FrameKind),

    #[error(transparent)]
    Signature(#[from] RemoteSignatureError),

    #[error(transparent)]
    BasisIndex(#[from] BasisIndexError),

    #[error("remote signature stream failed: {0}")]
    SignatureStream(String),

    #[error("remote signature stream ended without SignatureEnd")]
    MissingSignatureEnd,

    #[error("signature block-size selection changed unexpectedly: expected {expected}, got {actual}")]
    SignatureBlockSizeMismatch { expected: u32, actual: u32 },
''',
    1,
)
text = text.replace(
    '''    pub async fn signatures(
        &self,
        basis: &Entry,
    ) -> crate::remote::signature::Result<(u32, SignatureStream)> {
''',
    '''    pub async fn signatures(
        &self,
        basis: &Entry,
    ) -> crate::remote::signature::Result<(u32, SignatureStream)> {
''',
    1,
)
anchor = '''        .await
    }
}

/// Peer-opened operations currently implemented by the v3 session runtime.
'''
addition = '''        .await
    }

    /// Build the bounded rolling-delta basis index directly from the validated
    /// remote signature stream. If an honest basis would exceed the configured
    /// signature budget, return `None` before opening a remote stream so the
    /// planner can fall back to whole-file transfer without wasting bandwidth.
    pub async fn delta_basis(
        &self,
        basis: &Entry,
        limits: BasisIndexLimits,
    ) -> Result<Option<BasisIndex>> {
        let block_size = choose_signature_block_size(basis.size);
        let mut builder = BasisIndexBuilder::new(block_size, limits)?;
        let max_blocks = u64::try_from(limits.max_blocks).unwrap_or(u64::MAX);
        let expected_blocks = basis.size.div_ceil(u64::from(block_size));
        if expected_blocks > max_blocks {
            return Ok(None);
        }

        let (actual_block_size, mut signatures) = self.signatures(basis).await?;
        if actual_block_size != block_size {
            return Err(RemoteSessionError::SignatureBlockSizeMismatch {
                expected: block_size,
                actual: actual_block_size,
            });
        }

        let mut over_limit = false;
        loop {
            let Some(event) = signatures.next().await else {
                return Err(RemoteSessionError::MissingSignatureEnd);
            };
            let event = event.map_err(|error| {
                RemoteSessionError::SignatureStream(error.to_string())
            })?;
            match event {
                SignatureEvent::Block(block) if !over_limit => {
                    let block = BasisBlock {
                        index: block.index,
                        size: block.size,
                        weak: block.weak,
                        strong: block.strong,
                    };
                    match builder.push(block) {
                        Ok(()) => {}
                        Err(BasisIndexError::TooManyBlocks { .. }) => over_limit = true,
                        Err(error) => return Err(error.into()),
                    }
                }
                SignatureEvent::Block(_) => {}
                SignatureEvent::End(_) => break,
            }
        }

        if over_limit {
            Ok(None)
        } else {
            Ok(Some(builder.finish()))
        }
    }
}

/// Peer-opened operations currently implemented by the v3 session runtime.
'''
if anchor not in text:
    raise SystemExit("delta_basis insertion anchor missing")
text = text.replace(anchor, addition, 1)
text = text.replace(
    '''    use crate::remote::signature::{SignatureEvent, SignatureSummary};
    use futures::StreamExt;
''',
    '''    use crate::transfer::delta::BasisIndexLimits;
''',
    1,
)
text = text.replace(
    '''    async fn collect_signature_summary(
        mut signatures: SignatureStream,
    ) -> (usize, SignatureSummary) {
        let mut block_count = 0_usize;
        let mut summary = None;
        while let Some(event) = signatures.next().await {
            match event.unwrap() {
                SignatureEvent::Block(_) => block_count += 1,
                SignatureEvent::End(end) => summary = Some(end),
            }
        }
        (block_count, summary.unwrap())
    }

''',
    '',
    1,
)
text = text.replace(
    '''        let (block_size, signatures) = session.signatures(&signature_basis).await.unwrap();
        let (paths, (blocks, summary)) = tokio::join!(
            collect_paths(entries),
            collect_signature_summary(signatures)
        );
        server.await.unwrap();

        assert_eq!(block_size, 4 * 1024);
        assert_eq!(blocks, 3);
        assert_eq!(
            summary,
            SignatureSummary {
                file_size: 10_000,
                block_count: 3,
                basis_identity: signature_basis.identity.unwrap(),
            }
        );
''',
    '''        let (paths, delta_basis) = tokio::join!(
            collect_paths(entries),
            session.delta_basis(&signature_basis, BasisIndexLimits::default())
        );
        server.await.unwrap();

        let delta_basis = delta_basis.unwrap().unwrap();
        assert_eq!(delta_basis.block_size(), 4 * 1024);
        assert_eq!(delta_basis.block_count(), 3);
''',
    1,
)
text = text.replace(
    '''    #[cfg(unix)]
    #[tokio::test]
    async fn session_runtime_multiplexes_scan_and_signatures() {
''',
    '''    #[test]
    fn signature_budget_preflight_avoids_unbounded_index_growth() {
        let block_size = choose_signature_block_size(80 * 1024 * 1024 * 1024);
        assert_eq!(block_size, 1024 * 1024);
        let expected_blocks = (80_u64 * 1024 * 1024 * 1024).div_ceil(u64::from(block_size));
        assert!(expected_blocks > BasisIndexLimits::default().max_blocks as u64);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn session_runtime_multiplexes_scan_and_signatures() {
''',
    1,
)
runtime.write_text(text)
