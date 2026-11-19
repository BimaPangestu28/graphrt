//! The `graphrt` command-line tool.

use std::process::ExitCode;

use clap::Parser;
use graphrt_cli::args::Cli;

#[cfg(feature = "mimalloc")]
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> ExitCode {
    let cli = Cli::parse();
    // Opt-in usage telemetry lives here, in the binary only (never in the library).
    #[cfg(feature = "telemetry")]
    let telemetry = graphrt_cli::telemetry::Session::begin(&cli.command);
    let result = graphrt_cli::execute(cli);
    #[cfg(feature = "telemetry")]
    telemetry.end();
    match result {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            if error.preformatted {
                eprint!("{error}");
            } else {
                eprintln!("error: {error}");
            }
            error.exit_code()
        }
    }
}
