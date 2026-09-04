use super::{ProtocolError, Result};
use bitflags::bitflags;
use bytes::Bytes;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const HEADER_LEN: usize = 12;
pub const MAX_FRAME_PAYLOAD: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StreamId(u32);

impl StreamId {
    pub const CONTROL: Self = Self(0);
    pub const fn new(value: u32) -> Self {
        Self(value)
    }
    pub const fn get(self) -> u32 {
        self.0
    }
    pub const fn is_control(self) -> bool {
        self.0 == 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameKind {
    ClientHello = 1,
    ServerHello = 2,
    SessionOpen = 3,
    SessionReady = 4,
    Entry = 5,
    EntryEnd = 6,
    SignatureRequest = 7,
    Signature = 8,
    SignatureEnd = 9,
    FileBegin = 10,
    Data = 11,
    DeltaCopy = 12,
    FileEnd = 13,
    Metadata = 14,
    Ack = 15,
    Error = 16,
    Cancel = 17,
    Ping = 18,
    Pong = 19,
    ScanRequest = 20,
    Mutation = 21,
    HashRequest = 22,
    HashResult = 23,
}

impl TryFrom<u8> for FrameKind {
    type Error = ProtocolError;
    fn try_from(value: u8) -> Result<Self> {
        match value {
            1 => Ok(Self::ClientHello),
            2 => Ok(Self::ServerHello),
            3 => Ok(Self::SessionOpen),
            4 => Ok(Self::SessionReady),
            5 => Ok(Self::Entry),
            6 => Ok(Self::EntryEnd),
            7 => Ok(Self::SignatureRequest),
            8 => Ok(Self::Signature),
            9 => Ok(Self::SignatureEnd),
            10 => Ok(Self::FileBegin),
            11 => Ok(Self::Data),
            12 => Ok(Self::DeltaCopy),
            13 => Ok(Self::FileEnd),
            14 => Ok(Self::Metadata),
            15 => Ok(Self::Ack),
            16 => Ok(Self::Error),
            17 => Ok(Self::Cancel),
            18 => Ok(Self::Ping),
            19 => Ok(Self::Pong),
            20 => Ok(Self::ScanRequest),
            21 => Ok(Self::Mutation),
            22 => Ok(Self::HashRequest),
            23 => Ok(Self::HashResult),
            other => Err(ProtocolError::UnknownFrameKind(other)),
        }
    }
}

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct FrameFlags: u8 {
        const COMPRESSED = 1 << 0;
        const FINAL = 1 << 1;
        const ACK_REQUIRED = 1 << 2;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    kind: FrameKind,
    flags: FrameFlags,
    stream_id: StreamId,
    payload: Bytes,
}

impl Frame {
    pub fn new(
        kind: FrameKind,
        flags: FrameFlags,
        stream_id: StreamId,
        payload: impl Into<Bytes>,
    ) -> Result<Self> {
        let payload = payload.into();
        validate_payload_len(payload.len())?;
        Ok(Self {
            kind,
            flags,
            stream_id,
            payload,
        })
    }
    pub fn control(kind: FrameKind, payload: impl Into<Bytes>) -> Result<Self> {
        Self::new(kind, FrameFlags::empty(), StreamId::CONTROL, payload)
    }
    pub const fn kind(&self) -> FrameKind {
        self.kind
    }
    pub const fn flags(&self) -> FrameFlags {
        self.flags
    }
    pub const fn stream_id(&self) -> StreamId {
        self.stream_id
    }
    pub fn payload(&self) -> &Bytes {
        &self.payload
    }
    pub fn into_payload(self) -> Bytes {
        self.payload
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FrameHeader {
    payload_len: usize,
    kind: FrameKind,
    flags: FrameFlags,
    stream_id: StreamId,
}

impl FrameHeader {
    fn decode(bytes: [u8; HEADER_LEN]) -> Result<Self> {
        let payload_len = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        let payload_len = usize::try_from(payload_len)
            .map_err(|_| ProtocolError::InvalidMessage("payload length does not fit usize"))?;
        validate_payload_len(payload_len)?;
        let kind = FrameKind::try_from(bytes[4])?;
        let flags =
            FrameFlags::from_bits(bytes[5]).ok_or(ProtocolError::UnknownFrameFlags(bytes[5]))?;
        let reserved = u16::from_be_bytes([bytes[6], bytes[7]]);
        if reserved != 0 {
            return Err(ProtocolError::NonZeroReserved(reserved));
        }
        let stream_id = StreamId::new(u32::from_be_bytes([
            bytes[8], bytes[9], bytes[10], bytes[11],
        ]));
        Ok(Self {
            payload_len,
            kind,
            flags,
            stream_id,
        })
    }
    fn encode(self) -> Result<[u8; HEADER_LEN]> {
        validate_payload_len(self.payload_len)?;
        let payload_len = u32::try_from(self.payload_len)
            .map_err(|_| ProtocolError::InvalidMessage("payload length exceeds u32"))?;
        let mut header = [0_u8; HEADER_LEN];
        header[0..4].copy_from_slice(&payload_len.to_be_bytes());
        header[4] = self.kind as u8;
        header[5] = self.flags.bits();
        header[8..12].copy_from_slice(&self.stream_id.get().to_be_bytes());
        Ok(header)
    }
}

fn validate_payload_len(len: usize) -> Result<()> {
    if len > MAX_FRAME_PAYLOAD {
        return Err(ProtocolError::PayloadTooLarge {
            len,
            max: MAX_FRAME_PAYLOAD,
        });
    }
    Ok(())
}

/// Result of reading a frame, distinguishing a clean end of stream at a
/// frame boundary from truncation in the middle of a frame.
#[derive(Debug)]
pub enum ReadFrame {
    /// A complete frame was read.
    Frame(Frame),
    /// The reader reached end of stream exactly between two frames: no
    /// protocol bytes were lost. A peer closing stdin/stdout after its
    /// last acknowledgement must not be reported as a transport failure.
    CleanEof,
}

pub async fn read_frame<R>(reader: &mut R) -> Result<Frame>
where
    R: AsyncRead + Unpin,
{
    match read_frame_or_eof(reader).await? {
        ReadFrame::Frame(frame) => Ok(frame),
        ReadFrame::CleanEof => Err(ProtocolError::CleanEof),
    }
}

/// Read one frame, reporting a clean end of stream instead of an I/O error
/// when the transport closes exactly at a frame boundary. Mid-frame EOF
/// remains an `Io(UnexpectedEof)` error: bytes were promised by a header and
/// never delivered.
pub async fn read_frame_or_eof<R>(reader: &mut R) -> Result<ReadFrame>
where
    R: AsyncRead + Unpin,
{
    let mut header = [0_u8; HEADER_LEN];
    match read_exact_or_eof(reader, &mut header).await? {
        EofAt::Clean => return Ok(ReadFrame::CleanEof),
        EofAt::Byte(byte) => {
            let error = std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!("connection closed mid-header after {byte} of {HEADER_LEN} bytes"),
            );
            return Err(ProtocolError::Io(error));
        }
        EofAt::Complete => {}
    }
    let header = FrameHeader::decode(header)?;
    let mut payload = vec![0_u8; header.payload_len];
    reader.read_exact(&mut payload).await?;
    Ok(ReadFrame::Frame(Frame::new(
        header.kind,
        header.flags,
        header.stream_id,
        Bytes::from(payload),
    )?))
}

/// How much of a fixed-length read completed before the stream ended.
enum EofAt {
    Clean,
    Byte(usize),
    Complete,
}

async fn read_exact_or_eof<R>(reader: &mut R, buf: &mut [u8]) -> Result<EofAt>
where
    R: AsyncRead + Unpin,
{
    // An empty read buffer cannot distinguish clean from truncated EOF;
    // callers with zero-length headers do not exist in this protocol.
    debug_assert!(!buf.is_empty());
    let mut filled = 0;
    while filled < buf.len() {
        let n = reader.read(&mut buf[filled..]).await?;
        if n == 0 {
            return Ok(if filled == 0 {
                EofAt::Clean
            } else {
                EofAt::Byte(filled)
            });
        }
        filled += n;
    }
    Ok(EofAt::Complete)
}

pub async fn write_frame<W>(writer: &mut W, frame: &Frame) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let header = FrameHeader {
        payload_len: frame.payload.len(),
        kind: frame.kind,
        flags: frame.flags,
        stream_id: frame.stream_id,
    }
    .encode()?;
    writer.write_all(&header).await?;
    writer.write_all(&frame.payload).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[tokio::test]
    async fn frame_round_trip() {
        let (mut writer, mut reader) = tokio::io::duplex(4096);
        let frame = Frame::new(
            FrameKind::Data,
            FrameFlags::FINAL,
            StreamId::new(42),
            Bytes::from_static(b"payload"),
        )
        .unwrap();
        write_frame(&mut writer, &frame).await.unwrap();
        let decoded = read_frame(&mut reader).await.unwrap();
        assert_eq!(decoded, frame);
    }

    #[tokio::test]
    async fn rejects_oversized_payload_before_reading_body() {
        let (mut writer, mut reader) = tokio::io::duplex(HEADER_LEN);
        let mut header = [0_u8; HEADER_LEN];
        let len = u32::try_from(MAX_FRAME_PAYLOAD + 1).unwrap();
        header[0..4].copy_from_slice(&len.to_be_bytes());
        header[4] = FrameKind::Data as u8;
        writer.write_all(&header).await.unwrap();
        let error = read_frame(&mut reader).await.unwrap_err();
        assert!(matches!(error, ProtocolError::PayloadTooLarge { .. }));
    }

    #[test]
    fn rejects_unknown_kind_flags_and_reserved_bits() {
        let mut header = [0_u8; HEADER_LEN];
        header[4] = u8::MAX;
        assert!(matches!(
            FrameHeader::decode(header),
            Err(ProtocolError::UnknownFrameKind(u8::MAX))
        ));
        header[4] = FrameKind::Data as u8;
        header[5] = 0x80;
        assert!(matches!(
            FrameHeader::decode(header),
            Err(ProtocolError::UnknownFrameFlags(0x80))
        ));
        header[5] = 0;
        header[6..8].copy_from_slice(&1_u16.to_be_bytes());
        assert!(matches!(
            FrameHeader::decode(header),
            Err(ProtocolError::NonZeroReserved(1))
        ));
    }

    #[test]
    fn constructor_rejects_oversized_payload() {
        let payload = vec![0_u8; MAX_FRAME_PAYLOAD + 1];
        assert!(matches!(
            Frame::control(FrameKind::Data, payload),
            Err(ProtocolError::PayloadTooLarge { .. })
        ));
    }

    #[test]
    fn session_control_kinds_are_reserved_before_data_plane() {
        assert_eq!(FrameKind::SessionOpen as u8, 3);
        assert_eq!(FrameKind::SessionReady as u8, 4);
        assert_eq!(FrameKind::Entry as u8, 5);
        assert_eq!(FrameKind::ScanRequest as u8, 20);
        assert_eq!(FrameKind::Mutation as u8, 21);
        assert_eq!(FrameKind::HashRequest as u8, 22);
        assert_eq!(FrameKind::HashResult as u8, 23);
    }

    proptest! {
        #[test]
        fn arbitrary_headers_never_panic(header in any::<[u8; HEADER_LEN]>()) { let _ = FrameHeader::decode(header); }
    }
}

#[tokio::test]
async fn clean_eof_between_frames_is_reported_as_clean_eof() {
    let (mut writer, mut reader) = tokio::io::duplex(64);
    let frame = Frame::new(
        FrameKind::Data,
        FrameFlags::empty(),
        StreamId::CONTROL,
        Bytes::from_static(b"x"),
    )
    .unwrap();
    write_frame(&mut writer, &frame).await.unwrap();
    drop(writer);

    assert!(matches!(
        read_frame_or_eof(&mut reader).await.unwrap(),
        ReadFrame::Frame(_)
    ));
    assert!(matches!(
        read_frame_or_eof(&mut reader).await.unwrap(),
        ReadFrame::CleanEof
    ));
}

#[tokio::test]
async fn eof_mid_header_is_truncation_not_clean_eof() {
    let (mut writer, mut reader) = tokio::io::duplex(HEADER_LEN);
    // Partial header: 5 of HEADER_LEN bytes, then the peer closes.
    writer.write_all(&[0_u8; 5]).await.unwrap();
    drop(writer);
    let error = read_frame_or_eof(&mut reader).await.unwrap_err();
    assert!(
        error.to_string().contains("mid-header"),
        "mid-header EOF must be a loud truncation error, got: {error}"
    );
}

#[tokio::test]
async fn eof_mid_payload_is_truncation_not_clean_eof() {
    let (mut writer, mut reader) = tokio::io::duplex(HEADER_LEN + 8);
    let frame = Frame::new(
        FrameKind::Data,
        FrameFlags::empty(),
        StreamId::CONTROL,
        Bytes::from_static(b"eight-bytes-payload!"),
    )
    .unwrap();
    // Write the full header but only part of the promised payload before
    // closing: the header says 19 payload bytes, only 8 ever arrive.
    let header = FrameHeader {
        payload_len: frame.payload.len(),
        kind: frame.kind,
        flags: frame.flags,
        stream_id: frame.stream_id,
    }
    .encode()
    .unwrap();
    writer.write_all(&header).await.unwrap();
    writer.write_all(&frame.payload[..8]).await.unwrap();
    drop(writer);
    let error = read_frame_or_eof(&mut reader).await.unwrap_err();
    let text = error.to_string();
    assert!(
        text.contains("I/O"),
        "mid-payload EOF must surface as an I/O truncation error, got: {text}"
    );
}
