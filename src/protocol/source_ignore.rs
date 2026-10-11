//! Read-only source ignore matching for pull destination deletion protection.
use super::codec::SliceReader;
use super::{ProtocolError, RelativeWirePath, Result, MAX_WIRE_PATH_BYTES};
use bytes::{BufMut, Bytes, BytesMut};

/// Queries ignore policy under the remote source's held root, not destination
/// policy. The path uses the source's physical name, including for selected leaves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireSourceIgnoreRequest {
    pub path: RelativeWirePath,
    pub is_directory: bool,
    pub respect_gitignore: bool,
}

impl WireSourceIgnoreRequest {
    pub fn encode(&self) -> Result<Bytes> {
        let path = self.path.as_encoded();
        let path_len = u32::try_from(path.len())
            .map_err(|_| ProtocolError::InvalidMessage("source ignore path length exceeds u32"))?;
        let mut out = BytesMut::with_capacity(6 + path.len());
        out.put_u8(u8::from(self.respect_gitignore));
        out.put_u8(u8::from(self.is_directory));
        out.put_u32(path_len);
        out.extend_from_slice(path);
        Ok(out.freeze())
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut reader = SliceReader::new(payload);
        let respect_gitignore = read_bool(&mut reader, "respect_gitignore")?;
        let is_directory = read_bool(&mut reader, "is_directory")?;
        let len = reader.u32()? as usize;
        if len > MAX_WIRE_PATH_BYTES {
            return Err(ProtocolError::PathTooLong {
                len,
                max: MAX_WIRE_PATH_BYTES,
            });
        }
        let path = RelativeWirePath::decode(Bytes::copy_from_slice(reader.take(len)?))?;
        reader.finish()?;
        Ok(Self {
            path,
            is_directory,
            respect_gitignore,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WireSourceIgnoreResult {
    pub protected: bool,
}

impl WireSourceIgnoreResult {
    pub fn encode(&self) -> Result<Bytes> {
        Ok(Bytes::from_static(if self.protected { &[1] } else { &[0] }))
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut reader = SliceReader::new(payload);
        let protected = read_bool(&mut reader, "protected")?;
        reader.finish()?;
        Ok(Self { protected })
    }
}

fn read_bool(reader: &mut SliceReader<'_>, field: &'static str) -> Result<bool> {
    match reader.u8()? {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(ProtocolError::InvalidField {
            field,
            reason: "expected boolean 0 or 1",
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn request_layout_preserves_source_native_name_and_rejects_incomplete_messages() {
        let request = WireSourceIgnoreRequest {
            path: RelativeWirePath::from_components([b"\xff".as_slice()]).unwrap(),
            is_directory: false,
            respect_gitignore: true,
        };
        let encoded = request.encode().unwrap();
        assert_eq!(encoded.as_ref(), &[1, 0, 0, 0, 0, 5, 0, 1, 0, 1, 0xff]);
        assert_eq!(WireSourceIgnoreRequest::decode(&encoded).unwrap(), request);
        for len in 0..encoded.len() {
            assert!(WireSourceIgnoreRequest::decode(&encoded[..len]).is_err());
        }
        let mut trailing = encoded.to_vec();
        trailing.push(0);
        assert!(WireSourceIgnoreRequest::decode(&trailing).is_err());
    }

    #[test]
    fn request_checks_path_bound_before_body_and_uses_canonical_path_codec() {
        let mut oversized = vec![0, 0];
        oversized.extend_from_slice(
            &u32::try_from(MAX_WIRE_PATH_BYTES + 1)
                .unwrap()
                .to_be_bytes(),
        );
        assert!(matches!(
            WireSourceIgnoreRequest::decode(&oversized),
            Err(ProtocolError::PathTooLong { .. })
        ));
        // Shared path tests cover component bounds; exercise the request boundary
        // with empty paths, empty components, and trailing bytes inside the path.
        for path in [&[0, 0][..], &[0, 1, 0, 0], &[0, 1, 0, 1, b'a', 0]] {
            let mut payload = vec![0, 0];
            payload.extend_from_slice(&u32::try_from(path.len()).unwrap().to_be_bytes());
            payload.extend_from_slice(path);
            assert!(WireSourceIgnoreRequest::decode(&payload).is_err());
        }
    }

    #[test]
    fn all_boolean_fields_are_strict_and_result_is_exactly_one_byte() {
        let request = WireSourceIgnoreRequest {
            path: RelativeWirePath::from_components([b"file".as_slice()]).unwrap(),
            is_directory: true,
            respect_gitignore: false,
        };
        let encoded = request.encode().unwrap();
        assert!(WireSourceIgnoreResult::decode(&[]).is_err());
        for value in 0..=u8::MAX {
            if value <= 1 {
                let result = WireSourceIgnoreResult {
                    protected: value == 1,
                };
                assert_eq!(result.encode().unwrap().as_ref(), &[value]);
                assert_eq!(WireSourceIgnoreResult::decode(&[value]).unwrap(), result);
            } else {
                for index in 0..2 {
                    let mut invalid = encoded.to_vec();
                    invalid[index] = value;
                    assert!(matches!(
                        WireSourceIgnoreRequest::decode(&invalid),
                        Err(ProtocolError::InvalidField { .. })
                    ));
                }
                assert!(WireSourceIgnoreResult::decode(&[value]).is_err());
            }
            assert!(WireSourceIgnoreResult::decode(&[value, 0]).is_err());
        }
    }

    proptest! {
        #[test]
        fn request_round_trip(
            components in prop::collection::vec(prop::collection::vec(any::<u8>(), 1..64), 1..8),
            is_directory in any::<bool>(),
            respect_gitignore in any::<bool>(),
        ) {
            let request = WireSourceIgnoreRequest {
                path: RelativeWirePath::from_components(components).unwrap(),
                is_directory,
                respect_gitignore,
            };
            prop_assert_eq!(WireSourceIgnoreRequest::decode(&request.encode().unwrap()).unwrap(), request);
        }

        #[test]
        fn arbitrary_source_ignore_payloads_never_panic(payload in prop::collection::vec(any::<u8>(), 0..4096)) {
            let _ = WireSourceIgnoreRequest::decode(&payload);
            let _ = WireSourceIgnoreResult::decode(&payload);
        }
    }
}
