#![forbid(unsafe_code)]

use std::process::ExitCode;

#[cfg(unix)]
fn main() -> ExitCode {
    let args = std::env::args().skip(1).collect();
    let code = gantryd::cli::run(args, &mut std::io::stdout(), &mut std::io::stderr());
    ExitCode::from(code)
}

#[cfg(not(unix))]
fn main() -> ExitCode {
    eprintln!("gantry: host commands run on the Linux server only");
    ExitCode::FAILURE
}
