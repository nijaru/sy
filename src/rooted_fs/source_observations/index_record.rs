//! One canonical entry owns its observation locator and optional original state.
//! Original ordinals are authenticated here, never by the candidate-key tape.
use super::{invalid, inventory::OriginalState, Result};

pub(super) const RECORD_BYTES: usize = 66;

#[derive(Clone, Copy)]
pub(super) struct Entry {
    pub locator: [u8; 48],
    pub original: Option<OriginalState>,
}

#[derive(Clone, Copy)]
pub(super) enum IndexRecord {
    Entry(Entry),
    Epoch(u64),
    EmptyStamp([u8; 32]),
}

impl IndexRecord {
    pub fn encode(self) -> [u8; RECORD_BYTES] {
        let mut bytes = [0; RECORD_BYTES];
        match self {
            Self::Entry(entry) => {
                bytes[1..49].copy_from_slice(&entry.locator);
                if let Some(original) = entry.original {
                    bytes[0] = 1;
                    bytes[49..].copy_from_slice(&original.encode());
                }
            }
            Self::Epoch(epoch) => {
                bytes[0] = 3;
                bytes[1..9].copy_from_slice(&epoch.to_le_bytes());
            }
            Self::EmptyStamp(stamp) => {
                bytes[0] = 4;
                bytes[1..33].copy_from_slice(&stamp);
            }
        }
        bytes
    }

    pub fn decode(bytes: [u8; RECORD_BYTES]) -> Result<Self> {
        let (record, used) = match bytes[0] {
            tag @ (0 | 1) => (
                Self::Entry(Entry {
                    locator: bytes[1..49].try_into().map_err(|_| invalid())?,
                    original: if tag == 1 {
                        Some(OriginalState::decode(
                            bytes[49..].try_into().map_err(|_| invalid())?,
                        )?)
                    } else {
                        None
                    },
                }),
                if tag == 1 { RECORD_BYTES } else { 49 },
            ),
            3 => (
                Self::Epoch(u64::from_le_bytes(
                    bytes[1..9].try_into().map_err(|_| invalid())?,
                )),
                9,
            ),
            4 => (
                Self::EmptyStamp(bytes[1..33].try_into().map_err(|_| invalid())?),
                33,
            ),
            _ => return Err(invalid().into()),
        };
        if bytes[used..].iter().any(|byte| *byte != 0) {
            return Err(invalid().into());
        }
        Ok(record)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rooted_fs::source_observations::inventory::SourceDirectoryDisposition;

    #[test]
    fn original_entry_layout_owns_locator_and_all_state_fields() {
        for (disposition, expected) in [
            (SourceDirectoryDisposition::Opaque, 0),
            (SourceDirectoryDisposition::Traversed, 1),
            (SourceDirectoryDisposition::Protected, 2),
        ] {
            let original = OriginalState {
                ordinal: 0x0123_4567_89ab_cdef,
                children: u64::MAX,
                directory: disposition,
            };
            let record = IndexRecord::Entry(Entry {
                locator: [0xa5; 48],
                original: Some(original),
            });
            let mut reference = [0; 66];
            reference[0] = 1;
            reference[1..49].fill(0xa5);
            reference[49..57].copy_from_slice(&[0xef, 0xcd, 0xab, 0x89, 0x67, 0x45, 0x23, 0x01]);
            reference[57..65].fill(0xff);
            reference[65] = expected;
            assert_eq!(record.encode(), reference);
            let IndexRecord::Entry(decoded) = IndexRecord::decode(reference).unwrap() else {
                panic!("original must be an entry");
            };
            assert_eq!(decoded.locator, [0xa5; 48]);
            let state = decoded.original.unwrap();
            assert_eq!(state.ordinal, original.ordinal);
            assert_eq!(state.children, u64::MAX);
            assert_eq!(state.directory, disposition);
        }
    }

    #[test]
    fn unused_padding_unknown_tags_and_invalid_dispositions_are_rejected() {
        let records = [
            (
                IndexRecord::Entry(Entry {
                    locator: [7; 48],
                    original: None,
                }),
                0,
                49,
            ),
            (IndexRecord::Epoch(0x0807_0605_0403_0201), 3, 9),
            (IndexRecord::EmptyStamp([9; 32]), 4, 33),
        ];
        for (record, tag, used) in records {
            let bytes = record.encode();
            assert_eq!(bytes[0], tag);
            match IndexRecord::decode(bytes).unwrap() {
                IndexRecord::Entry(entry) => {
                    assert_eq!(entry.locator, [7; 48]);
                    assert!(entry.original.is_none());
                }
                IndexRecord::Epoch(epoch) => assert_eq!(epoch, 0x0807_0605_0403_0201),
                IndexRecord::EmptyStamp(stamp) => assert_eq!(stamp, [9; 32]),
            }
            assert!(bytes[used..].iter().all(|byte| *byte == 0));
            for position in used..RECORD_BYTES {
                let mut invalid = bytes;
                invalid[position] = 1;
                assert!(
                    IndexRecord::decode(invalid).is_err(),
                    "tag {tag}, byte {position}"
                );
            }
        }
        let mut bytes = [0; RECORD_BYTES];
        for tag in std::iter::once(2).chain(5..=255) {
            bytes[0] = tag;
            assert!(IndexRecord::decode(bytes).is_err(), "tag {tag}");
        }
        bytes[0] = 1;
        for disposition in 3..=255 {
            bytes[65] = disposition;
            assert!(
                IndexRecord::decode(bytes).is_err(),
                "disposition {disposition}"
            );
        }
    }
}
