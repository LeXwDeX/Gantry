#![forbid(unsafe_code)]

use std::process::ExitCode;

#[cfg(unix)]
#[tokio::main]
async fn main() -> ExitCode {
    let options = match gantryd::Options::parse(std::env::args().skip(1)) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("gantryd: {error}");
            return ExitCode::from(2);
        }
    };
    match gantryd::run(options).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("gantryd: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(unix))]
fn main() -> ExitCode {
    eprintln!("gantryd: the server runs on Linux only");
    ExitCode::FAILURE
}
