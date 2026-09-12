use super::codec::SliceReader;
use super::{ProtocolError, RelativeWirePath, Result, WirePath, MAX_WIRE_PATH_BYTES};
use bytes::{BufMut, Bytes, BytesMut};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum WireMutationKind {
    CreateDirectory = 1,
    ReplaceSymlink = 2,
    RemoveFileLike = 3,
    RemoveDirectory = 4,
    /// Server-side copy beneath the pinned root. Seeds the object-native
    /// copy strategy and backs up replaced/deleted files for `--backup`.
    CopyFile = 5,
}

impl TryFrom<u8> for WireMutationKind {
    type Error = ProtocolError;

    fn try_from(value: u8) -> Result<Self> {
        match value {
            1 => Ok(Self::CreateDirectory),
            2 => Ok(Self::ReplaceSymlink),
            3 => Ok(Self::RemoveFileLike),
            4 => Ok(Self::RemoveDirectory),
            5 => Ok(Self::CopyFile),
            _ => Err(ProtocolError::InvalidField {
                field: "mutation_kind",
                reason: "unknown mutation kind",
            }),
        }
    }
}

/// One bounded namespace mutation request.
///
/// Regular-file contents have their own transfer stream. This message covers
/// only operations represented atomically by a small request. The symlink
/// target is opaque native path data encoded for the initiating platform; all
/// destination paths remain validated relative wire paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireMutation {
    pub path: RelativeWirePath,
    kind: WireMutationKind,
    symlink_target: Option<WirePath>,
    /// Copy source for `CopyFile` mutations; the primary `path` is the copy
    /// destination (the backup location). Both stay beneath the pinned root.
    copy_source: Option<RelativeWirePath>,
}

impl WireMutation {
    pub const fn create_directory(path: RelativeWirePath) -> Self {
        Self {
            path,
            kind: WireMutationKind::CreateDirectory,
            symlink_target: None,
            copy_source: None,
        }
    }

    pub const fn replace_symlink(path: RelativeWirePath, target: WirePath) -> Self {
        Self {
            path,
            kind: WireMutationKind::ReplaceSymlink,
            symlink_target: Some(target),
            copy_source: None,
        }
    }

    pub const fn remove_file_like(path: RelativeWirePath) -> Self {
        Self {
            path,
            kind: WireMutationKind::RemoveFileLike,
            symlink_target: None,
            copy_source: None,
        }
    }

    pub const fn remove_directory(path: RelativeWirePath) -> Self {
        Self {
            path,
            kind: WireMutationKind::RemoveDirectory,
            symlink_target: None,
            copy_source: None,
        }
    }

    /// Copy an existing root-relative file to another root-relative path.
    /// Used for `--backup`: the server copies the soon-to-be-replaced or
    /// deleted destination file before the mutation, keeping every resolved
    /// path beneath the session root.
    pub fn copy_file(source: RelativeWirePath, destination: RelativeWirePath) -> Self {
        Self {
            path: destination,
            kind: WireMutationKind::CopyFile,
            symlink_target: None,
            copy_source: Some(source),
        }
    }

    pub const fn kind(&self) -> WireMutationKind {
        self.kind
    }

    pub fn symlink_target(&self) -> Option<&WirePath> {
        self.symlink_target.as_ref()
    }

    pub fn copy_source(&self) -> Option<&RelativeWirePath> {
        self.copy_source.as_ref()
    }

    pub fn encode(&self) -> Result<Bytes> {
        self.validate()?;
        let path_len = u32::try_from(self.path.as_encoded().len()).map_err(|_| {
            ProtocolError::InvalidField {
                field: "mutation_path",
                reason: "encoded relative path length exceeds u32",
            }
        })?;
        let target_len = self
            .symlink_target
            .as_ref()
            .map(|target| {
                u32::try_from(target.as_bytes().len()).map_err(|_| ProtocolError::InvalidField {
                    field: "symlink_target",
                    reason: "symlink target length exceeds u32",
                })
            })
            .transpose()?;
        let copy_len = self
            .copy_source
            .as_ref()
            .map(|source| {
                u32::try_from(source.as_encoded().len()).map_err(|_| ProtocolError::InvalidField {
                    field: "copy_source",
                    reason: "encoded copy source length exceeds u32",
                })
            })
            .transpose()?;

        let mut capacity = 5_usize.checked_add(self.path.as_encoded().len()).ok_or(
            ProtocolError::InvalidMessage("mutation payload length overflow"),
        )?;
        if let Some(target) = self.symlink_target.as_ref() {
            capacity = capacity
                .checked_add(4)
                .and_then(|value| value.checked_add(target.as_bytes().len()))
                .ok_or(ProtocolError::InvalidMessage(
                    "mutation payload length overflow",
                ))?;
        }
        if let Some(source) = self.copy_source.as_ref() {
            capacity = capacity
                .checked_add(4)
                .and_then(|value| value.checked_add(source.as_encoded().len()))
                .ok_or(ProtocolError::InvalidMessage(
                    "mutation payload length overflow",
                ))?;
        }

        let mut out = BytesMut::with_capacity(capacity);
        out.put_u8(self.kind as u8);
        out.put_u32(path_len);
        out.extend_from_slice(self.path.as_encoded());
        if let (Some(target), Some(target_len)) = (self.symlink_target.as_ref(), target_len) {
            out.put_u32(target_len);
            out.extend_from_slice(target.as_bytes());
        }
        if let (Some(source), Some(copy_len)) = (self.copy_source.as_ref(), copy_len) {
            out.put_u32(copy_len);
            out.extend_from_slice(source.as_encoded());
        }
        Ok(out.freeze())
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut reader = SliceReader::new(payload);
        let kind = WireMutationKind::try_from(reader.u8()?)?;
        let path_len = reader.u32()? as usize;
        if path_len > MAX_WIRE_PATH_BYTES {
            return Err(ProtocolError::PathTooLong {
                len: path_len,
                max: MAX_WIRE_PATH_BYTES,
            });
        }
        let path = RelativeWirePath::decode(Bytes::copy_from_slice(reader.take(path_len)?))?;
        let symlink_target = if kind == WireMutationKind::ReplaceSymlink {
            let target_len = reader.u32()? as usize;
            if target_len > MAX_WIRE_PATH_BYTES {
                return Err(ProtocolError::PathTooLong {
                    len: target_len,
                    max: MAX_WIRE_PATH_BYTES,
                });
            }
            Some(WirePath::new(Bytes::copy_from_slice(
                reader.take(target_len)?,
            ))?)
        } else {
            None
        };
        let copy_source = if kind == WireMutationKind::CopyFile {
            let source_len = reader.u32()? as usize;
            if source_len > MAX_WIRE_PATH_BYTES {
                return Err(ProtocolError::PathTooLong {
                    len: source_len,
                    max: MAX_WIRE_PATH_BYTES,
                });
            }
            Some(RelativeWirePath::decode(Bytes::copy_from_slice(
                reader.take(source_len)?,
            ))?)
        } else {
            None
        };
        reader.finish()?;
        let mutation = Self {
            path,
            kind,
            symlink_target,
            copy_source,
        };
        mutation.validate()?;
        Ok(mutation)
    }

    fn validate(&self) -> Result<()> {
        match (
            self.kind,
            self.symlink_target.is_some(),
            self.copy_source.is_some(),
        ) {
            (WireMutationKind::ReplaceSymlink, true, false)
            | (WireMutationKind::CopyFile, false, true)
            | (
                WireMutationKind::CreateDirectory
                | WireMutationKind::RemoveFileLike
                | WireMutationKind::RemoveDirectory,
                false,
                false,
            ) => Ok(()),
            (WireMutationKind::ReplaceSymlink, false, _) => Err(ProtocolError::InvalidField {
                field: "symlink_target",
                reason: "replace-symlink mutation requires a target",
            }),
            (WireMutationKind::CopyFile, _, _) => Err(ProtocolError::InvalidField {
                field: "copy_source",
                reason: "copy-file mutation requires a source path",
            }),
            (_, true, _) => Err(ProtocolError::InvalidField {
                field: "symlink_target",
                reason: "target is valid only for replace-symlink mutation",
            }),
            (_, _, true) => Err(ProtocolError::InvalidField {
                field: "copy_source",
                reason: "copy source is valid only for copy-file mutation",
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn path() -> RelativeWirePath {
        RelativeWirePath::from_components([b"dir".as_slice(), b"entry".as_slice()]).unwrap()
    }

    #[test]
    fn mutation_round_trips_all_kinds() {
        let target = WirePath::new(Bytes::from_static(b"../target")).unwrap();
        let backup_dir = RelativeWirePath::from_components([b"backup".as_slice()]).unwrap();
        let mutations = [
            WireMutation::create_directory(path()),
            WireMutation::replace_symlink(path(), target),
            WireMutation::remove_file_like(path()),
            WireMutation::remove_directory(path()),
            WireMutation::copy_file(path(), backup_dir),
        ];
        for mutation in mutations {
            assert_eq!(
                WireMutation::decode(&mutation.encode().unwrap()).unwrap(),
                mutation
            );
        }
    }

    #[test]
    fn decoder_rejects_unknown_kind_truncation_and_trailing_data() {
        let encoded = WireMutation::create_directory(path()).encode().unwrap();
        for len in 0..encoded.len() {
            assert!(WireMutation::decode(&encoded[..len]).is_err());
        }
        let mut unknown = encoded.to_vec();
        unknown[0] = u8::MAX;
        assert!(matches!(
            WireMutation::decode(&unknown),
            Err(ProtocolError::InvalidField {
                field: "mutation_kind",
                ..
            })
        ));
        let mut trailing = encoded.to_vec();
        trailing.push(0);
        assert!(WireMutation::decode(&trailing).is_err());
    }

    proptest! {
        #[test]
        fn arbitrary_mutation_payloads_never_panic(payload in prop::collection::vec(any::<u8>(), 0..4096)) {
            let _ = WireMutation::decode(&payload);
        }
    }
}
