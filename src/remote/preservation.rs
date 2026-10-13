//! Mandatory preservation admission, shared by both endpoint roles and directions.
use crate::endpoint::Capabilities;
use crate::remote::transfer::RemoteTransferError;

#[derive(Debug, Clone, Copy, Default)]
pub struct PreservationRequest {
    pub xattrs: bool,
    pub acls: bool,
    pub hardlinks: bool,
    pub flags: bool,
}

impl PreservationRequest {
    /// Each endpoint must implement the requested semantics. An empty source
    /// tree or a plan without file transfers does not waive a mandatory request.
    pub fn validate(self, capabilities: &Capabilities) -> Result<(), RemoteTransferError> {
        for (requested, supported, feature) in [
            (self.xattrs, capabilities.preserve_xattrs, "xattrs"),
            (self.acls, capabilities.preserve_acls, "ACLs"),
            (self.hardlinks, capabilities.preserve_hardlinks, "hardlinks"),
            (self.flags, capabilities.preserve_flags, "BSD flags"),
        ] {
            if requested && !supported {
                return Err(RemoteTransferError::PreservationUnavailable { feature });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mandatory_requests_require_each_endpoint_not_a_union_of_features() {
        let xattrs_only = Capabilities {
            preserve_xattrs: true,
            ..Default::default()
        };
        let acls_only = Capabilities {
            preserve_acls: true,
            ..Default::default()
        };
        let request = PreservationRequest {
            xattrs: true,
            acls: true,
            ..Default::default()
        };
        for capabilities in [xattrs_only, acls_only] {
            assert!(request.validate(&capabilities).is_err());
        }
        let both = Capabilities {
            preserve_xattrs: true,
            preserve_acls: true,
            ..Default::default()
        };
        assert!(request.validate(&both).is_ok());
        for request in [
            PreservationRequest {
                hardlinks: true,
                ..Default::default()
            },
            PreservationRequest {
                flags: true,
                ..Default::default()
            },
        ] {
            assert!(request.validate(&both).is_err());
        }
        assert!(PreservationRequest::default()
            .validate(&Capabilities::default())
            .is_ok());
    }
}
