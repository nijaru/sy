from pathlib import Path
import subprocess

BASE = "ebc12d6a0f824478cf343b815a06b30951fbdb1f"

# Restore remote/mod.rs exactly from the known-good parent, then add only the
# new v3 module exports. This prevents a truncated contents read from rewriting
# unrelated control-plane tests.
subprocess.run(["git", "fetch", "origin", BASE, "--depth=1"], check=True)
text = subprocess.check_output(["git", "show", f"{BASE}:src/remote/mod.rs"], text=True)
marker = "pub mod scan;\n"
addition = "pub mod serve;\n"
if addition not in text:
    if marker not in text:
        raise SystemExit("remote scan module marker missing")
    text = text.replace(marker, marker + addition, 1)
marker = "pub mod signature;\n"
addition = '#[cfg(feature = "ssh")]\npub mod ssh;\n'
if addition not in text:
    if marker not in text:
        raise SystemExit("remote signature module marker missing")
    text = text.replace(marker, marker + addition, 1)
Path("src/remote/mod.rs").write_text(text)

# The private agent must bypass Clap/config/log initialization so stdout is
# protocol-only from the first byte. SessionOpen carries the remote root.
main = Path("src/main.rs")
text = main.read_text()
old = '''#[tokio::main]
async fn main() {
    // Parse CLI arguments
    let mut cli = Cli::parse();

    // Set RUST_BACKTRACE=0 unless user explicitly set it
    if std::env::var("RUST_BACKTRACE").is_err() {
        std::env::set_var("RUST_BACKTRACE", "0");
    }

    if let Err(e) = run(&mut cli).await {
        eprintln!("Error: {:#}", e);
        std::process::exit(1);
    }
}
'''
new = '''#[tokio::main]
async fn main() {
    // Set RUST_BACKTRACE=0 unless user explicitly set it.
    if std::env::var("RUST_BACKTRACE").is_err() {
        std::env::set_var("RUST_BACKTRACE", "0");
    }

    // The private v3 SSH agent bypasses Clap and all normal CLI setup so stdout
    // remains protocol-only from the first byte. The remote root is negotiated
    // in SessionOpen; it is deliberately not accepted as an argv pathname.
    let mut raw_args = std::env::args_os();
    let _program = raw_args.next();
    if raw_args.next().as_deref() == Some(std::ffi::OsStr::new("__serve")) {
        if raw_args.next().is_some() {
            eprintln!("Error: __serve does not accept command-line arguments");
            std::process::exit(2);
        }
        if let Err(error) = sy::remote::serve::run_stdio().await {
            eprintln!("Error: {error}");
            std::process::exit(1);
        }
        return;
    }

    // Parse user-facing CLI arguments only after private-agent dispatch.
    let mut cli = Cli::parse();
    if let Err(e) = run(&mut cli).await {
        eprintln!("Error: {:#}", e);
        std::process::exit(1);
    }
}
'''
if old not in text:
    raise SystemExit("main entrypoint marker missing")
text = text.replace(old, new, 1)
main.write_text(text)
