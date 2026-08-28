from pathlib import Path


def replace_required(path: str, old: str, new: str, marker: str) -> None:
    file = Path(path)
    text = file.read_text()
    if marker in text:
        return
    if old not in text:
        raise SystemExit(f"required patch anchor not found in {path}: {marker}")
    file.write_text(text.replace(old, new, 1))


def replace_optional(path: str, old: str, new: str, marker: str) -> None:
    file = Path(path)
    text = file.read_text()
    if marker in text:
        return
    if old in text:
        file.write_text(text.replace(old, new, 1))


replace_required(
    "src/remote/mod.rs",
    """    endpoint_capabilities(&Capabilities::local())
        | CapabilitySet::BLAKE3
        | CapabilitySet::RAW_PATHS
        | CapabilitySet::MULTIPLEXING
}""",
    """    let capabilities = endpoint_capabilities(&Capabilities::local())
        | CapabilitySet::BLAKE3
        | CapabilitySet::RAW_PATHS
        | CapabilitySet::MULTIPLEXING;

    // Rolling-signature basis reads are advertised only where RootedFs can
    // enforce held-directory-FD confinement for every peer-controlled path.
    #[cfg(unix)]
    {
        capabilities | CapabilitySet::ROLLING_SIGNATURES
    }
    #[cfg(not(unix))]
    {
        capabilities
    }
}""",
    "capabilities | CapabilitySet::ROLLING_SIGNATURES",
)

replace_required(
    "src/remote/mod.rs",
    """        assert!(!client
            .ready
            .capabilities
            .contains(CapabilitySet::ROLLING_SIGNATURES));""",
    """        assert_eq!(
            client
                .ready
                .capabilities
                .contains(CapabilitySet::ROLLING_SIGNATURES),
            cfg!(unix)
        );""",
    "cfg!(unix)\n        );",
)

replace_optional(
    "agent-context/ai/0.5-design-audit.md",
    """User-facing default: `compression=auto`.

Auto policy should eventually consider:""",
    """User-facing default: `compression=auto`.

The default optimization objective is **minimum end-to-end sync completion time**. `auto` compares the estimated elapsed time of sending raw bytes with the compressed streaming pipeline (encode -> transport -> decode), including startup cost and steady-state overlap. Compression ratio, codec throughput, link throughput, chunk size, SSH overhead, and CPU pressure matter only insofar as they change completion time or interact with scheduler resource budgets. If the estimates tie, send raw.

Auto policy should eventually consider:""",
    "minimum end-to-end sync completion time",
)

replace_optional(
    "agent-context/ai/0.5-rewrite.md",
    """- panic-free production frame decoder.

Relative paths are delimiter-free component sequences, not slash-delimited strings.""",
    """- panic-free production frame decoder;
- central post-handshake frame routing with bounded byte/frame budgets and stream multiplexing;
- ordered remote scan streams;
- demand-driven adaptive rolling-signature streams, with basis reads confined through `RootedFs` on Unix.

Relative paths are delimiter-free component sequences, not slash-delimited strings.""",
    "central post-handshake frame routing",
)

replace_optional(
    "agent-context/ai/0.5-rewrite.md",
    """Lexical relative-path validation is not a security boundary. Local and remote filesystem endpoints should ultimately resolve relative to held root directory handles. Linux should prefer `openat2`/`RESOLVE_BENEATH`; macOS/other Unix should walk directory FDs with no-follow semantics.""",
    """Lexical relative-path validation is not a security boundary. Local and remote filesystem endpoints should resolve peer-influenced paths through one canonical held-directory-FD resolver with component-wise no-follow semantics across Unix. Linux `openat2(RESOLVE_BENEATH, ...)` may be added only as a transparent optimization after semantic-equivalence review and benchmarks.""",
    "one canonical held-directory-FD resolver",
)

replace_optional(
    "agent-context/ai/0.5-rewrite.md",
    """The remote agent must confine every operation to a held root directory handle. On Linux use `openat2`/`RESOLVE_BENEATH` style resolution; on macOS walk directory FDs with no-follow semantics.""",
    """The remote agent must confine every peer-influenced operation to a held root directory handle. Use one canonical component-wise directory-FD resolver with no-follow semantics across Unix. Linux `openat2(RESOLVE_BENEATH, ...)` may be added only as a transparent optimization if its semantics are equivalent and benchmarks justify it; it is not a separate fast/insecure mode.""",
    "it is not a separate fast/insecure mode",
)
