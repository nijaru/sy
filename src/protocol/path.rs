use super::{ProtocolError, Result};
use bytes::{BufMut, Bytes, BytesMut};

pub const MAX_WIRE_PATH_BYTES: usize = 128 * 1024;
pub const MAX_WIRE_COMPONENT_BYTES: usize = u16::MAX as usize;
pub const MAX_WIRE_COMPONENTS: usize = u16::MAX as usize;

/// Opaque native path payload carried by a control message.
///
/// Native encoding belongs to the endpoint platform. The protocol layer only
/// enforces a byte bound; it must not reject byte patterns such as NUL before
/// the negotiated platform interprets them (UTF-16LE names commonly contain
/// zero bytes). Root-path semantic validation belongs at the endpoint boundary.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WirePath(Bytes);

impl WirePath {
    pub fn new(path: impl Into<Bytes>) -> Result<Self> {
        let path = path.into();
        validate_total_len(path.len())?;
        Ok(Self(path))
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn into_bytes(self) -> Bytes {
        self.0
    }
}

/// Delimiter-free relative path represented as opaque native name components.
///
/// The encoding is canonical:
///
/// ```text
/// component_count: u16
/// repeated component_count times:
///   byte_len: u16
///   native_name_bytes: [u8; byte_len]
/// ```
///
/// Separators are structural rather than encoded as bytes. This avoids assuming
/// `/` or `\\` semantics and lets Windows names travel as raw UTF-16LE while Unix
/// names remain arbitrary native bytes. The sender platform from the handshake
/// determines how each component is interpreted.
///
/// This type validates only wire structure and resource bounds. Endpoint
/// adapters must reject platform-specific invalid names (`.`, `..`, separators,
/// NUL code units, reserved Windows names, and so on) before filesystem access.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RelativeWirePath {
    encoded: Bytes,
    component_count: u16,
}

impl RelativeWirePath {
    pub fn from_components<I, B>(components: I) -> Result<Self>
    where
        I: IntoIterator<Item = B>,
        B: AsRef<[u8]>,
    {
        // Reserve the count prefix and fill it after one-pass encoding. This
        // avoids allocating a temporary Vec for every path in metadata streams.
        let mut encoded = BytesMut::with_capacity(256);
        encoded.put_u16(0);
        let mut count = 0_usize;

        for component in components {
            if count == MAX_WIRE_COMPONENTS {
                return Err(ProtocolError::TooManyPathComponents {
                    count: count + 1,
                    max: MAX_WIRE_COMPONENTS,
                });
            }

            let component = component.as_ref();
            validate_component_len(component.len())?;
            let next_len = encoded
                .len()
                .checked_add(2)
                .and_then(|len| len.checked_add(component.len()))
                .ok_or(ProtocolError::InvalidMessage("wire path length overflow"))?;
            validate_total_len(next_len)?;

            let len = u16::try_from(component.len()).map_err(|_| {
                ProtocolError::InvalidMessage("wire path component length exceeds u16")
            })?;
            encoded.put_u16(len);
            encoded.extend_from_slice(component);
            count += 1;
        }

        if count == 0 {
            return Err(ProtocolError::InvalidRelativePath("path is empty"));
        }
        let count = u16::try_from(count).map_err(|_| {
            ProtocolError::InvalidMessage("wire path component count exceeds u16")
        })?;
        encoded[..2].copy_from_slice(&count.to_be_bytes());

        Ok(Self {
            encoded: encoded.freeze(),
            component_count: count,
        })
    }

    /// Validates a path payload received from the wire without allocating one
    /// object per component. The validated encoded bytes are retained directly.
    pub fn decode(encoded: impl Into<Bytes>) -> Result<Self> {
        let encoded = encoded.into();
        validate_total_len(encoded.len())?;
        if encoded.len() < 2 {
            return Err(ProtocolError::InvalidMessage(
                "truncated wire path component count",
            ));
        }

        let component_count = u16::from_be_bytes([encoded[0], encoded[1]]);
        if component_count == 0 {
            return Err(ProtocolError::InvalidRelativePath("path is empty"));
        }

        let mut offset = 2_usize;
        for _ in 0..component_count {
            let length_end = offset
                .checked_add(2)
                .ok_or(ProtocolError::InvalidMessage("wire path length overflow"))?;
            let length_bytes = encoded.get(offset..length_end).ok_or(
                ProtocolError::InvalidMessage("truncated wire path component length"),
            )?;
            let len = u16::from_be_bytes([length_bytes[0], length_bytes[1]]) as usize;
            validate_component_len(len)?;
            offset = length_end
                .checked_add(len)
                .ok_or(ProtocolError::InvalidMessage("wire path length overflow"))?;
            if offset > encoded.len() {
                return Err(ProtocolError::InvalidMessage(
                    "truncated wire path component bytes",
                ));
            }
        }

        if offset != encoded.len() {
            return Err(ProtocolError::InvalidMessage(
                "trailing bytes after wire path",
            ));
        }

        Ok(Self {
            encoded,
            component_count,
        })
    }

    pub fn as_encoded(&self) -> &[u8] {
        &self.encoded
    }

    pub fn into_encoded(self) -> Bytes {
        self.encoded
    }

    pub const fn component_count(&self) -> usize {
        self.component_count as usize
    }

    pub fn components(&self) -> impl ExactSizeIterator<Item = &[u8]> + '_ {
        WireComponents {
            encoded: &self.encoded,
            offset: 2,
            remaining: self.component_count,
        }
    }
}

struct WireComponents<'a> {
    encoded: &'a [u8],
    offset: usize,
    remaining: u16,
}

impl<'a> Iterator for WireComponents<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }

        // RelativeWirePath construction validates these bounds once. Iteration
        // therefore needs no fallible branch or repeated semantic validation.
        let len = u16::from_be_bytes([
            *self.encoded.get(self.offset)?,
            *self.encoded.get(self.offset + 1)?,
        ]) as usize;
        let start = self.offset + 2;
        let end = start.checked_add(len)?;
        let component = self.encoded.get(start..end)?;
        self.offset = end;
        self.remaining -= 1;
        Some(component)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.remaining as usize;
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for WireComponents<'_> {}

fn validate_total_len(len: usize) -> Result<()> {
    if len > MAX_WIRE_PATH_BYTES {
        return Err(ProtocolError::PathTooLong {
            len,
            max: MAX_WIRE_PATH_BYTES,
        });
    }
    Ok(())
}

fn validate_component_len(len: usize) -> Result<()> {
    if len == 0 {
        return Err(ProtocolError::InvalidRelativePath(
            "path contains an empty component",
        ));
    }
    if len > MAX_WIRE_COMPONENT_BYTES {
        return Err(ProtocolError::PathComponentTooLong {
            len,
            max: MAX_WIRE_COMPONENT_BYTES,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn relative_path_round_trip_preserves_opaque_components() {
        let path = RelativeWirePath::from_components([
            b"dir".as_slice(),
            b"\xff/file".as_slice(),
            b"\x00\x01".as_slice(),
        ])
        .unwrap();
        let decoded = RelativeWirePath::decode(path.as_encoded().to_vec()).unwrap();
        assert_eq!(decoded, path);
        assert_eq!(
            decoded.components().collect::<Vec<_>>(),
            vec![
                b"dir".as_slice(),
                b"\xff/file".as_slice(),
                b"\x00\x01".as_slice()
            ]
        );
    }

    #[test]
    fn relative_path_rejects_empty_component() {
        assert!(RelativeWirePath::from_components([b"dir".as_slice(), b"".as_slice()]).is_err());
    }

    #[test]
    fn decoder_rejects_truncation_and_trailing_bytes() {
        let path = RelativeWirePath::from_components([b"a".as_slice(), b"b".as_slice()]).unwrap();
        let encoded = path.as_encoded();
        for len in 0..encoded.len() {
            assert!(RelativeWirePath::decode(Bytes::copy_from_slice(&encoded[..len])).is_err());
        }

        let mut trailing = encoded.to_vec();
        trailing.push(0);
        assert!(RelativeWirePath::decode(trailing).is_err());
    }

    #[test]
    fn native_root_bytes_are_not_interpreted_by_protocol() {
        let utf16_like = Bytes::from_static(&[b'C', 0, b':', 0, b'\\', 0]);
        assert_eq!(
            WirePath::new(utf16_like.clone()).unwrap().into_bytes(),
            utf16_like
        );
    }

    proptest! {
        #[test]
        fn arbitrary_relative_wire_path_payload_never_panics(payload in prop::collection::vec(any::<u8>(), 0..4096)) {
            let _ = RelativeWirePath::decode(payload);
        }
    }
}
