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

            // Keep process alive to check memory
            // println!("Press enter to exit...");
            // std::io::stdin().read_line(&mut String::new()).unwrap();
        }
        Err(e) => eprintln!("Error: {}", e),
    }
}
