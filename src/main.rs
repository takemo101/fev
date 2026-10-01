mod config;
mod execution;
mod model;
mod runtime;

use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
#[command(version, about = "Run commands on native filesystem events")]
struct Args {
    /// YAML configuration file. Relative root/state paths resolve beside this file.
    #[arg(short, long, value_name = "FILE")]
    config: PathBuf,
}

fn main() -> std::process::ExitCode {
    let args = Args::parse();
    match config::Config::load(&args.config).and_then(runtime::run) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("fev: {error:#}");
            std::process::ExitCode::FAILURE
        }
    }
}
