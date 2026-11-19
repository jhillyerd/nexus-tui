//! `nexus-tui`: pair or load the stored credential, connect, run the UI,
//! exit with the contract's code (0 clean, 2 connect failure, 4 credential
//! rejected).

mod boot;
mod cli;

use std::process::ExitCode;

use clap::Parser;

/// The exit code for boot failures (bad pairing URL, pairing refused) and
/// unexpected errors.
const EXIT_CONNECT_FAILED: u8 = 2;

fn main() -> ExitCode {
    let cli = cli::Cli::parse();
    let _ = color_eyre::install();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    match runtime.block_on(boot::run(&cli)) {
        Ok(code) => ExitCode::from(code),
        Err(report) => {
            eprintln!("{report}");
            ExitCode::from(EXIT_CONNECT_FAILED)
        }
    }
}
