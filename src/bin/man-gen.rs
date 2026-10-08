use anyhow::{bail, Context, Result};
use std::io::Write;
use std::process::{Command, Stdio};
use sy::cli::Arguments;

/// Generate sy's manual, or print a native shell completion script.
#[derive(usage::Cli)]
#[usage(bin = "man-gen", unknown_flags = "error", args_override_self = false)]
struct Args {
    /// Print completions instead of writing man/sy.1
    #[usage(long, choices("bash", "fish", "zsh", "powershell", "nu"))]
    completion: Option<String>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    if let Some(shell) = args.completion {
        let shell =
            usage::complete::Shell::from_name(&shell).context("unsupported completion shell")?;
        print!("{}", Arguments::completion_script(shell));
        return Ok(());
    }

    // usage-rs owns the declaration; usage's documentation generator consumes
    // its KDL. Generation is a development task, not a runtime dependency of sy.
    let mut child = Command::new("usage")
        .args(["generate", "manpage", "--file", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .context("man generation requires the usage v6 CLI on PATH")?;
    let write_result = child
        .stdin
        .take()
        .context("usage generator stdin was not piped")?
        .write_all(Arguments::to_kdl().as_bytes());
    let output = child
        .wait_with_output()
        .context("wait for usage generator")?;
    write_result.context("send CLI spec to usage generator")?;
    if !output.status.success() {
        bail!("usage manpage generator failed: {}", output.status);
    }

    let path = std::path::Path::new("man/sy.1");
    std::fs::create_dir_all("man")?;
    std::fs::write(path, output.stdout)?;
    eprintln!("Generated man page: {}", path.display());
    Ok(())
}
