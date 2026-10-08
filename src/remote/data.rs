//! One bounded Data-frame decoder for push reconstruction and pull fetching.

use crate::protocol::{Frame, FrameFlags, FrameKind, WireData, MAX_TRANSFER_DATA_SIZE};
use crate::remote::transfer::{RemoteTransferError, Result};
use bytes::Bytes;

pub(super) enum DataChunk {
    Plain(Bytes),
    Zstd(Bytes),
}

impl DataChunk {
    pub(super) fn from_frame(frame: &Frame) -> Result<Self> {
        if frame.kind() != FrameKind::Data {
            return Err(RemoteTransferError::UnexpectedFrame {
                expected: FrameKind::Data,
                actual: frame.kind(),
            });
        }
        let bytes = WireData::new(frame.payload().clone())?.into_bytes();
        match frame.flags() {
            flags if flags.is_empty() => Ok(Self::Plain(bytes)),
            FrameFlags::COMPRESSED => Ok(Self::Zstd(bytes)),
            flags => Err(RemoteTransferError::FrameFlags {
                kind: frame.kind(),
                flags: flags.bits(),
            }),
        }
    }

    /// Run codec work on a blocking worker, never on a Tokio worker thread.
    pub(super) fn decode_blocking(self) -> Result<Bytes> {
        match self {
            Self::Plain(bytes) => Ok(bytes),
            Self::Zstd(bytes) => {
                // The wire cap bounds encoded bytes, not decompressed bytes.
                // Bound the output allocation before invoking the native codec;
                // checking the file size after decode_all would be too late.
                let decoded = zstd::bulk::decompress(&bytes, MAX_TRANSFER_DATA_SIZE)
                    .map_err(|error| RemoteTransferError::Decompression(error.to_string()))?;
                Ok(WireData::new(decoded)?.into_bytes())
            }
        }
    }

    pub(super) async fn decode(self) -> Result<Bytes> {
        match self {
            Self::Plain(bytes) => Ok(bytes),
            compressed => tokio::task::spawn_blocking(move || compressed.decode_blocking())
                .await
                .map_err(|error| RemoteTransferError::Decompression(error.to_string()))?,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::StreamId;

    fn compressed_frame(data: &[u8]) -> Frame {
        let encoded = zstd::bulk::compress(data, -5).unwrap();
        assert!(encoded.len() <= MAX_TRANSFER_DATA_SIZE);
        Frame::new(
            FrameKind::Data,
            FrameFlags::COMPRESSED,
            StreamId::new(1),
            encoded,
        )
        .unwrap()
    }

    #[test]
    fn decoded_chunks_are_bounded_and_nonempty() {
        for size in [0, 1, MAX_TRANSFER_DATA_SIZE, MAX_TRANSFER_DATA_SIZE + 1] {
            let source = vec![b'x'; size];
            let frame = compressed_frame(&source);
            let decoded = DataChunk::from_frame(&frame).unwrap().decode_blocking();
            if (1..=MAX_TRANSFER_DATA_SIZE).contains(&size) {
                assert_eq!(decoded.unwrap().as_ref(), source);
            } else {
                assert!(
                    decoded.is_err(),
                    "decoded {size} bytes outside the chunk limit"
                );
            }
        }
    }

    #[test]
    fn compressed_frames_cannot_expand_past_the_chunk_limit() {
        let frame = compressed_frame(&vec![b'x'; MAX_TRANSFER_DATA_SIZE * 32]);
        assert!(matches!(
            DataChunk::from_frame(&frame).unwrap().decode_blocking(),
            Err(RemoteTransferError::Decompression(_))
        ));
    }

    #[test]
    fn corrupt_compressed_data_is_rejected() {
        let frame = Frame::new(
            FrameKind::Data,
            FrameFlags::COMPRESSED,
            StreamId::new(1),
            Bytes::from_static(b"not zstd"),
        )
        .unwrap();
        assert!(matches!(
            DataChunk::from_frame(&frame).unwrap().decode_blocking(),
            Err(RemoteTransferError::Decompression(_))
        ));
    }
}
