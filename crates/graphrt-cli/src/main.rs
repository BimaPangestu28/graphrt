//! The `graphrt` command-line tool.

use std::process::ExitCode;

use clap::Parser;
use graphrt_cli::args::Cli;

fn main() -> ExitCode {
    let cli = Cli::parse();
    match graphrt_cli::execute(cli) {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("error: {error}");
            error.exit_code()
        }
    }
}
