//! Protocol admission narrows a held parent FD to the operator-selected leaf.
//! Private staging and explicit backup destinations remain handler-owned effects.
use crate::protocol::{
    AcquireRoot, Frame, FrameKind, PlatformOs, ProtocolError, RelativeWirePath, ResolvedOperand,
    WireDirectoryMetadata, WireFileBegin, WireFileFetchRequest, WireHashRequest, WireMetadata,
    WireMutation, WireMutationKind, WireObservationRead, WirePath, WireScanRequest, WireScanScope,
    WireSignatureRequest, WireSourceMetadataRead, DIRECTORY_FINALIZE, DIRECTORY_READ,
    OBSERVATION_READ,
};
use crate::remote::path::{
    decode_relative_path, encode_relative_path, ensure_compatible_path_encoding, RemotePathError,
};

#[derive(Debug)]
pub enum RequestedScope {
    Tree,
    Entry(WirePath),
    Path(RelativeWirePath),
}

#[derive(Debug, thiserror::Error)]
pub enum ScopeError {
    #[error("selected operand scope mismatch for {kind:?}: selected {selected:?}, requested {requested:?}")]
    ScopeMismatch {
        kind: FrameKind,
        selected: WirePath,
        requested: RequestedScope,
    },
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
    #[error(transparent)]
    Path(#[from] RemotePathError),
    #[error(transparent)]
    Operand(#[from] crate::remote::RemoteError),
}

pub(super) fn authorize(
    resolved: &ResolvedOperand,
    frame: &Frame,
    peer: PlatformOs,
) -> Result<(), ScopeError> {
    let ResolvedOperand::Entry { name } = resolved else {
        return Ok(());
    };
    ensure_compatible_path_encoding(peer)?;
    let selected = crate::remote::operand::decode_name(name.clone())?;
    let selected_wire = encode_relative_path(selected.as_path())?;
    let mismatch = |requested| ScopeError::ScopeMismatch {
        kind: frame.kind(),
        selected: name.clone(),
        requested,
    };
    let require_selected = |path: RelativeWirePath| {
        // Compare canonical wire components, not Path::components(), which can
        // erase spellings such as chosen/. within a malicious native component.
        if path == selected_wire {
            Ok(())
        } else {
            Err(mismatch(RequestedScope::Path(path)))
        }
    };
    let payload = frame.payload();
    let path = match frame.kind() {
        FrameKind::ScanRequest => {
            return match WireScanRequest::decode(payload)?.scope {
                WireScanScope::Entry(requested) if requested == *name => Ok(()),
                WireScanScope::Entry(requested) => Err(mismatch(RequestedScope::Entry(requested))),
                WireScanScope::Tree => Err(mismatch(RequestedScope::Tree)),
            };
        }
        FrameKind::AcquireRoot => AcquireRoot::decode(payload)?.create,
        FrameKind::HashRequest => WireHashRequest::decode(payload)?.path,
        FrameKind::SignatureRequest => WireSignatureRequest::decode(payload)?.path,
        FrameKind::FileFetchRequest => WireFileFetchRequest::decode(payload)?.path,
        FrameKind::FileBegin => WireFileBegin::decode(payload)?.path,
        FrameKind::Metadata => match payload.first() {
            Some(&DIRECTORY_READ) | Some(&DIRECTORY_FINALIZE) => {
                WireDirectoryMetadata::decode(payload)?.path
            }
            Some(&OBSERVATION_READ) => WireObservationRead::decode(payload)?.path,
            _ => WireMetadata::decode(payload)?.path,
        },
        FrameKind::Mutation => {
            let mutation = WireMutation::decode(payload)?;
            if mutation.kind() == WireMutationKind::CopyFile {
                let source = mutation
                    .copy_source()
                    .ok_or(ProtocolError::InvalidMessage("copy mutation has no source"))?;
                require_selected(source.clone())?;
                // Backups may write a separate root-confined slot, never the
                // selected leaf itself (including normalized aliases).
                if decode_relative_path(mutation.path.clone(), peer)? == selected {
                    return Err(mismatch(RequestedScope::Path(mutation.path)));
                }
                return Ok(());
            }
            // A hardlink's representative is also primary authority; sharing
            // CopyFile's field must not confer its secondary-backup exception.
            if let Some(source) = mutation.copy_source() {
                require_selected(source.clone())?;
            }
            mutation.path
        }
        FrameKind::XattrRequest | FrameKind::AclRequest | FrameKind::BsdFlagsRequest => {
            WireSourceMetadataRead::decode(payload)?.path().clone()
        }
        // Direction/opener validation runs before scope admission.
        _ => return Err(ProtocolError::InvalidMessage("unsupported scoped opener").into()),
    };
    require_selected(path)
}
