//! Destination namespace semantics and bounded alias preflight.
//!
//! A case-insensitive or normalization-insensitive destination can collapse two
//! distinct selected names onto one entry, silently replacing user data.
//! Preflight therefore checks every planned and kept name — plus its ancestor
//! prefixes — for aliasing before any destination mutation.
//!
//! Two bounds matter:
//!
//! - **Memory.** A tree (or one directory) can be arbitrarily wide, so the
//!   detector spools sorted runs to anonymous temporary files and merges with
//!   bounded fan-in instead of retaining a whole-tree map.
//! - **Honesty.** Filesystem name rules are not fully knowable from an OS name
//!   or a Unicode transform. `NamespaceSemantics` records what the destination
//!   root actually folds; when a rule is unknown the check stays conservative
//!   (assumes folding) and reports `NamespaceAmbiguity` instead of claiming a
//!   definitive collision.

use super::domain::RelativePath;
use super::native_path;
use crate::protocol::PlatformOs;
use std::cmp::Ordering;
use std::io;
use tokio::io::{AsyncSeekExt, AsyncWriteExt, SeekFrom};
use unicode_normalization::UnicodeNormalization;

/// How one name-comparison axis behaves on the destination filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Folding {
    /// The axis preserves distinctions: only byte-identical names alias.
    Exact,
    /// The axis folds names (case folding and/or Unicode canonical
    /// decomposition), so distinct byte names can alias.
    Folded,
    /// The axis could not be determined for this root.
    ///
    /// Preflight treats an unknown axis as folding (the conservative
    /// direction) and reports detected aliases as ambiguity rather than as a
    /// proven collision.
    Unspecified,
}

/// Name-comparison semantics of one destination root.
///
/// These describe the destination filesystem, not the endpoint OS: APFS can be
/// case-sensitive, ext4 directories can opt into case folding, and removable
/// FAT/exFAT volumes fold case on any OS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NamespaceSemantics {
    pub case: Folding,
    pub normalization: Folding,
}

impl Default for NamespaceSemantics {
    fn default() -> Self {
        Self::BYTE_EXACT
    }
}

impl NamespaceSemantics {
    /// Byte-exact names (Linux ext4/xfs/btrfs and most Unix defaults).
    pub const BYTE_EXACT: Self = Self {
        case: Folding::Exact,
        normalization: Folding::Exact,
    };

    /// Nothing is known about the root; assume folding and stay conservative.
    pub const UNSPECIFIED: Self = Self {
        case: Folding::Unspecified,
        normalization: Folding::Unspecified,
    };

    /// Case folding with Unicode canonical decomposition (APFS default, NTFS).
    pub const CASE_AND_NORMALIZATION_FOLDED: Self = Self {
        case: Folding::Folded,
        normalization: Folding::Folded,
    };

    /// Fallback approximation used only when a peer predates root-scoped
    /// namespace negotiation. Prefer `fs_util::namespace_semantics` or the
    /// negotiated value.
    pub fn for_platform(os: PlatformOs) -> Self {
        match os {
            PlatformOs::Macos | PlatformOs::Windows => Self::CASE_AND_NORMALIZATION_FOLDED,
            _ => Self::BYTE_EXACT,
        }
    }

    /// True when distinct byte names can never alias, letting preflight skip.
    pub const fn is_byte_exact(self) -> bool {
        matches!(self.case, Folding::Exact) && matches!(self.normalization, Folding::Exact)
    }

    /// True when every alias report is a proven collision under known rules.
    pub const fn is_fully_specified(self) -> bool {
        !matches!(self.case, Folding::Unspecified)
            && !matches!(self.normalization, Folding::Unspecified)
    }

    const fn folds_case(self, lenient: bool) -> bool {
        match self.case {
            Folding::Exact => false,
            Folding::Folded => true,
            Folding::Unspecified => lenient,
        }
    }

    const fn folds_normalization(self, lenient: bool) -> bool {
        match self.normalization {
            Folding::Exact => false,
            Folding::Folded => true,
            Folding::Unspecified => lenient,
        }
    }

    /// Alias key for `path`.
    ///
    /// `lenient` folds every axis that is `Folded` *or* `Unspecified`; the
    /// strict form folds only proven-folded axes. Equal strict keys alias under
    /// every interpretation of the unknown axes (a proven collision), while
    /// equal lenient-only keys alias under some interpretations (ambiguity).
    fn key(self, path: &RelativePath, lenient: bool) -> Vec<u8> {
        let fold_case = self.folds_case(lenient);
        let fold_norm = self.folds_normalization(lenient);
        if !fold_case && !fold_norm {
            return native_path::encode(path.as_path().as_os_str());
        }

        let mut bytes = Vec::new();
        for (index, component) in path.as_path().components().enumerate() {
            if index > 0 {
                bytes.push(0); // NUL cannot occur inside a path component.
            }
            fold_component(&mut bytes, component.as_os_str(), fold_case, fold_norm);
        }
        bytes
    }
}

fn fold_component(
    bytes: &mut Vec<u8>,
    component: &std::ffi::OsStr,
    fold_case: bool,
    fold_norm: bool,
) {
    let raw = component.as_encoded_bytes();
    let Ok(text) = std::str::from_utf8(raw) else {
        // Non-UTF-8 names have no Unicode folding model; fold ASCII case only.
        for &byte in raw {
            bytes.push(if fold_case {
                byte.to_ascii_lowercase()
            } else {
                byte
            });
        }
        return;
    };

    let mut folded: std::borrow::Cow<'_, str> = std::borrow::Cow::Borrowed(text);
    if fold_case {
        folded = std::borrow::Cow::Owned(folded.to_lowercase());
    }
    if fold_norm {
        folded = std::borrow::Cow::Owned(folded.nfd().collect());
    }
    if fold_case {
        // Second pass: lowercase expansions produced by decomposition.
        for ch in folded.chars() {
            for lower in ch.to_lowercase() {
                let mut buf = [0_u8; 4];
                bytes.extend_from_slice(lower.encode_utf8(&mut buf).as_bytes());
            }
        }
    } else {
        bytes.extend_from_slice(folded.as_bytes());
    }
}

/// Destination names alias: one entry would silently replace the other.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "destination namespace collision: '{colliding}' collides with '{existing}' under case-insensitive/normalizing semantics"
)]
pub struct NamespaceCollision {
    pub existing: RelativePath,
    pub colliding: RelativePath,
}

/// Destination names may alias because name semantics are unknown.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "destination namespace ambiguity: '{colliding}' may collide with '{existing}'; destination name semantics could not be determined"
)]
pub struct NamespaceAmbiguity {
    pub existing: RelativePath,
    pub colliding: RelativePath,
}

#[derive(Debug, thiserror::Error)]
pub enum NamespacePreflightError {
    #[error(transparent)]
    Collision(#[from] NamespaceCollision),

    #[error(transparent)]
    Ambiguity(#[from] NamespaceAmbiguity),

    #[error("namespace preflight scratch I/O failed: {0}")]
    Io(#[from] io::Error),

    #[error("namespace preflight scratch worker failed: {0}")]
    Worker(String),

    #[error("namespace preflight record is too large: {actual} bytes, maximum {maximum}")]
    RecordTooLarge { actual: usize, maximum: usize },

    #[error("namespace preflight scratch record is invalid: {0}")]
    InvalidRecord(&'static str),
}

pub type Result<T> = std::result::Result<T, NamespacePreflightError>;

const MAX_RECORD_BYTES: usize = 1024 * 1024;

/// Resource bounds for the disk-backed alias sort.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpillBudget {
    /// Buffered bytes before an in-memory run is sorted and spilled.
    pub run_bytes: usize,
    /// Maximum simultaneously merged (and therefore open) run files.
    pub merge_fan_in: usize,
}

impl Default for SpillBudget {
    fn default() -> Self {
        Self {
            run_bytes: 4 * 1024 * 1024,
            merge_fan_in: 8,
        }
    }
}

/// One alias-check record: sort key, classification key, and native name bytes.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct SortRecord {
    lenient_key: Vec<u8>,
    strict_key: Vec<u8>,
    path: Vec<u8>,
}

impl SortRecord {
    fn new(semantics: NamespaceSemantics, path: &RelativePath) -> Result<Self> {
        let path_bytes = native_path::encode(path.as_path().as_os_str());
        let record = Self {
            lenient_key: semantics.key(path, true),
            strict_key: semantics.key(path, false),
            path: path_bytes,
        };
        let size = record.lenient_key.len() + record.strict_key.len() + record.path.len() + 12;
        if size > MAX_RECORD_BYTES {
            return Err(NamespacePreflightError::RecordTooLarge {
                actual: size,
                maximum: MAX_RECORD_BYTES,
            });
        }
        Ok(record)
    }

    fn encoded_len(&self) -> usize {
        self.lenient_key.len() + self.strict_key.len() + self.path.len() + 12
    }

    fn encode(&self, out: &mut Vec<u8>) {
        for field in [&self.lenient_key, &self.strict_key, &self.path] {
            out.extend_from_slice(&(field.len() as u32).to_be_bytes());
            out.extend_from_slice(field);
        }
    }

    fn relative_path(&self) -> Result<RelativePath> {
        let path = native_path::decode(&self.path)?;
        RelativePath::new(path).map_err(|_| {
            NamespacePreflightError::InvalidRecord("scratch record holds an invalid relative path")
        })
    }
}

/// Sequential reader for scratch records with length validation before
/// allocation.
mod codec {
    use super::{NamespacePreflightError, Result, MAX_RECORD_BYTES};
    use tokio::io::AsyncReadExt;

    pub(super) struct RecordReader<'a> {
        file: &'a mut tokio::fs::File,
    }

    impl<'a> RecordReader<'a> {
        pub(super) fn new(file: &'a mut tokio::fs::File) -> Self {
            Self { file }
        }

        pub(super) async fn field(&mut self) -> Result<Vec<u8>> {
            let len = self.file.read_u32().await.map_err(map_truncated)? as usize;
            if len > MAX_RECORD_BYTES {
                return Err(NamespacePreflightError::InvalidRecord(
                    "record field exceeds maximum size",
                ));
            }
            let mut bytes = vec![0_u8; len];
            self.file
                .read_exact(&mut bytes)
                .await
                .map_err(map_truncated)?;
            Ok(bytes)
        }

        /// Read one record, returning `None` at a clean end of run.
        pub(super) async fn record(&mut self) -> Result<Option<super::SortRecord>> {
            // Probe four bytes for the first field length without consuming a
            // partial record: 0 bytes is a clean end, 1-3 is corruption.
            let mut probe = [0_u8; 4];
            let mut filled = 0;
            while filled < 4 {
                match self.file.read(&mut probe[filled..]).await {
                    Ok(0) if filled == 0 => return Ok(None),
                    Ok(0) => {
                        return Err(NamespacePreflightError::InvalidRecord(
                            "truncated record length",
                        ))
                    }
                    Ok(n) => filled += n,
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(NamespacePreflightError::Io(error)),
                }
            }
            let first_len = u32::from_be_bytes(probe) as usize;
            if first_len > MAX_RECORD_BYTES {
                return Err(NamespacePreflightError::InvalidRecord(
                    "record field exceeds maximum size",
                ));
            }
            let mut lenient_key = vec![0_u8; first_len];
            self.file
                .read_exact(&mut lenient_key)
                .await
                .map_err(map_truncated)?;
            let strict_key = self.field().await?;
            let path = self.field().await?;
            let record = super::SortRecord {
                lenient_key,
                strict_key,
                path,
            };
            if record.encoded_len() > MAX_RECORD_BYTES {
                return Err(NamespacePreflightError::InvalidRecord(
                    "record fields exceed maximum size",
                ));
            }
            Ok(Some(record))
        }
    }

    fn map_truncated(error: std::io::Error) -> NamespacePreflightError {
        if error.kind() == std::io::ErrorKind::UnexpectedEof {
            NamespacePreflightError::InvalidRecord("truncated scratch record")
        } else {
            NamespacePreflightError::Io(error)
        }
    }
}

/// Scans sorted records for aliasing within equal lenient-key groups.
///
/// Every run file is internally checked when it is created (spill or merge),
/// which makes the final single run a complete proof over all records.
struct AliasScanner {
    previous: Option<std::sync::Arc<SortRecord>>,
    pending_ambiguity: Option<(RelativePath, RelativePath)>,
}

impl AliasScanner {
    fn new() -> Self {
        Self {
            previous: None,
            pending_ambiguity: None,
        }
    }

    /// Feed the next record in sorted order.
    ///
    /// Returns the record to emit when it is new, or `None` for an exact
    /// duplicate of the previous record.
    fn observe(&mut self, record: SortRecord) -> Result<Option<std::sync::Arc<SortRecord>>> {
        let Some(previous) = self.previous.as_ref() else {
            let record = std::sync::Arc::new(record);
            self.previous = Some(std::sync::Arc::clone(&record));
            return Ok(Some(record));
        };

        match previous.lenient_key.cmp(&record.lenient_key) {
            Ordering::Equal => {
                if previous.path == record.path {
                    return Ok(None);
                }
                // Distinct names alias under lenient folding. Equal strict keys
                // prove the alias under every interpretation of unknown axes.
                if previous.strict_key == record.strict_key {
                    return Err(NamespaceCollision {
                        existing: previous.relative_path()?,
                        colliding: record.relative_path()?,
                    }
                    .into());
                }
                if self.pending_ambiguity.is_none() {
                    self.pending_ambiguity =
                        Some((previous.relative_path()?, record.relative_path()?));
                }
            }
            Ordering::Greater => {
                return Err(NamespacePreflightError::InvalidRecord(
                    "scratch run records are out of order",
                ));
            }
            Ordering::Less => {
                self.finish_group()?;
            }
        }
        let record = std::sync::Arc::new(record);
        self.previous = Some(std::sync::Arc::clone(&record));
        Ok(Some(record))
    }

    fn finish_group(&mut self) -> Result<()> {
        if let Some((existing, colliding)) = self.pending_ambiguity.take() {
            return Err(NamespaceAmbiguity {
                existing,
                colliding,
            }
            .into());
        }
        Ok(())
    }

    fn finish(mut self) -> Result<()> {
        self.previous = None;
        self.finish_group()
    }
}

#[derive(Debug)]
struct RunFile {
    file: tokio::fs::File,
}

/// Bounded, exact destination-alias preflight over streaming names.
///
/// `record` every selected source name, kept destination name, and (through
/// `record`) their ancestor prefixes; `finish` proves the complete set before
/// mutation. Memory stays within `SpillBudget::run_bytes` plus one record per
/// merged run; open files stay within `SpillBudget::merge_fan_in`.
#[derive(Debug)]
pub struct NamespaceCollisionDetector {
    semantics: NamespaceSemantics,
    budget: SpillBudget,
    buffer: Vec<SortRecord>,
    buffered_bytes: usize,
    runs: Vec<RunFile>,
}

impl NamespaceCollisionDetector {
    pub fn new(semantics: NamespaceSemantics) -> Self {
        Self::with_budget(semantics, SpillBudget::default())
    }

    pub fn with_budget(semantics: NamespaceSemantics, budget: SpillBudget) -> Self {
        Self {
            semantics,
            budget: SpillBudget {
                run_bytes: budget.run_bytes.max(1),
                merge_fan_in: budget.merge_fan_in.max(2),
            },
            buffer: Vec::new(),
            buffered_bytes: 0,
            runs: Vec::new(),
        }
    }

    pub const fn semantics(&self) -> NamespaceSemantics {
        self.semantics
    }

    /// Record `path` and every ancestor directory prefix.
    pub async fn record(&mut self, path: &RelativePath) -> Result<()> {
        if self.semantics.is_byte_exact() {
            return Ok(());
        }
        self.record_single(path)?;
        let mut current = path.parent();
        while let Some(parent) = current {
            self.record_single(&parent)?;
            current = parent.parent();
        }
        while self.buffered_bytes >= self.budget.run_bytes {
            self.spill_run().await?;
        }
        Ok(())
    }

    fn record_single(&mut self, path: &RelativePath) -> Result<()> {
        let record = SortRecord::new(self.semantics, path)?;
        self.buffered_bytes += record.encoded_len();
        self.buffer.push(record);
        Ok(())
    }

    /// Prove that no two recorded names alias. Runs complete before any
    /// destination mutation.
    pub async fn finish(mut self) -> Result<()> {
        if self.semantics.is_byte_exact() {
            return Ok(());
        }
        if self.runs.is_empty() {
            return scan_buffered(std::mem::take(&mut self.buffer));
        }

        self.spill_run().await?;
        while self.runs.len() > self.budget.merge_fan_in {
            let mut next_round: Vec<RunFile> = Vec::new();
            let runs = std::mem::take(&mut self.runs);
            let mut groups = runs.into_iter().peekable();
            while groups.peek().is_some() {
                let mut group = Vec::new();
                for run in groups.by_ref().take(self.budget.merge_fan_in) {
                    group.push(run);
                }
                if group.len() == 1 {
                    if let Some(run) = group.pop() {
                        next_round.push(run);
                    }
                } else if let Some(run) = merge_group(group, true).await? {
                    next_round.push(run);
                }
            }
            self.runs = next_round;
        }

        merge_group(std::mem::take(&mut self.runs), false).await?;
        Ok(())
    }

    async fn spill_run(&mut self) -> Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let mut records = std::mem::take(&mut self.buffer);
        self.buffered_bytes = 0;
        records.sort();

        let file = tokio::task::spawn_blocking(tempfile::tempfile)
            .await
            .map_err(|error| NamespacePreflightError::Worker(error.to_string()))??;
        let file = tokio::fs::File::from_std(file);
        let mut writer = tokio::io::BufWriter::new(file);
        let mut scanner = AliasScanner::new();
        let mut payload = Vec::new();
        for record in records {
            let Some(emitted) = scanner.observe(record)? else {
                continue;
            };
            payload.clear();
            emitted.encode(&mut payload);
            writer.write_all(&payload).await?;
        }
        scanner.finish()?;
        writer.flush().await?;
        let mut file = writer.into_inner();
        file.seek(SeekFrom::Start(0)).await?;
        self.runs.push(RunFile { file });
        Ok(())
    }
}

fn scan_buffered(mut records: Vec<SortRecord>) -> Result<()> {
    records.sort();
    let mut scanner = AliasScanner::new();
    for record in records {
        scanner.observe(record)?;
    }
    scanner.finish()
}

/// Merge sorted runs in bounded fan-in, alias-checking the emitted order.
///
/// `write_output` is false for the final round: the records are proven checked
/// while scanning, so spilling a terminal run would only waste I/O.
async fn merge_group(group: Vec<RunFile>, write_output: bool) -> Result<Option<RunFile>> {
    let mut group = group;
    let mut readers: Vec<codec::RecordReader<'_>> = Vec::with_capacity(group.len());
    for run in group.iter_mut() {
        readers.push(codec::RecordReader::new(&mut run.file));
    }
    let mut heads: Vec<Option<SortRecord>> = Vec::with_capacity(readers.len());
    for reader in readers.iter_mut() {
        heads.push(reader.record().await?);
    }

    let mut writer = if write_output {
        let file = tokio::task::spawn_blocking(tempfile::tempfile)
            .await
            .map_err(|error| NamespacePreflightError::Worker(error.to_string()))??;
        Some(tokio::io::BufWriter::new(tokio::fs::File::from_std(file)))
    } else {
        None
    };

    let mut scanner = AliasScanner::new();
    loop {
        // Bounded fan-in keeps this linear scan over ≤ merge_fan_in heads cheap.
        let mut next: Option<usize> = None;
        for (index, head) in heads.iter().enumerate() {
            let Some(head) = head else {
                continue;
            };
            let select = match next {
                Some(best) => match heads[best].as_ref() {
                    Some(best_head) => head.cmp(best_head) == Ordering::Less,
                    None => true,
                },
                None => true,
            };
            if select {
                next = Some(index);
            }
        }
        let Some(index) = next else {
            break;
        };
        let Some(record) = heads[index].take() else {
            continue;
        };
        heads[index] = readers[index].record().await?;

        if let Some(emitted) = scanner.observe(record)? {
            if let Some(writer) = writer.as_mut() {
                let mut payload = Vec::new();
                emitted.encode(&mut payload);
                writer.write_all(&payload).await?;
            }
        }
    }
    scanner.finish()?;

    let Some(mut writer) = writer else {
        // Check-only round consumed the group for verification alone.
        return Ok(None);
    };
    writer.flush().await?;
    let mut file = writer.into_inner();
    file.seek(SeekFrom::Start(0)).await?;
    Ok(Some(RunFile { file }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rel(path: &str) -> RelativePath {
        RelativePath::new(path).unwrap()
    }

    fn insensitive() -> NamespaceSemantics {
        NamespaceSemantics::CASE_AND_NORMALIZATION_FOLDED
    }

    fn tiny_budget() -> SpillBudget {
        SpillBudget {
            run_bytes: 1,
            merge_fan_in: 2,
        }
    }

    async fn detect(
        semantics: NamespaceSemantics,
        budget: SpillBudget,
        paths: &[&str],
    ) -> Result<()> {
        let mut detector = NamespaceCollisionDetector::with_budget(semantics, budget);
        for path in paths {
            detector.record(&rel(path)).await?;
        }
        detector.finish().await
    }

    #[tokio::test]
    async fn case_folding_detects_collision() {
        let error = detect(
            insensitive(),
            tiny_budget(),
            &["foo/bar.txt", "FOO/BAR.TXT"],
        )
        .await
        .unwrap_err();
        let NamespacePreflightError::Collision(collision) = error else {
            panic!("expected definitive collision, got: {error}");
        };
        // Ancestor prefixes sort before deeper names, so the directory-level
        // alias is reported first; it is the root cause of the leaf alias.
        assert_eq!(collision.existing, rel("FOO"));
        assert_eq!(collision.colliding, rel("foo"));
    }

    #[tokio::test]
    async fn byte_exact_mode_skips_tracking() {
        detect(
            NamespaceSemantics::BYTE_EXACT,
            tiny_budget(),
            &["foo.txt", "FOO.TXT"],
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn unicode_normalization_detects_collision() {
        let nfc = "caf\u{e9}.txt";
        let nfd = "cafe\u{301}.txt";
        let error = detect(insensitive(), tiny_budget(), &[nfc, nfd])
            .await
            .unwrap_err();
        let NamespacePreflightError::Collision(collision) = error else {
            panic!("expected definitive collision, got: {error}");
        };
        // Reports in native byte order: the NFD spelling sorts first.
        assert_eq!(collision.existing, rel(nfd));
        assert_eq!(collision.colliding, rel(nfc));
    }

    #[tokio::test]
    async fn distinct_paths_do_not_collide() {
        detect(
            insensitive(),
            tiny_budget(),
            &["a/b.txt", "a/c.txt", "b/b.txt"],
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn exact_duplicates_do_not_collide() {
        detect(
            insensitive(),
            tiny_budget(),
            &["foo/bar.txt", "foo/bar.txt"],
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn parent_directory_case_collision_detected() {
        let error = detect(
            insensitive(),
            tiny_budget(),
            &["Dir/file1.txt", "dir/file2.txt"],
        )
        .await
        .unwrap_err();
        let NamespacePreflightError::Collision(collision) = error else {
            panic!("expected definitive collision, got: {error}");
        };
        assert_eq!(collision.existing, rel("Dir"));
        assert_eq!(collision.colliding, rel("dir"));
    }

    #[tokio::test]
    async fn component_delimiters_prevent_concatenation_collision() {
        detect(insensitive(), tiny_budget(), &["a/bc", "ab/c"])
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn unknown_semantics_report_ambiguity() {
        let error = detect(
            NamespaceSemantics::UNSPECIFIED,
            tiny_budget(),
            &["foo", "FOO"],
        )
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            NamespacePreflightError::Ambiguity(NamespaceAmbiguity { .. })
        ));
    }

    #[tokio::test]
    async fn unknown_normalization_still_proves_case_collisions() {
        // Case folding is known; only normalization is unknown. `foo`/`FOO`
        // collide under every interpretation (same strict key), while NFC/NFD
        // spellings are only a normalization ambiguity.
        let semantics = NamespaceSemantics {
            case: Folding::Folded,
            normalization: Folding::Unspecified,
        };
        let error = detect(semantics, tiny_budget(), &["foo", "FOO"])
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            NamespacePreflightError::Collision(NamespaceCollision { .. })
        ));

        let nfc = "caf\u{e9}.txt";
        let nfd = "cafe\u{301}.txt";
        let error = detect(semantics, tiny_budget(), &[nfc, nfd])
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            NamespacePreflightError::Ambiguity(NamespaceAmbiguity { .. })
        ));
    }

    #[tokio::test]
    async fn wide_tree_spills_and_merges_within_budget() {
        // 4096 names with one alias far from the others; tiny budgets force
        // many sorted runs and multi-round fan-in-2 merging.
        let mut paths: Vec<String> = (0..4095).map(|i| format!("dir-{i:04}/file")).collect();
        paths.push("DIR-4000/FILE".to_string());
        let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        let error = detect(insensitive(), tiny_budget(), &refs)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            NamespacePreflightError::Collision(NamespaceCollision { .. })
        ));
    }

    #[tokio::test]
    async fn wide_tree_without_aliases_survives_spilling() {
        let paths: Vec<String> = (0..4096)
            .map(|i| format!("dir-{i:04}/file-{i:04}"))
            .collect();
        let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        detect(insensitive(), tiny_budget(), &refs).await.unwrap();
    }

    #[tokio::test]
    async fn multi_run_aliases_are_detected_across_spill_boundaries() {
        // One record per run (run_bytes=1) and fan-in 2 force two merge rounds;
        // each alias pair meets only in the final cross-run merge.
        let paths = ["a-1", "b-1", "A-1", "B-1"];
        let error = detect(insensitive(), tiny_budget(), &paths)
            .await
            .unwrap_err();
        let NamespacePreflightError::Collision(collision) = error else {
            panic!("expected definitive collision, got: {error}");
        };
        assert_eq!(collision.existing, rel("A-1"));
        assert_eq!(collision.colliding, rel("a-1"));
    }

    #[tokio::test]
    async fn non_utf8_paths_round_trip_without_false_aliases() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let raw = RelativePath::new(std::path::PathBuf::from(OsString::from_vec(vec![
            b'a', 0xff, b'b',
        ])))
        .unwrap();
        let mut detector = NamespaceCollisionDetector::with_budget(insensitive(), tiny_budget());
        detector.record(&raw).await.unwrap();
        detector.record(&rel("c")).await.unwrap();
        detector.finish().await.unwrap();
    }
}
