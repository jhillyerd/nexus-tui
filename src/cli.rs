//! The `nexus-tui` command line: a pairing URL, `--reset`, `--role
//! viewer`, and the `NEXUS_*` env overrides with the contract's
//! precedence (env wins over the grant's pin).

use clap::Parser;
use nexus::pair::sanitize_node_name;

#[cfg(unix)]
use rustix::system::uname;

/// Last-resort host label when neither the kernel nor the environment
/// names this machine.
const FALLBACK_HOST: &str = "localhost";

/// Last-resort user when the environment names no one.
#[cfg(not(unix))]
const FALLBACK_USER: &str = "user";

#[derive(Parser, Debug)]
#[command(
    name = "nexus-tui",
    about = "A nexus TUI node: pair, connect, render the thread.",
    version
)]
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
            node: std::env::var("NEXUS_NODE").unwrap_or_else(|_| default_node()),
            role: nexus::client::Role::parse(
                &std::env::var("NEXUS_ROLE")
                    .unwrap_or_else(|_| self.role.clone().unwrap_or_else(|| "env".to_owned())),
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

/// The default node identity: `<user>-<host>`, lowercased and folded
/// onto the server's charset (an `@` cannot ride the wire, so
/// `james@nixos` pairs as `james-nixos`).
fn default_node() -> String {
    sanitize_node_name(&format!("{}-{}", default_user(), default_host()).to_lowercase())
}

#[cfg(unix)]
fn default_user() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| rustix::process::getuid().as_raw().to_string())
}

#[cfg(not(unix))]
fn default_user() -> String {
    std::env::var("USERNAME").unwrap_or_else(|_| FALLBACK_USER.to_owned())
}

/// The short hostname: the kernel's nodename, cut at the first dot.
#[cfg(unix)]
fn default_host() -> String {
    let uts = uname();
    let nodename = uts.nodename().to_string_lossy();
    let label = nodename.split('.').next().unwrap_or_default();
    if label.is_empty() {
        FALLBACK_HOST.to_owned()
    } else {
        label.to_owned()
    }
}

#[cfg(not(unix))]
fn default_host() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_else(|_| FALLBACK_HOST.to_owned())
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{Cli, default_node};
    use nexus::pair::validate_node_name;

    const PAIR_URL: &str = "http://host:8080/pair/code";

    #[test]
    fn role_flag_parses() {
        let cli =
            Cli::try_parse_from(["nexus-tui", PAIR_URL, "--role", "viewer"]).expect("args parse");
        assert_eq!(cli.role.as_deref(), Some("viewer"));
    }

    #[test]
    fn role_defaults_to_unset() {
        let cli = Cli::try_parse_from(["nexus-tui", PAIR_URL]).expect("args parse");
        assert_eq!(cli.role, None);
    }

    #[test]
    fn default_node_pairs_from_this_machine() {
        let node = default_node();
        assert!(!node.is_empty());
        assert_eq!(
            validate_node_name(&node),
            Ok(()),
            "derived default must pass the server's rule"
        );
    }
}
