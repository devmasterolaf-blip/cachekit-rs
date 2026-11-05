use std::io::{self, Write};
use std::process::ExitCode;

use cachekit::cli::{self, CliError};

fn main() -> ExitCode {
    let mut args = Vec::new();
    for arg in std::env::args_os().skip(1) {
        match arg.into_string() {
            Ok(arg) => args.push(arg),
            Err(raw) => {
                eprintln!("cachekit: argument {raw:?} is not valid UTF-8");
                return ExitCode::from(2);
            }
        }
    }
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
