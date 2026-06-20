use clap::CommandFactory;
use sy::cli::Cli;

fn main() -> std::io::Result<()> {
    let cmd = Cli::command();
    let man = clap_mangen::Man::new(cmd);
    let mut buffer = Vec::new();
    man.render(&mut buffer)?;

    let path = std::path::Path::new("man/sy.1");
    std::fs::create_dir_all("man")?;
    std::fs::write(path, buffer)?;

    eprintln!("Generated man page: {}", path.display());
    Ok(())
}
