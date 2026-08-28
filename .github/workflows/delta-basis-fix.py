from pathlib import Path

p = Path("src/remote/runtime.rs")
text = p.read_text()
old = '''    ) -> Result<Option<BasisIndex>> {
        let block_size = choose_signature_block_size(basis.size);
'''
new = '''    ) -> Result<Option<BasisIndex>> {
        if !self
            .ready
            .capabilities
            .contains(CapabilitySet::ROLLING_SIGNATURES)
        {
            return Err(RemoteSignatureError::UnsupportedByPeer.into());
        }
        if !basis.is_file() {
            return Err(RemoteSignatureError::InvalidBasis.into());
        }
        if basis.identity.is_none() {
            return Err(RemoteSignatureError::MissingBasisIdentity.into());
        }

        let block_size = choose_signature_block_size(basis.size);
'''
if old not in text:
    raise SystemExit("delta_basis validation anchor missing")
text = text.replace(old, new, 1)
text = text.replace(
    "BasisIndexLimits::default().max_blocks as u64",
    "u64::try_from(BasisIndexLimits::default().max_blocks).unwrap()",
    1,
)
p.write_text(text)
