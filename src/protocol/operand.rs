//! Exact 3.10 operand vocabulary. Names and operands use target-native bytes.
use super::codec::SliceReader;
use super::{ProtocolError, Result, WirePath, MAX_WIRE_PATH_BYTES};
use bytes::{BufMut, Bytes, BytesMut};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceShape {
    Directory { basename: Option<WirePath> },
    Leaf { name: WirePath },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OperandRequest {
    Source { contents: bool },
    Destination { source: SourceShape },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedOperand {
    Tree,
    Entry { name: WirePath },
}

pub(super) fn put_path(out: &mut BytesMut, path: &WirePath) -> Result<()> {
    let len = u32::try_from(path.as_bytes().len())
        .map_err(|_| ProtocolError::InvalidMessage("operand path length exceeds u32"))?;
    out.put_u32(len);
    out.extend_from_slice(path.as_bytes());
    Ok(())
}

pub(super) fn get_path(reader: &mut SliceReader<'_>) -> Result<WirePath> {
    let len = reader.u32()? as usize;
    if len > MAX_WIRE_PATH_BYTES {
        return Err(ProtocolError::PathTooLong {
            len,
            max: MAX_WIRE_PATH_BYTES,
        });
    }
    WirePath::new(Bytes::copy_from_slice(reader.take(len)?))
}

impl SourceShape {
    pub(super) fn put(&self, out: &mut BytesMut) -> Result<()> {
        match self {
            Self::Directory { basename: None } => out.put_u8(0),
            Self::Directory {
                basename: Some(name),
            } => {
                out.put_u8(1);
                put_path(out, name)?;
            }
            Self::Leaf { name } => {
                out.put_u8(2);
                put_path(out, name)?;
            }
        }
        Ok(())
    }
    pub(super) fn get(reader: &mut SliceReader<'_>) -> Result<Self> {
        match reader.u8()? {
            0 => Ok(Self::Directory { basename: None }),
            1 => Ok(Self::Directory {
                basename: Some(get_path(reader)?),
            }),
            2 => Ok(Self::Leaf {
                name: get_path(reader)?,
            }),
            _ => Err(ProtocolError::InvalidMessage("unknown source shape")),
        }
    }
}

impl OperandRequest {
    pub(super) fn put(&self, out: &mut BytesMut) -> Result<()> {
        match self {
            Self::Source { contents } => {
                out.put_u8(0);
                out.put_u8(u8::from(*contents));
            }
            Self::Destination { source } => {
                out.put_u8(1);
                source.put(out)?;
            }
        }
        Ok(())
    }
    pub(super) fn get(reader: &mut SliceReader<'_>) -> Result<Self> {
        match reader.u8()? {
            0 => match reader.u8()? {
                0 => Ok(Self::Source { contents: false }),
                1 => Ok(Self::Source { contents: true }),
                _ => Err(ProtocolError::InvalidMessage("unknown contents flag")),
            },
            1 => Ok(Self::Destination {
                source: SourceShape::get(reader)?,
            }),
            _ => Err(ProtocolError::InvalidMessage("unknown operand request")),
        }
    }
}

impl ResolvedOperand {
    pub(super) fn put(&self, out: &mut BytesMut) -> Result<()> {
        match self {
            Self::Tree => out.put_u8(0),
            Self::Entry { name } => {
                out.put_u8(1);
                put_path(out, name)?;
            }
        }
        Ok(())
    }
    pub(super) fn get(reader: &mut SliceReader<'_>) -> Result<Self> {
        match reader.u8()? {
            0 => Ok(Self::Tree),
            1 => Ok(Self::Entry {
                name: get_path(reader)?,
            }),
            _ => Err(ProtocolError::InvalidMessage("unknown resolved operand")),
        }
    }
}
