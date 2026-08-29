from pathlib import Path

remote = Path("src/remote/mod.rs")
text = remote.read_text()
marker = "pub mod router;\n"
if "pub mod serve;\n" not in text:
    if marker not in text:
        raise SystemExit("remote module marker missing")
    text = text.replace(marker, marker + "pub mod serve;\n", 1)
    remote.write_text(text)

transport = Path("src/transport/mod.rs")
text = transport.read_text()
marker = '#[cfg(feature = "ssh")]\npub mod ssh;\n'
addition = '#[cfg(feature = "ssh")]\npub mod v3_ssh;\n'
if addition not in text:
    if marker not in text:
        raise SystemExit("transport ssh module marker missing")
    text = text.replace(marker, marker + addition, 1)
    transport.write_text(text)

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
