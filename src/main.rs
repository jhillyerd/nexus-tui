//! `nexus-tui`: pair or load the stored credential, connect, run the UI,
//! exit with the contract's code (0 clean, 2 connect failure, 4 credential
//! rejected).

mod boot;

use std::process::ExitCode;

use clap::Parser;

/// The exit-code contract (tui-pairing.md): 4 on WS 401, 2 on other
/// connect failures, 0 clean.
const EXIT_CONNECT_FAILED: u8 = 2;

#[derive(Parser, Debug)]
#[command(
    name = "nexus-tui",
    about = "A nexus TUI node: pair, connect, render the thread.",
    version
)]
pub struct Args {
    /// A pairing URL (`http://host:port/pair/<code>`). Bare relaunches use
    /// the stored credential instead.
    pub pair_url: Option<String>,

    /// Forget every stored credential under the node name.
    #[arg(long)]
    pub reset: bool,
}

fn main() -> ExitCode {
    let args = Args::parse();
    let _ = color_eyre::install();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    match runtime.block_on(boot::run(args)) {
        Ok(code) => ExitCode::from(code),
        Err(report) => {
            eprintln!("{report}");
            ExitCode::from(EXIT_CONNECT_FAILED)
        }
    }
}
