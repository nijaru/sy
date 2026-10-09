use super::codec::SliceReader;
use super::{
    ProtocolError, RelativeWirePath, Result, WireEntryKind, WirePath, MAX_WIRE_PATH_BYTES,
};
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
    /// Server-side hardlink beneath the pinned root (`-H/--preserve-hardlinks`:
    /// link `path` to the existing `copy_source` inode). Both stay beneath
    /// the pinned root; the source must be a regular file.
    Hardlink = 6,
    /// Revalidate a finalized file/symlink or verified-existing destination
    /// observation before source removal.
    VerifyPublication = 7,
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
            6 => Ok(Self::Hardlink),
            7 => Ok(Self::VerifyPublication),
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
    source_identity: Option<[u8; 32]>,
    entry_kind: Option<WireEntryKind>,
    symlink_target: Option<WirePath>,
    /// Copy source for `CopyFile` mutations; the primary `path` is the copy
    /// destination (the backup location). Both stay beneath the pinned root.
    copy_source: Option<RelativeWirePath>,
    /// For symlinks/hardlinks, None means expected Absent. For copies this
    /// required token identifies the source, not the backup destination.
    expected_identity: Option<[u8; 32]>,
    modified: Option<(i64, u32)>,
}

impl WireMutation {
    pub const fn create_directory(path: RelativeWirePath) -> Self {
        Self {
            path,
            kind: WireMutationKind::CreateDirectory,
            source_identity: None,
            entry_kind: None,
            symlink_target: None,
            copy_source: None,
            expected_identity: None,
            modified: None,
        }
    }

    pub const fn replace_symlink(
        path: RelativeWirePath,
        target: WirePath,
        expected_identity: Option<[u8; 32]>,
        modified: Option<(i64, u32)>,
    ) -> Self {
        Self {
            path,
            kind: WireMutationKind::ReplaceSymlink,
            source_identity: None,
            entry_kind: None,
            symlink_target: Some(target),
            copy_source: None,
            expected_identity,
            modified,
        }
    }

    pub const fn remove_file_like(
        path: RelativeWirePath,
        expected_identity: Option<[u8; 32]>,
    ) -> Self {
        Self {
            path,
            kind: WireMutationKind::RemoveFileLike,
            source_identity: None,
            entry_kind: None,
            symlink_target: None,
            copy_source: None,
            expected_identity,
            modified: None,
        }
    }

    pub const fn remove_directory(
        path: RelativeWirePath,
        expected_identity: Option<[u8; 32]>,
    ) -> Self {
        Self {
            path,
            kind: WireMutationKind::RemoveDirectory,
            source_identity: None,
            entry_kind: None,
            symlink_target: None,
            copy_source: None,
            expected_identity,
            modified: None,
        }
    }

    /// Copy an existing root-relative file to another root-relative path.
    /// Used for `--backup`: the server copies the soon-to-be-replaced or
    /// deleted destination file before the mutation, keeping every resolved
    /// path beneath the session root.
    pub fn copy_file(
        source: RelativeWirePath,
        destination: RelativeWirePath,
        expected_source_identity: [u8; 32],
    ) -> Self {
        Self {
            path: destination,
            kind: WireMutationKind::CopyFile,
            source_identity: None,
            entry_kind: None,
            symlink_target: None,
            copy_source: Some(source),
            expected_identity: Some(expected_source_identity),
            modified: None,
        }
    }

    /// Link `destination` to the existing root-relative `source` inode.
    /// Used for `-H/--preserve-hardlinks`: members of one scanned hardlink
    /// group share a single transferred representative; the rest become
    /// links to it. The `copy_source` field carries the link source so the
    /// wire shape stays bounded like `CopyFile`.
    pub fn hardlink(
        source: RelativeWirePath,
        destination: RelativeWirePath,
        source_identity: [u8; 32],
        expected_destination: Option<[u8; 32]>,
    ) -> Self {
        Self {
            path: destination,
            kind: WireMutationKind::Hardlink,
            source_identity: Some(source_identity),
            entry_kind: None,
            symlink_target: None,
            copy_source: Some(source),
            expected_identity: expected_destination,
            modified: None,
        }
    }

    pub const fn verify_publication(
        path: RelativeWirePath,
        identity: [u8; 32],
        kind: WireEntryKind,
    ) -> Self {
        Self {
            path,
            kind: WireMutationKind::VerifyPublication,
            source_identity: None,
            entry_kind: Some(kind),
            symlink_target: None,
            copy_source: None,
            expected_identity: Some(identity),
            modified: None,
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

    pub const fn expected_identity(&self) -> Option<&[u8; 32]> {
        self.expected_identity.as_ref()
    }

    pub const fn source_identity(&self) -> Option<[u8; 32]> {
        self.source_identity
    }
    pub const fn entry_kind(&self) -> Option<WireEntryKind> {
        self.entry_kind
    }

    pub const fn modified(&self) -> Option<(i64, u32)> {
        self.modified
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
        if self.kind == WireMutationKind::ReplaceSymlink {
            capacity = capacity
                .checked_add(1 + if self.modified.is_some() { 12 } else { 0 })
                .ok_or(ProtocolError::InvalidMessage(
                    "mutation payload length overflow",
                ))?;
        }
        if self.kind == WireMutationKind::RemoveFileLike
            || self.kind == WireMutationKind::RemoveDirectory
            || self.kind == WireMutationKind::ReplaceSymlink
            || self.kind == WireMutationKind::CopyFile
            || self.kind == WireMutationKind::VerifyPublication
            || self.kind == WireMutationKind::Hardlink
        {
            capacity = capacity
                .checked_add(
                    1 + if self.expected_identity.is_some() {
                        32
                    } else {
                        0
                    },
                )
                .ok_or(ProtocolError::InvalidMessage(
                    "mutation payload length overflow",
                ))?;
        }

        capacity = capacity
            .checked_add(if self.source_identity.is_some() {
                32
            } else {
                0
            })
            .and_then(|value| value.checked_add(usize::from(self.entry_kind.is_some())))
            .ok_or(ProtocolError::InvalidMessage(
                "mutation payload length overflow",
            ))?;
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
        if self.kind == WireMutationKind::RemoveFileLike
            || self.kind == WireMutationKind::RemoveDirectory
            || self.kind == WireMutationKind::ReplaceSymlink
            || self.kind == WireMutationKind::CopyFile
            || self.kind == WireMutationKind::VerifyPublication
            || self.kind == WireMutationKind::Hardlink
        {
            if let Some(identity) = self.expected_identity {
                out.put_u8(1);
                out.extend_from_slice(&identity);
            } else {
                out.put_u8(0);
            }
        }
        if self.kind == WireMutationKind::ReplaceSymlink {
            if let Some((seconds, nanos)) = self.modified {
                out.put_u8(1);
                out.put_i64(seconds);
                out.put_u32(nanos);
            } else {
                out.put_u8(0);
            }
        }
        if let Some(identity) = self.source_identity {
            out.extend_from_slice(&identity);
        }
        if let Some(kind) = self.entry_kind {
            out.put_u8(kind as u8);
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
        let copy_source =
            if kind == WireMutationKind::CopyFile || kind == WireMutationKind::Hardlink {
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
        let expected_identity = if kind == WireMutationKind::RemoveFileLike
            || kind == WireMutationKind::RemoveDirectory
            || kind == WireMutationKind::ReplaceSymlink
            || kind == WireMutationKind::CopyFile
            || kind == WireMutationKind::VerifyPublication
            || kind == WireMutationKind::Hardlink
        {
            match reader.u8()? {
                0 => None,
                1 => Some(reader.array::<32>()?),
                _ => {
                    return Err(ProtocolError::InvalidField {
                        field: "expected_identity",
                        reason: "unknown identity presence flag",
                    })
                }
            }
        } else {
            None
        };
        let modified = if kind == WireMutationKind::ReplaceSymlink {
            match reader.u8()? {
                0 => None,
                1 => Some((reader.i64()?, reader.u32()?)),
                _ => {
                    return Err(ProtocolError::InvalidField {
                        field: "modified",
                        reason: "unknown timestamp presence flag",
                    })
                }
            }
        } else {
            None
        };
        let source_identity = if kind == WireMutationKind::Hardlink {
            Some(reader.array::<32>()?)
        } else {
            None
        };
        let entry_kind = if kind == WireMutationKind::VerifyPublication {
            Some(WireEntryKind::try_from(reader.u8()?)?)
        } else {
            None
        };
        reader.finish()?;
        let mutation = Self {
            path,
            kind,
            source_identity,
            entry_kind,
            symlink_target,
            copy_source,
            expected_identity,
            modified,
        };
        mutation.validate()?;
        Ok(mutation)
    }

    fn validate(&self) -> Result<()> {
        if self.source_identity.is_some() != (self.kind == WireMutationKind::Hardlink)
            || self.entry_kind.is_some() != (self.kind == WireMutationKind::VerifyPublication)
            || self.entry_kind == Some(WireEntryKind::Directory)
        {
            return Err(ProtocolError::InvalidMessage(
                "invalid mutation publication authority",
            ));
        }
        if self.modified.is_some() && self.kind != WireMutationKind::ReplaceSymlink {
            return Err(ProtocolError::InvalidField {
                field: "modified",
                reason: "timestamp is valid only for replace-symlink mutation",
            });
        }
        if self
            .modified
            .is_some_and(|(_, nanos)| nanos >= 1_000_000_000)
        {
            return Err(ProtocolError::InvalidField {
                field: "modified_nanoseconds",
                reason: "nanoseconds must be less than one second",
            });
        }
        match (
            self.kind,
            self.symlink_target.is_some(),
            self.copy_source.is_some(),
            self.expected_identity.is_some(),
        ) {
            (WireMutationKind::ReplaceSymlink, true, false, _)
            | (WireMutationKind::CopyFile, false, true, true)
            | (WireMutationKind::Hardlink, false, true, _)
            | (WireMutationKind::CreateDirectory, false, false, false)
            | (WireMutationKind::RemoveFileLike, false, false, _)
            | (WireMutationKind::RemoveDirectory, false, false, _)
            | (WireMutationKind::VerifyPublication, false, false, true) => Ok(()),
            (WireMutationKind::VerifyPublication, false, false, false) => {
                Err(ProtocolError::InvalidField {
                    field: "expected_identity",
                    reason: "publication verification requires identity",
                })
            }
            (WireMutationKind::ReplaceSymlink, false, _, _) => Err(ProtocolError::InvalidField {
                field: "symlink_target",
                reason: "replace-symlink mutation requires a target",
            }),
            (WireMutationKind::CopyFile, false, true, false) => Err(ProtocolError::InvalidField {
                field: "expected_identity",
                reason: "copy-file mutation requires a source identity",
            }),
            (WireMutationKind::CopyFile, _, _, _) => Err(ProtocolError::InvalidField {
                field: "copy_source",
                reason: "copy-file mutation requires a source path",
            }),
            (WireMutationKind::Hardlink, _, _, _) => Err(ProtocolError::InvalidField {
                field: "copy_source",
                reason: "hardlink mutation requires a source path",
            }),
            (_, true, _, _) => Err(ProtocolError::InvalidField {
                field: "symlink_target",
                reason: "target is valid only for replace-symlink mutation",
            }),
            (_, _, true, _) => Err(ProtocolError::InvalidField {
                field: "copy_source",
                reason: "copy source is valid only for copy-file or hardlink mutation",
            }),
            (_, _, _, true) => Err(ProtocolError::InvalidField {
                field: "expected_identity",
                reason:
                    "expected identity is valid only for remove, replace-symlink or copy mutations",
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
            WireMutation::replace_symlink(path(), target.clone(), None, None),
            WireMutation::replace_symlink(path(), target, Some([9; 32]), Some((-10, 123))),
            WireMutation::remove_file_like(path(), None),
            WireMutation::remove_file_like(path(), Some([7; 32])),
            WireMutation::remove_directory(path(), None),
            WireMutation::remove_directory(path(), Some([8; 32])),
            WireMutation::copy_file(path(), backup_dir.clone(), [7; 32]),
            WireMutation::hardlink(path(), backup_dir, [3; 32], None),
            WireMutation::verify_publication(path(), [4; 32], WireEntryKind::Symlink),
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
        let symlink = WireMutation::replace_symlink(
            path(),
            WirePath::new(Bytes::from_static(b"target")).unwrap(),
            Some([9; 32]),
            Some((-10, 123)),
        )
        .encode()
        .unwrap();
        let encoded = WireMutation::create_directory(path()).encode().unwrap();
        let copy = WireMutation::copy_file(path(), path(), [7; 32])
            .encode()
            .unwrap();
        let hardlink = WireMutation::hardlink(path(), path(), [3; 32], Some([4; 32]))
            .encode()
            .unwrap();
        let verification =
            WireMutation::verify_publication(path(), [4; 32], WireEntryKind::Symlink)
                .encode()
                .unwrap();
        for message in [&encoded, &symlink, &copy, &hardlink, &verification] {
            for len in 0..message.len() {
                assert!(WireMutation::decode(&message[..len]).is_err());
            }
            let mut trailing = message.to_vec();
            trailing.push(0);
            assert!(WireMutation::decode(&trailing).is_err());
        }
        let mut bad_nanos = symlink.to_vec();
        let len = bad_nanos.len();
        bad_nanos[len - 4..].copy_from_slice(&1_000_000_000_u32.to_be_bytes());
        assert!(matches!(
            WireMutation::decode(&bad_nanos),
            Err(ProtocolError::InvalidField {
                field: "modified_nanoseconds",
                ..
            })
        ));
        let mut flags = WireMutation::replace_symlink(
            path(),
            WirePath::new(Bytes::from_static(b"target")).unwrap(),
            None,
            None,
        )
        .encode()
        .unwrap()
        .to_vec();
        let len = flags.len();
        for index in [len - 2, len - 1] {
            flags[index] = 2;
            assert!(WireMutation::decode(&flags).is_err());
            flags[index] = 0;
        }
        let mut missing_copy_identity = copy[..copy.len() - 32].to_vec();
        *missing_copy_identity.last_mut().unwrap() = 0;
        assert!(matches!(
            WireMutation::decode(&missing_copy_identity),
            Err(ProtocolError::InvalidField {
                field: "expected_identity",
                ..
            })
        ));
        let mut invalid_copy_flag = missing_copy_identity;
        *invalid_copy_flag.last_mut().unwrap() = 2;
        assert!(WireMutation::decode(&invalid_copy_flag).is_err());
        let mut unknown = encoded.to_vec();
        unknown[0] = u8::MAX;
        assert!(matches!(
            WireMutation::decode(&unknown),
            Err(ProtocolError::InvalidField {
                field: "mutation_kind",
                ..
            })
        ));
    }

    proptest! {
        #[test]
        fn arbitrary_mutation_payloads_never_panic(payload in prop::collection::vec(any::<u8>(), 0..4096)) {
            let _ = WireMutation::decode(&payload);
        }
    }
}
