# Release Process

Guide me through releasing a new version of sy.

## Steps to perform:

1. **Version Bump**: Update version in Cargo.toml
2. **Update Docs**: Ensure CHANGELOG.md or relevant docs are updated
3. **Commit**: Create commit with version bump
4. **Push**: Push to main
5. **Wait for CI**: Verify CI passes with `gh run watch`
6. **Create Tag**: `git tag -a vX.Y.Z -m "Release vX.Y.Z" && git push --tags`
7. **Wait for Release Workflow**: The release workflow will:
   - Create draft release with auto-generated notes
   - Build binaries for macOS (ARM/Intel) and Linux
   - Upload binaries and checksums
   - Update Homebrew tap (nijaru/homebrew-tap)
   - Publish the release
8. **Publish to crates.io**: After confirming release looks good, run `cargo publish`

## Notes:
- Release workflow uses actions/checkout@v5
- Binaries are built for: aarch64-apple-darwin, x86_64-apple-darwin, x86_64-unknown-linux-gnu
- Homebrew tap updates automatically (requires TAP_UPDATE_TOKEN secret in GitHub)
- Release starts as draft, gets published automatically after binaries upload and tap update
- Crate publishing is MANUAL - wait for explicit approval
- Follow semantic versioning: 0.0.x -> 0.0.y -> 0.1.0 -> 1.0.0 (sequential only)

## Prerequisites:
- TAP_UPDATE_TOKEN secret must be set in GitHub repo settings (personal access token with repo scope)

Ask me what version to bump to and proceed step by step, waiting for confirmation before each push/tag/publish operation.
