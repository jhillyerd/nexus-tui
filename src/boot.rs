//! Credential resolution and the UI run: the pairing exchange, the state
//! file, the ambiguity messages, and the exit codes.

use color_eyre::eyre::{self, Context};
use nexus::client::NodeConfig;
use nexus::pair;
use nexus::state::{self, Credential};

use crate::cli::{Boot, Cli};

/// The UI run's outcome: the process exit code.
const EXIT_CLEAN: u8 = 0;
const EXIT_CONNECT_FAILED: u8 = 2;
const EXIT_CREDENTIAL_REJECTED: u8 = 4;

pub async fn run(cli: &Cli) -> Result<u8, eyre::Report> {
    let Boot {
        node,
        role,
        thread: thread_override,
        server: server_filter,
        pair_url,
    } = cli.boot();

    if cli.reset {
        let removed = state::reset(&state::state_root(), &node);
        for path in &removed {
            println!("removed {}", path.display());
        }
        if removed.is_empty() {
            println!("no stored credential for {node}");
        }
    }

    let root = state::state_root();
    let credential = match state::discover(
        &root,
        &node,
        server_filter.as_deref(),
        thread_override.as_deref(),
    ) {
        state::Discovery::Found { credential, .. } => credential,
        state::Discovery::Ambiguous(matches) => {
            eprintln!("several stored credentials for {node}; disambiguate:");
            for (path, credential) in &matches {
                eprintln!("  {} (thread {})", path.display(), credential.thread);
            }
            eprintln!("  relaunch with the pairing URL, or set NEXUS_URL");
            return Ok(EXIT_CONNECT_FAILED);
        }
        state::Discovery::Missing => {
            let Some(url) = pair_url.as_deref() else {
                eprintln!("no stored credential for {node} — pass a pairing URL:");
                eprintln!("  nexus-tui http://<server>/pair/<code>");
                return Ok(EXIT_CONNECT_FAILED);
            };
            pair_new(url, &node, role, thread_override.as_deref(), &root).await?
        }
    };

    run_ui(credential, role, pair_url).await
}

/// Exchange a pairing code and persist the minted credential.
async fn pair_new(
    url: &str,
    node: &str,
    role: nexus::client::Role,
    thread_override: Option<&str>,
    root: &std::path::Path,
) -> Result<Credential, eyre::Report> {
    let parsed = pair::parse_pair_url(url).wrap_err("bad pairing URL")?;
    if let Err(reason) = nexus::pair::validate_node_name(node) {
        return Err(eyre::eyre!("bad node name: {reason}"));
    }
    let paired = pair::exchange(&parsed.base, &parsed.code, node, thread_override)
        .await
        .wrap_err("pairing failed")?;
    let credential = Credential {
        server: parsed.base,
        node: paired.name,
        token: paired.token,
        thread: paired.thread_id,
    };
    let path = state::save(root, &credential).wrap_err("saving the credential")?;
    tracing::info!(path = %path.display(), thread = %credential.thread, "paired");
    let _ = role;
    Ok(credential)
}

/// Connect and run the UI. A 401 on a live join turns into a re-pair when
/// a pairing URL is held (the launcher's contract), else exit 4.
async fn run_ui(
    credential: Credential,
    role: nexus::client::Role,
    pair_url: Option<String>,
) -> Result<u8, eyre::Report> {
    let config = NodeConfig {
        server: credential.server.clone(),
        node: credential.node.clone(),
        token: credential.token,
        thread: credential.thread.clone(),
        role,
        workspace: std::env::current_dir().wrap_err("no working directory")?,
        timeout_ms: nexus::env::timeout_ms_from_env(),
        max_output_bytes: nexus::env::max_output_bytes_from_env(),
    };
    let node = maki_ui::NodeHandle::spawn(config);
    let events = node
        .take_events()
        .ok_or_else(|| eyre::eyre!("node event stream already taken"))?;

    // The lua host feeds keymaps, input decor and config; its agent-coupled
    // plugins are inert here.
    let mut host = maki_lua::PluginHost::start_standalone(maki_lua::Interaction::Tty, true)
        .wrap_err("initialize lua plugin host")?;
    let (ui_config, warnings) = maki_ui::load_config(&mut host)?;
    for warning in &warnings {
        eprintln!("{warning}");
    }

    let app = maki_ui::build_app(
        maki_storage::id::MakiId::generate(),
        node.clone(),
        maki_storage::StateDir::from_path(state::state_root()),
        ui_config.clone(),
        &host,
    )?;

    let outcome = maki_ui::run(maki_ui::EventLoopParams {
        events,
        ui_actions: host.take_ui_action_rx(),
        app,
        max_log_bytes: 1 << 20,
        ui_config,
    })
    .await?;

    drop(host);
    match outcome {
        maki_ui::RunOutcome::Exit { code: 0 } => Ok(EXIT_CLEAN),
        maki_ui::RunOutcome::Exit { code: 4 } => {
            // Credential swept or revoked mid-run: re-pair if a URL is held.
            if let Some(url) = pair_url.as_deref() {
                eprintln!("credential rejected — re-pairing");
                let parsed = pair::parse_pair_url(url)?;
                let node_name = credential.node.clone();
                let paired = pair::exchange(&parsed.base, &parsed.code, &node_name, None).await?;
                let fresh = Credential {
                    server: parsed.base,
                    node: paired.name,
                    token: paired.token,
                    thread: paired.thread_id,
                };
                state::save(&state::state_root(), &fresh)?;
                let _ = role;
                Box::pin(run_ui(fresh, role, None)).await
            } else {
                eprintln!("credential rejected — pair again (pass a pairing URL)");
                Ok(EXIT_CREDENTIAL_REJECTED)
            }
        }
        maki_ui::RunOutcome::Exit { code: _ } => Ok(EXIT_CONNECT_FAILED),
    }
}
