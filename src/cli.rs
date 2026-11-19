//! The `nexus-tui` command line: a pairing URL, `--reset`, `--role
//! viewer`, and the `NEXUS_*` env overrides with the contract's
//! precedence (env wins over the grant's pin).

use clap::Parser;

/// Default node name, used when neither `NEXUS_NODE` nor a stored
/// credential names the host.
const DEFAULT_NODE: &str = "workspace-tui";

#[derive(Parser, Debug)]
#[command(name = "nexus-tui", about = "A nexus TUI node: pair, connect, render the thread.", version)]
pub struct Cli {
    /// A pairing URL (`http://host:port/pair/<code>`). Bare relaunches
    /// use the stored credential instead.
    pub pair_url: Option<String>,

    /// Forget every stored credential under the node name.
    #[arg(long)]
    pub reset: bool,

    /// Join read-only: events + send, no tool plane.
    #[arg(long)]
    pub role: Option<String>,
}

/// The resolved boot identity, after the URL, the env and the state file
/// have had their say.
#[derive(Debug, Clone)]
pub struct Boot {
    pub node: String,
    pub role: nexus::client::Role,
    pub thread: Option<String>,
    pub server: Option<String>,
    pub pair_url: Option<String>,
}

impl Cli {
    pub fn boot(&self) -> Boot {
        Boot {
            node: std::env::var("NEXUS_NODE").unwrap_or_else(|_| DEFAULT_NODE.to_owned()),
            role: nexus::client::Role::parse(
                &std::env::var("NEXUS_ROLE").unwrap_or_else(|_| {
                    self.role.clone().unwrap_or_else(|| "env".to_owned())
                }),
            ),
            thread: std::env::var("NEXUS_THREAD").ok().filter(|t| !t.is_empty()),
            server: std::env::var("NEXUS_URL").ok().filter(|t| !t.is_empty()),
            pair_url: std::env::var("NEXUS_PAIR_URL")
                .ok()
                .filter(|t| !t.is_empty())
                .or_else(|| self.pair_url.clone()),
        }
    }
}
