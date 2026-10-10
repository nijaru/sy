use std::fs;
use std::process::Command;
use tempfile::TempDir;

#[test]
fn compression_options_preserve_local_copy_bytes() {
    let source = TempDir::new().unwrap();
    let content = "Hello World\n".repeat(100_000);
    fs::write(source.path().join("compressible.txt"), &content).unwrap();
    fs::write(source.path().join("small.txt"), b"tiny").unwrap();

    // Local sync never sends encoded wire data, regardless of the requested
    // remote compression policy. Exercise the real entrypoint, not a simulated
    // codec round trip or the removed whole-file size/extension heuristics.
    for mode in ["auto", "extension", "always", "never"] {
        let dest = TempDir::new().unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_sy"))
            .arg(format!("{}/", source.path().display()))
            .arg(dest.path())
            .arg(format!("--compress={mode}"))
            .arg("--no-hooks")
            .output()
            .unwrap();

        assert!(
            output.status.success(),
            "{mode}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            fs::read(dest.path().join("compressible.txt")).unwrap(),
            content.as_bytes(),
            "{mode}"
        );
        assert_eq!(
            fs::read(dest.path().join("small.txt")).unwrap(),
            b"tiny",
            "{mode}"
        );
    }
}
