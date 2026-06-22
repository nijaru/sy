use clap::Parser;
use std::path::PathBuf;
use std::time::Instant;
use sy::sync::scanner::Scanner;

#[derive(Parser, Debug)]
#[command(name = "sy-scan")]
struct Args {
    root: PathBuf,
}

fn main() {
    let args = Args::parse();
    let start = Instant::now();

    println!("Scanning {:?}", args.root);

    let scanner = Scanner::new(args.root);
    match scanner.scan() {
        Ok(files) => {
            let duration = start.elapsed();
            println!("Scanned {} files in {:.2?}", files.len(), duration);
        }
        Err(e) => eprintln!("Error: {}", e),
    }
}
