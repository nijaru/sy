use crate::engine::bisync::ValueId;
use crate::engine::domain::EntryIdentity;
use std::io::{self, Read, Write};
use std::num::NonZeroU64;

const GENERATION_MAGIC: [u8; 8] = *b"SYBGEN01";
const POINTER_MAGIC: [u8; 8] = *b"SYBPTR01";
const RECOVERY_MAGIC: [u8; 8] = *b"SYBREC01";
const FORMAT_VERSION: u16 = 1;
const RESERVED: u16 = 0;
pub const MAX_NAMESPACE_KEY_BYTES: usize = 128 * 1024;
const MAX_RECORD_BYTES: usize = MAX_NAMESPACE_KEY_BYTES + 4 + 32 + 1 + 64;
const POINTER_BYTES: usize = 8 + 2 + 2 + 8 + 32 + 32;
const RECOVERY_BYTES: usize = 8 + 2 + 2 + 8 + 8 + 32;

#[derive(Debug, thiserror::Error)]
pub enum BisyncStateError {
    #[error(transparent)]
    Io(#[from] io::Error),

    #[error("invalid bisync state magic")]
    InvalidMagic,

    #[error("unsupported bisync state version {0}")]
    UnsupportedVersion(u16),

    #[error("bisync state reserved field is non-zero")]
    NonZeroReserved,

    #[error("bisync generation id must be non-zero")]
    ZeroGeneration,

    #[error("bisync generation id overflow")]
    GenerationOverflow,

    #[error("namespace key must be non-empty")]
    EmptyNamespaceKey,

    #[error("namespace key is too large: {actual} bytes (maximum {maximum})")]
    NamespaceKeyTooLarge { actual: usize, maximum: usize },

    #[error("bisync state record is too large: {actual} bytes (maximum {maximum})")]
    RecordTooLarge { actual: usize, maximum: usize },

    #[error("bisync state record has unsupported flags 0x{0:02x}")]
    UnsupportedRecordFlags(u8),

    #[error("bisync state record is truncated")]
    TruncatedRecord,

    #[error("bisync state record has trailing bytes")]
    RecordTrailingBytes,

    #[error("bisync generation records are not strictly ordered")]
    RecordOrder,

    #[error("bisync generation record count mismatch: trailer={trailer}, decoded={decoded}")]
    RecordCountMismatch { trailer: u64, decoded: u64 },

    #[error("bisync state checksum mismatch")]
    ChecksumMismatch,

    #[error("bisync state contains trailing bytes")]
    TrailingData,

    #[error("invalid fixed-size bisync state object length: expected {expected}, got {actual}")]
    FixedSize { expected: usize, actual: usize },
}

pub type Result<T> = std::result::Result<T, BisyncStateError>;

/// Opaque canonical path identity for one synchronized namespace.
///
/// The namespace-mapping layer owns how endpoint-native names become this key.
/// The state layer only requires a deterministic, collision-free byte ordering;
/// it never decodes the key as a host `PathBuf` or UTF-8 string.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NamespaceKey(Vec<u8>);

impl NamespaceKey {
    pub fn new(bytes: Vec<u8>) -> Result<Self> {
        if bytes.is_empty() {
            return Err(BisyncStateError::EmptyNamespaceKey);
        }
        if bytes.len() > MAX_NAMESPACE_KEY_BYTES {
            return Err(BisyncStateError::NamespaceKeyTooLarge {
                actual: bytes.len(),
                maximum: MAX_NAMESPACE_KEY_BYTES,
            });
        }
        Ok(Self(bytes))
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }
}

impl std::fmt::Debug for NamespaceKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NamespaceKey")
            .field("bytes", &self.0.len())
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GenerationId(NonZeroU64);

impl GenerationId {
    pub const FIRST: Self = Self(NonZeroU64::MIN);

    pub const fn new(value: u64) -> Option<Self> {
        match NonZeroU64::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    pub const fn get(self) -> u64 {
        self.0.get()
    }

    pub fn next(self) -> Result<Self> {
        let next = self
            .get()
            .checked_add(1)
            .ok_or(BisyncStateError::GenerationOverflow)?;
        Self::new(next).ok_or(BisyncStateError::GenerationOverflow)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PolicyFingerprint([u8; 32]);

impl PolicyFingerprint {
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaselineRecord {
    pub key: NamespaceKey,
    pub value: ValueId,
    pub left_identity: Option<EntryIdentity>,
    pub right_identity: Option<EntryIdentity>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GenerationHeader {
    pub generation: GenerationId,
    pub policy: PolicyFingerprint,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GenerationSummary {
    pub generation: GenerationId,
    pub record_count: u64,
    pub digest: [u8; 32],
}

/// Streaming writer for one immutable baseline generation.
///
/// Records must arrive in strict namespace-key order, matching the ordered scan
/// and reconciliation pipeline. Memory remains bounded by one previous key.
pub struct GenerationWriter<W> {
    writer: W,
    hasher: blake3::Hasher,
    header: GenerationHeader,
    previous_key: Option<NamespaceKey>,
    record_count: u64,
}

impl<W: Write> GenerationWriter<W> {
    pub fn new(writer: W, header: GenerationHeader) -> Result<Self> {
        let mut state = Self {
            writer,
            hasher: blake3::Hasher::new(),
            header,
            previous_key: None,
            record_count: 0,
        };
        state.write_hashed(&GENERATION_MAGIC)?;
        state.write_hashed(&FORMAT_VERSION.to_be_bytes())?;
        state.write_hashed(&RESERVED.to_be_bytes())?;
        state.write_hashed(&header.generation.get().to_be_bytes())?;
        state.write_hashed(header.policy.as_bytes())?;
        Ok(state)
    }

    pub fn write_record(&mut self, record: &BaselineRecord) -> Result<()> {
        if self
            .previous_key
            .as_ref()
            .is_some_and(|previous| previous >= &record.key)
        {
            return Err(BisyncStateError::RecordOrder);
        }

        let flags = u8::from(record.left_identity.is_some())
            | (u8::from(record.right_identity.is_some()) << 1);
        let record_len = 4_usize
            .checked_add(record.key.as_bytes().len())
            .and_then(|length| length.checked_add(32 + 1))
            .and_then(|length| length.checked_add(32 * usize::from(record.left_identity.is_some())))
            .and_then(|length| {
                length.checked_add(32 * usize::from(record.right_identity.is_some()))
            })
            .ok_or(BisyncStateError::RecordTooLarge {
                actual: usize::MAX,
                maximum: MAX_RECORD_BYTES,
            })?;
        if record_len > MAX_RECORD_BYTES {
            return Err(BisyncStateError::RecordTooLarge {
                actual: record_len,
                maximum: MAX_RECORD_BYTES,
            });
        }

        let record_len_u32 =
            u32::try_from(record_len).map_err(|_| BisyncStateError::RecordTooLarge {
                actual: record_len,
                maximum: MAX_RECORD_BYTES,
            })?;
        let key_len = u32::try_from(record.key.as_bytes().len()).map_err(|_| {
            BisyncStateError::NamespaceKeyTooLarge {
                actual: record.key.as_bytes().len(),
                maximum: MAX_NAMESPACE_KEY_BYTES,
            }
        })?;

        self.write_hashed(&record_len_u32.to_be_bytes())?;
        self.write_hashed(&key_len.to_be_bytes())?;
        self.write_hashed(record.key.as_bytes())?;
        self.write_hashed(record.value.as_bytes())?;
        self.write_hashed(&[flags])?;
        if let Some(identity) = record.left_identity {
            self.write_hashed(identity.as_bytes())?;
        }
        if let Some(identity) = record.right_identity {
            self.write_hashed(identity.as_bytes())?;
        }

        self.previous_key = Some(record.key.clone());
        self.record_count = self
            .record_count
            .checked_add(1)
            .ok_or(BisyncStateError::GenerationOverflow)?;
        Ok(())
    }

    pub fn finish(mut self) -> Result<(W, GenerationSummary)> {
        self.write_hashed(&0_u32.to_be_bytes())?;
        self.write_hashed(&self.record_count.to_be_bytes())?;
        let digest = *self.hasher.finalize().as_bytes();
        self.writer.write_all(&digest)?;
        self.writer.flush()?;
        let summary = GenerationSummary {
            generation: self.header.generation,
            record_count: self.record_count,
            digest,
        };
        Ok((self.writer, summary))
    }

    fn write_hashed(&mut self, bytes: &[u8]) -> Result<()> {
        self.writer.write_all(bytes)?;
        self.hasher.update(bytes);
        Ok(())
    }
}

/// Streaming reader that validates the complete immutable generation before
/// reporting end-of-stream. A caller must consume through `None`; abandoning a
/// reader early intentionally does not imply that the generation was trusted.
pub struct GenerationReader<R> {
    reader: R,
    hasher: blake3::Hasher,
    header: GenerationHeader,
    previous_key: Option<NamespaceKey>,
    record_count: u64,
    verified: Option<GenerationSummary>,
}

impl<R: Read> GenerationReader<R> {
    pub fn new(reader: R) -> Result<Self> {
        let mut state = Self {
            reader,
            hasher: blake3::Hasher::new(),
            header: GenerationHeader {
                generation: GenerationId::FIRST,
                policy: PolicyFingerprint::from_bytes([0; 32]),
            },
            previous_key: None,
            record_count: 0,
            verified: None,
        };

        let magic = state.read_hashed_array::<8>()?;
        if magic != GENERATION_MAGIC {
            return Err(BisyncStateError::InvalidMagic);
        }
        let version = u16::from_be_bytes(state.read_hashed_array()?);
        if version != FORMAT_VERSION {
            return Err(BisyncStateError::UnsupportedVersion(version));
        }
        let reserved = u16::from_be_bytes(state.read_hashed_array()?);
        if reserved != RESERVED {
            return Err(BisyncStateError::NonZeroReserved);
        }
        let generation_raw = u64::from_be_bytes(state.read_hashed_array()?);
        let generation =
            GenerationId::new(generation_raw).ok_or(BisyncStateError::ZeroGeneration)?;
        let policy = PolicyFingerprint::from_bytes(state.read_hashed_array()?);
        state.header = GenerationHeader { generation, policy };
        Ok(state)
    }

    pub const fn header(&self) -> GenerationHeader {
        self.header
    }

    pub const fn verified_summary(&self) -> Option<GenerationSummary> {
        self.verified
    }

    pub fn next_record(&mut self) -> Result<Option<BaselineRecord>> {
        if self.verified.is_some() {
            return Ok(None);
        }

        let record_len = u32::from_be_bytes(self.read_hashed_array()?) as usize;
        if record_len == 0 {
            self.finish_generation()?;
            return Ok(None);
        }
        if record_len > MAX_RECORD_BYTES {
            return Err(BisyncStateError::RecordTooLarge {
                actual: record_len,
                maximum: MAX_RECORD_BYTES,
            });
        }

        let mut payload = vec![0_u8; record_len];
        self.reader.read_exact(&mut payload)?;
        self.hasher.update(&payload);
        let record = decode_record(&payload)?;
        if self
            .previous_key
            .as_ref()
            .is_some_and(|previous| previous >= &record.key)
        {
            return Err(BisyncStateError::RecordOrder);
        }
        self.previous_key = Some(record.key.clone());
        self.record_count = self
            .record_count
            .checked_add(1)
            .ok_or(BisyncStateError::GenerationOverflow)?;
        Ok(Some(record))
    }

    fn finish_generation(&mut self) -> Result<()> {
        let trailer_count = u64::from_be_bytes(self.read_hashed_array()?);
        if trailer_count != self.record_count {
            return Err(BisyncStateError::RecordCountMismatch {
                trailer: trailer_count,
                decoded: self.record_count,
            });
        }

        let expected = read_array::<32, _>(&mut self.reader)?;
        let actual = *self.hasher.finalize().as_bytes();
        if expected != actual {
            return Err(BisyncStateError::ChecksumMismatch);
        }

        let mut trailing = [0_u8; 1];
        if self.reader.read(&mut trailing)? != 0 {
            return Err(BisyncStateError::TrailingData);
        }
        self.verified = Some(GenerationSummary {
            generation: self.header.generation,
            record_count: self.record_count,
            digest: actual,
        });
        Ok(())
    }

    fn read_hashed_array<const N: usize>(&mut self) -> Result<[u8; N]> {
        let bytes = read_array::<N, _>(&mut self.reader)?;
        self.hasher.update(&bytes);
        Ok(bytes)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CurrentPointer {
    pub generation: GenerationId,
    pub generation_digest: [u8; 32],
}

impl CurrentPointer {
    pub fn encode(self) -> [u8; POINTER_BYTES] {
        let mut bytes = [0_u8; POINTER_BYTES];
        let mut cursor = 0;
        put(&mut bytes, &mut cursor, &POINTER_MAGIC);
        put(&mut bytes, &mut cursor, &FORMAT_VERSION.to_be_bytes());
        put(&mut bytes, &mut cursor, &RESERVED.to_be_bytes());
        put(
            &mut bytes,
            &mut cursor,
            &self.generation.get().to_be_bytes(),
        );
        put(&mut bytes, &mut cursor, &self.generation_digest);
        let checksum = blake3::hash(&bytes[..cursor]);
        put(&mut bytes, &mut cursor, checksum.as_bytes());
        debug_assert_eq!(cursor, POINTER_BYTES);
        bytes
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != POINTER_BYTES {
            return Err(BisyncStateError::FixedSize {
                expected: POINTER_BYTES,
                actual: bytes.len(),
            });
        }
        let payload_len = POINTER_BYTES - 32;
        if blake3::hash(&bytes[..payload_len]).as_bytes() != &bytes[payload_len..] {
            return Err(BisyncStateError::ChecksumMismatch);
        }
        let mut cursor = SliceCursor::new(&bytes[..payload_len]);
        if cursor.array::<8>()? != POINTER_MAGIC {
            return Err(BisyncStateError::InvalidMagic);
        }
        validate_version_and_reserved(&mut cursor)?;
        let generation =
            GenerationId::new(cursor.u64()?).ok_or(BisyncStateError::ZeroGeneration)?;
        let generation_digest = cursor.array()?;
        cursor.finish()?;
        Ok(Self {
            generation,
            generation_digest,
        })
    }
}

/// Durable marker written before any bisync mutation begins.
///
/// `base_generation` is the trusted generation the run started from. `target`
/// is chosen before execution. If a crash leaves this marker behind, the next
/// run must prove whether the target pointer committed before continuing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecoveryMarker {
    pub base_generation: Option<GenerationId>,
    pub target_generation: GenerationId,
}

impl RecoveryMarker {
    pub fn encode(self) -> [u8; RECOVERY_BYTES] {
        let mut bytes = [0_u8; RECOVERY_BYTES];
        let mut cursor = 0;
        put(&mut bytes, &mut cursor, &RECOVERY_MAGIC);
        put(&mut bytes, &mut cursor, &FORMAT_VERSION.to_be_bytes());
        put(&mut bytes, &mut cursor, &RESERVED.to_be_bytes());
        put(
            &mut bytes,
            &mut cursor,
            &self
                .base_generation
                .map_or(0, GenerationId::get)
                .to_be_bytes(),
        );
        put(
            &mut bytes,
            &mut cursor,
            &self.target_generation.get().to_be_bytes(),
        );
        let checksum = blake3::hash(&bytes[..cursor]);
        put(&mut bytes, &mut cursor, checksum.as_bytes());
        debug_assert_eq!(cursor, RECOVERY_BYTES);
        bytes
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != RECOVERY_BYTES {
            return Err(BisyncStateError::FixedSize {
                expected: RECOVERY_BYTES,
                actual: bytes.len(),
            });
        }
        let payload_len = RECOVERY_BYTES - 32;
        if blake3::hash(&bytes[..payload_len]).as_bytes() != &bytes[payload_len..] {
            return Err(BisyncStateError::ChecksumMismatch);
        }
        let mut cursor = SliceCursor::new(&bytes[..payload_len]);
        if cursor.array::<8>()? != RECOVERY_MAGIC {
            return Err(BisyncStateError::InvalidMagic);
        }
        validate_version_and_reserved(&mut cursor)?;
        let base_raw = cursor.u64()?;
        let base_generation = GenerationId::new(base_raw);
        let target_generation =
            GenerationId::new(cursor.u64()?).ok_or(BisyncStateError::ZeroGeneration)?;
        cursor.finish()?;
        Ok(Self {
            base_generation,
            target_generation,
        })
    }
}

fn decode_record(payload: &[u8]) -> Result<BaselineRecord> {
    let mut cursor = SliceCursor::new(payload);
    let key_len = usize::try_from(cursor.u32()?).map_err(|_| BisyncStateError::RecordTooLarge {
        actual: usize::MAX,
        maximum: MAX_RECORD_BYTES,
    })?;
    if key_len > MAX_NAMESPACE_KEY_BYTES {
        return Err(BisyncStateError::NamespaceKeyTooLarge {
            actual: key_len,
            maximum: MAX_NAMESPACE_KEY_BYTES,
        });
    }
    let key = NamespaceKey::new(cursor.take(key_len)?.to_vec())?;
    let value = ValueId::from_bytes(cursor.array()?);
    let flags = cursor.u8()?;
    if flags & !0b11 != 0 {
        return Err(BisyncStateError::UnsupportedRecordFlags(flags));
    }
    let left_identity = if flags & 0b01 != 0 {
        Some(EntryIdentity::from_bytes(cursor.array()?))
    } else {
        None
    };
    let right_identity = if flags & 0b10 != 0 {
        Some(EntryIdentity::from_bytes(cursor.array()?))
    } else {
        None
    };
    cursor.finish()?;
    Ok(BaselineRecord {
        key,
        value,
        left_identity,
        right_identity,
    })
}

fn validate_version_and_reserved(cursor: &mut SliceCursor<'_>) -> Result<()> {
    let version = cursor.u16()?;
    if version != FORMAT_VERSION {
        return Err(BisyncStateError::UnsupportedVersion(version));
    }
    let reserved = cursor.u16()?;
    if reserved != RESERVED {
        return Err(BisyncStateError::NonZeroReserved);
    }
    Ok(())
}

fn read_array<const N: usize, R: Read>(reader: &mut R) -> Result<[u8; N]> {
    let mut bytes = [0_u8; N];
    reader.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn put<const N: usize>(destination: &mut [u8], cursor: &mut usize, source: &[u8; N]) {
    let end = *cursor + N;
    destination[*cursor..end].copy_from_slice(source);
    *cursor = end;
}

struct SliceCursor<'a> {
    bytes: &'a [u8],
    cursor: usize,
}

impl<'a> SliceCursor<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, cursor: 0 }
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8]> {
        let end = self
            .cursor
            .checked_add(count)
            .ok_or(BisyncStateError::TruncatedRecord)?;
        let result = self
            .bytes
            .get(self.cursor..end)
            .ok_or(BisyncStateError::TruncatedRecord)?;
        self.cursor = end;
        Ok(result)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        self.take(N)?
            .try_into()
            .map_err(|_| BisyncStateError::TruncatedRecord)
    }

    fn u8(&mut self) -> Result<u8> {
        self.take(1)?
            .first()
            .copied()
            .ok_or(BisyncStateError::TruncatedRecord)
    }

    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_be_bytes(self.array()?))
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.array()?))
    }

    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_be_bytes(self.array()?))
    }

    fn finish(self) -> Result<()> {
        if self.cursor == self.bytes.len() {
            Ok(())
        } else {
            Err(BisyncStateError::RecordTrailingBytes)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    const POLICY: PolicyFingerprint = PolicyFingerprint::from_bytes([7; 32]);
    const VALUE_A: ValueId = ValueId::from_bytes([1; 32]);
    const VALUE_B: ValueId = ValueId::from_bytes([2; 32]);
    const LEFT_ID: EntryIdentity = EntryIdentity::from_bytes([3; 32]);
    const RIGHT_ID: EntryIdentity = EntryIdentity::from_bytes([4; 32]);

    fn record(key: &[u8], value: ValueId) -> BaselineRecord {
        BaselineRecord {
            key: NamespaceKey::new(key.to_vec()).unwrap(),
            value,
            left_identity: Some(LEFT_ID),
            right_identity: Some(RIGHT_ID),
        }
    }

    #[test]
    fn generation_round_trip_is_streaming_and_verified() {
        let header = GenerationHeader {
            generation: GenerationId::FIRST,
            policy: POLICY,
        };
        let mut writer = GenerationWriter::new(Vec::new(), header).unwrap();
        writer.write_record(&record(b"a", VALUE_A)).unwrap();
        writer.write_record(&record(b"b", VALUE_B)).unwrap();
        let (bytes, written) = writer.finish().unwrap();

        let mut reader = GenerationReader::new(Cursor::new(bytes)).unwrap();
        assert_eq!(reader.header(), header);
        assert_eq!(reader.next_record().unwrap(), Some(record(b"a", VALUE_A)));
        assert_eq!(reader.next_record().unwrap(), Some(record(b"b", VALUE_B)));
        assert_eq!(reader.next_record().unwrap(), None);
        assert_eq!(reader.verified_summary(), Some(written));
    }

    #[test]
    fn writer_rejects_duplicate_or_out_of_order_keys() {
        let header = GenerationHeader {
            generation: GenerationId::FIRST,
            policy: POLICY,
        };
        let mut writer = GenerationWriter::new(Vec::new(), header).unwrap();
        writer.write_record(&record(b"b", VALUE_A)).unwrap();
        assert!(matches!(
            writer.write_record(&record(b"a", VALUE_B)),
            Err(BisyncStateError::RecordOrder)
        ));
    }

    #[test]
    fn generation_corruption_is_detected_at_end_of_stream() {
        let header = GenerationHeader {
            generation: GenerationId::FIRST,
            policy: POLICY,
        };
        let mut writer = GenerationWriter::new(Vec::new(), header).unwrap();
        writer.write_record(&record(b"path", VALUE_A)).unwrap();
        let (mut bytes, _) = writer.finish().unwrap();
        let index = bytes.len() - 1;
        bytes[index] ^= 1;

        let mut reader = GenerationReader::new(Cursor::new(bytes)).unwrap();
        assert!(reader.next_record().unwrap().is_some());
        assert!(matches!(
            reader.next_record(),
            Err(BisyncStateError::ChecksumMismatch)
        ));
        assert_eq!(reader.verified_summary(), None);
    }

    #[test]
    fn oversized_record_is_rejected_before_allocation() {
        let header = GenerationHeader {
            generation: GenerationId::FIRST,
            policy: POLICY,
        };
        let (mut bytes, _) = GenerationWriter::new(Vec::new(), header)
            .unwrap()
            .finish()
            .unwrap();
        let header_bytes = 8 + 2 + 2 + 8 + 32;
        bytes[header_bytes..header_bytes + 4]
            .copy_from_slice(&(u32::try_from(MAX_RECORD_BYTES).unwrap() + 1).to_be_bytes());

        let mut reader = GenerationReader::new(Cursor::new(bytes)).unwrap();
        assert!(matches!(
            reader.next_record(),
            Err(BisyncStateError::RecordTooLarge { .. })
        ));
    }

    #[test]
    fn current_pointer_round_trip_and_checksum() {
        let pointer = CurrentPointer {
            generation: GenerationId::FIRST,
            generation_digest: [9; 32],
        };
        let mut bytes = pointer.encode();
        assert_eq!(CurrentPointer::decode(&bytes).unwrap(), pointer);
        bytes[20] ^= 1;
        assert!(matches!(
            CurrentPointer::decode(&bytes),
            Err(BisyncStateError::ChecksumMismatch)
        ));
    }

    #[test]
    fn recovery_marker_represents_initial_and_existing_baselines() {
        let initial = RecoveryMarker {
            base_generation: None,
            target_generation: GenerationId::FIRST,
        };
        assert_eq!(RecoveryMarker::decode(&initial.encode()).unwrap(), initial);

        let existing = RecoveryMarker {
            base_generation: Some(GenerationId::FIRST),
            target_generation: GenerationId::FIRST.next().unwrap(),
        };
        assert_eq!(
            RecoveryMarker::decode(&existing.encode()).unwrap(),
            existing
        );
    }

    #[test]
    fn namespace_key_is_opaque_and_bounded() {
        let raw = vec![0, 0xff, b'/', 0, b'\\'];
        let key = NamespaceKey::new(raw.clone()).unwrap();
        assert_eq!(key.as_bytes(), raw);
        assert!(matches!(
            NamespaceKey::new(Vec::new()),
            Err(BisyncStateError::EmptyNamespaceKey)
        ));
        assert!(matches!(
            NamespaceKey::new(vec![0; MAX_NAMESPACE_KEY_BYTES + 1]),
            Err(BisyncStateError::NamespaceKeyTooLarge { .. })
        ));
    }
}
