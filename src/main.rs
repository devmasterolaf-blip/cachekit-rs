use std::io::{self, Write};
use std::process::ExitCode;

use cachekit::cli::{self, CliError};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let stdin = io::stdin();
    let mut out = io::BufWriter::new(io::stdout().lock());

    let result = cli::run(&args, &mut stdin.lock(), &mut out)
        .and_then(|()| out.flush().map_err(CliError::Output));
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("cachekit: {error}");
            if matches!(error, CliError::Usage(_)) {
                eprintln!("Run 'cachekit help' for usage.");
            }
            ExitCode::from(error.exit_code())
        }
    }
}
