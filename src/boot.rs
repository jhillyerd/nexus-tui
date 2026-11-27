//! Credential resolution and the UI run: the pairing exchange, the state
//! file, the ambiguity messages, and the exit codes.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use color_eyre::eyre::{self, Context};
use maki_lua::{Interaction, PluginHost};
use maki_storage::StateDir;
use maki_storage::id::MakiId;
use maki_ui::{EventLoopParams, NodeHandle, RunOutcome};
use nexus::client::{NodeConfig, Role};
use nexus::pair::{self, PairError, PairUrl, Paired};
use nexus::state::{self, Credential, Discovery};

use crate::cli::{Boot, Cli};

/// The UI run's outcome: the process exit code.
const EXIT_CLEAN: u8 = 0;
const EXIT_CONNECT_FAILED: u8 = 2;
const EXIT_CREDENTIAL_REJECTED: u8 = 4;

const AMBIGUOUS_HINT: &str =
    "  set NEXUS_THREAD (or NEXUS_URL) to pick one, or pass a pairing URL to pair a new thread";
const MISSING_HINT: &str = "  nexus-tui http://<server>/pair/<code>";
const RECONNECT_HINT: &str = "  to reconnect to an existing thread, launch without the pairing URL (or set NEXUS_THREAD=<thread>)";

#[derive(Debug, PartialEq, Eq)]
enum Resolution {
    Ready(Credential),
    Blocked(String),
}

pub async fn run(cli: &Cli) -> Result<u8, eyre::Report> {
    let boot = cli.boot();
    let root = state::state_root();

    if cli.reset {
        let removed = state::reset(&root, &boot.node);
        for path in &removed {
            println!("removed {}", path.display());
        }
        if removed.is_empty() {
            println!("no stored credential for {}", boot.node);
        }
        if boot.pair_url.is_none() {
            return Ok(EXIT_CLEAN);
        }
    }

    let credential = match resolve_credential(&root, &boot).await? {
        Resolution::Ready(credential) => credential,
        Resolution::Blocked(message) => {
            eprintln!("{message}");
            return Ok(EXIT_CONNECT_FAILED);
        }
    };

    run_ui(credential, boot.role, boot.pair_url, boot.thread).await
}

/// Resolve the credential to connect with.
///
/// Pairing URLs are opaque on the client — only the server knows which thread a
/// code is pinned to — so a pairing URL exchanges directly unless `NEXUS_THREAD`
/// already pins an exact stored `(server, node, thread)` credential. Bare
/// relaunches discover by node name with optional `NEXUS_URL` / `NEXUS_THREAD`
/// narrowing.
async fn resolve_credential(root: &Path, boot: &Boot) -> Result<Resolution, eyre::Report> {
    if let Some(url) = boot.pair_url.as_deref() {
        let parsed = pair::parse_pair_url(url).wrap_err("bad pairing URL")?;
        if let Some(thread) = boot.thread.as_deref()
            && let Discovery::Found { credential, .. } =
                state::discover(root, &boot.node, Some(&parsed.base), Some(thread))
        {
            return Ok(Resolution::Ready(credential));
        }
        return pair_or_explain(root, &parsed, &boot.node, boot.thread.as_deref()).await;
    }

    match state::discover(
        root,
        &boot.node,
        boot.server.as_deref(),
        boot.thread.as_deref(),
    ) {
        Discovery::Found { credential, .. } => Ok(Resolution::Ready(credential)),
        Discovery::Ambiguous(matches) => {
            Ok(Resolution::Blocked(format_ambiguous(&boot.node, &matches)))
        }
        Discovery::Missing => Ok(Resolution::Blocked(format_missing(&boot.node))),
    }
}

async fn pair_or_explain(
    root: &Path,
    parsed: &PairUrl,
    node: &str,
    thread_override: Option<&str>,
) -> Result<Resolution, eyre::Report> {
    if let Err(reason) = pair::validate_node_name(node) {
        return Err(eyre::eyre!("bad node name: {reason}"));
    }
    match pair::exchange(&parsed.base, &parsed.code, node, thread_override).await {
        Ok(paired) => {
            let credential = save_paired(root, parsed.base.clone(), paired)?;
            Ok(Resolution::Ready(credential))
        }
        Err(PairError::NameTaken) => {
            let stored = stored_for_server(root, node, &parsed.base);
            if stored.is_empty() {
                Err(eyre::Report::new(PairError::NameTaken).wrap_err("pairing failed"))
            } else {
                Ok(Resolution::Blocked(format_name_taken_with_stored(
                    node,
                    &parsed.base,
                    &stored,
                )))
            }
        }
        Err(err) => Err(eyre::Report::new(err).wrap_err("pairing failed")),
    }
}

fn save_paired(root: &Path, server: String, paired: Paired) -> Result<Credential, eyre::Report> {
    let credential = Credential {
        server,
        node: paired.name,
        token: paired.token,
        thread: paired.thread_id,
    };
    let path = state::save(root, &credential).wrap_err("saving the credential")?;
    tracing::info!(path = %path.display(), thread = %credential.thread, "paired");
    Ok(credential)
}

fn stored_for_server(root: &Path, node: &str, server: &str) -> Vec<(PathBuf, Credential)> {
    match state::discover(root, node, Some(server), None) {
        Discovery::Found { path, credential } => vec![(path, credential)],
        Discovery::Ambiguous(matches) => matches,
        Discovery::Missing => Vec::new(),
    }
}

fn format_ambiguous(node: &str, matches: &[(PathBuf, Credential)]) -> String {
    let mut out = format!("several stored credentials for {node}; disambiguate:\n");
    for (path, credential) in matches {
        let _ = writeln!(
            out,
            "  {} (server {}, thread {})",
            path.display(),
            credential.server,
            credential.thread
        );
    }
    out.push_str(AMBIGUOUS_HINT);
    out
}

fn format_missing(node: &str) -> String {
    format!("no stored credential for {node} — pass a pairing URL:\n{MISSING_HINT}")
}

fn format_name_taken_with_stored(
    node: &str,
    server: &str,
    stored: &[(PathBuf, Credential)],
) -> String {
    let mut out = format!(
        "pairing failed: {}\nstored credentials for {node} on {server}:\n",
        PairError::NameTaken
    );
    for (path, credential) in stored {
        let _ = writeln!(out, "  {} (thread {})", path.display(), credential.thread);
    }
    out.push_str(RECONNECT_HINT);
    out
}

/// Connect and run the UI. A 401 on a live join turns into a re-pair when
/// a pairing URL is held (the launcher's contract), else exit 4.
async fn run_ui(
    credential: Credential,
    role: Role,
    pair_url: Option<String>,
    thread_override: Option<String>,
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
    let node = NodeHandle::spawn(config);
    let events = node
        .take_events()
        .ok_or_else(|| eyre::eyre!("node event stream already taken"))?;

    // The lua host feeds keymaps, input decor and config; its agent-coupled
    // plugins are inert here.
    let mut host = PluginHost::start_standalone(Interaction::Tty, true)
        .wrap_err("initialize lua plugin host")?;
    let (ui_config, warnings) = maki_ui::load_config(&mut host)?;
    for warning in &warnings {
        eprintln!("{warning}");
    }

    let app = maki_ui::build_app(
        MakiId::generate(),
        node.clone(),
        StateDir::from_path(state::state_root()),
        ui_config.clone(),
        &host,
    )?;

    let outcome = maki_ui::run(EventLoopParams {
        events,
        ui_actions: host.take_ui_action_rx(),
        app,
        max_log_bytes: 1 << 20,
        ui_config,
    })
    .await?;

    drop(host);
    match outcome {
        RunOutcome::Exit { code: 0 } => Ok(EXIT_CLEAN),
        RunOutcome::Exit { code: 4 } => {
            // Credential swept or revoked mid-run: re-pair if a URL is held.
            if let Some(url) = pair_url.as_deref() {
                eprintln!("credential rejected — re-pairing");
                let parsed = pair::parse_pair_url(url).wrap_err("bad pairing URL")?;
                let paired = pair::exchange(
                    &parsed.base,
                    &parsed.code,
                    &credential.node,
                    thread_override.as_deref(),
                )
                .await
                .wrap_err("pairing failed")?;
                let fresh = save_paired(&state::state_root(), parsed.base, paired)?;
                Box::pin(run_ui(fresh, role, None, thread_override)).await
            } else {
                eprintln!("credential rejected — pair again (pass a pairing URL)");
                Ok(EXIT_CREDENTIAL_REJECTED)
            }
        }
        RunOutcome::Exit { code: _ } => Ok(EXIT_CONNECT_FAILED),
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    const NODE: &str = "proxmox-2";
    const CODE: &str = "pair-code-1";
    const SERVER: &str = "http://boss:8080";
    const THREAD_A: &str = "s2";
    const THREAD_B: &str = "s9";
    const THREAD_NEW: &str = "s10";

    fn stored_cred(server: &str, thread: &str, token: &str) -> Credential {
        Credential {
            server: server.to_owned(),
            node: NODE.to_owned(),
            token: token.to_owned(),
            thread: thread.to_owned(),
        }
    }

    fn boot_cfg(server: Option<&str>, thread: Option<&str>, pair_url: Option<String>) -> Boot {
        Boot {
            node: NODE.to_owned(),
            role: Role::Environment,
            thread: thread.map(str::to_owned),
            server: server.map(str::to_owned),
            pair_url,
        }
    }

    async fn spawn_pair_server(status: u16, body: String) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0_u8; 4096];
            let mut req = Vec::new();
            loop {
                let n = socket.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                req.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&req);
                if let Some(head_end) = text.find("\r\n\r\n") {
                    let length = text[..head_end]
                        .lines()
                        .find_map(|line| {
                            line.strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    if req.len() >= head_end + 4 + length {
                        break;
                    }
                }
            }
            let response = format!(
                "HTTP/1.1 {status} TEST\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;
        });
        format!("http://127.0.0.1:{port}")
    }

    fn paired_body(thread: &str, token: &str) -> String {
        serde_json::json!({
            "id": "n01",
            "name": NODE,
            "token": token,
            "thread_id": thread,
            "created_at": 1_737_000_000,
        })
        .to_string()
    }

    #[tokio::test]
    async fn pair_url_pairs_new_thread_despite_multiple_stored_credentials() {
        let root = tempfile::tempdir().unwrap();
        let server = spawn_pair_server(201, paired_body(THREAD_NEW, "tok-new")).await;
        state::save(root.path(), &stored_cred(&server, THREAD_A, "tok-a")).unwrap();
        state::save(root.path(), &stored_cred(&server, THREAD_B, "tok-b")).unwrap();

        let boot = boot_cfg(None, None, Some(format!("{server}/pair/{CODE}")));
        let resolved = resolve_credential(root.path(), &boot).await.unwrap();

        let expected = stored_cred(&server, THREAD_NEW, "tok-new");
        assert_eq!(resolved, Resolution::Ready(expected.clone()));
        assert_eq!(
            state::load(&state::state_path(root.path(), &server, NODE, THREAD_NEW)),
            Some(expected)
        );
    }

    #[tokio::test]
    async fn pair_url_pairs_new_thread_despite_single_stored_credential() {
        let root = tempfile::tempdir().unwrap();
        let server = spawn_pair_server(201, paired_body(THREAD_B, "tok-b")).await;
        state::save(root.path(), &stored_cred(&server, THREAD_A, "tok-a")).unwrap();

        let boot = boot_cfg(None, None, Some(format!("{server}/pair/{CODE}")));
        let resolved = resolve_credential(root.path(), &boot).await.unwrap();

        assert_eq!(
            resolved,
            Resolution::Ready(stored_cred(&server, THREAD_B, "tok-b"))
        );
    }

    #[tokio::test]
    async fn pair_url_with_thread_override_reuses_matching_stored_credential() {
        let root = tempfile::tempdir().unwrap();
        let server = "http://127.0.0.1:1";
        let existing = stored_cred(server, THREAD_A, "tok-a");
        state::save(root.path(), &existing).unwrap();
        state::save(root.path(), &stored_cred(server, THREAD_B, "tok-b")).unwrap();

        let boot = boot_cfg(None, Some(THREAD_A), Some(format!("{server}/pair/{CODE}")));
        let resolved = resolve_credential(root.path(), &boot).await.unwrap();

        assert_eq!(resolved, Resolution::Ready(existing));
    }

    #[tokio::test]
    async fn pair_url_409_lists_stored_credentials_when_present() {
        let root = tempfile::tempdir().unwrap();
        let server =
            spawn_pair_server(409, r#"{"error":"node name is already paired"}"#.into()).await;
        state::save(root.path(), &stored_cred(&server, THREAD_A, "tok-a")).unwrap();

        let boot = boot_cfg(None, None, Some(format!("{server}/pair/{CODE}")));
        let resolved = resolve_credential(root.path(), &boot).await.unwrap();

        let Resolution::Blocked(msg) = resolved else {
            panic!("expected Blocked, got {resolved:?}");
        };
        assert!(msg.contains(&PairError::NameTaken.to_string()));
        assert!(msg.contains(THREAD_A));
        assert!(msg.contains(RECONNECT_HINT));
    }

    #[tokio::test]
    async fn bare_relaunch_ambiguous_mentions_thread_and_server() {
        let root = tempfile::tempdir().unwrap();
        state::save(root.path(), &stored_cred(SERVER, THREAD_A, "tok-a")).unwrap();
        state::save(root.path(), &stored_cred(SERVER, THREAD_B, "tok-b")).unwrap();

        let boot = boot_cfg(None, None, None);
        let resolved = resolve_credential(root.path(), &boot).await.unwrap();

        let Resolution::Blocked(msg) = resolved else {
            panic!("expected Blocked, got {resolved:?}");
        };
        assert!(msg.contains(AMBIGUOUS_HINT));
        assert!(msg.contains(SERVER));
        assert!(msg.contains(THREAD_A));
        assert!(msg.contains(THREAD_B));
    }

    #[tokio::test]
    async fn bare_relaunch_disambiguates_by_thread() {
        let root = tempfile::tempdir().unwrap();
        state::save(root.path(), &stored_cred(SERVER, THREAD_A, "tok-a")).unwrap();
        let expected = stored_cred(SERVER, THREAD_B, "tok-b");
        state::save(root.path(), &expected).unwrap();

        let boot = boot_cfg(None, Some(THREAD_B), None);
        let resolved = resolve_credential(root.path(), &boot).await.unwrap();

        assert_eq!(resolved, Resolution::Ready(expected));
    }

    #[tokio::test]
    async fn bare_relaunch_missing_prints_pairing_hint() {
        let root = tempfile::tempdir().unwrap();
        let boot = boot_cfg(None, None, None);
        let resolved = resolve_credential(root.path(), &boot).await.unwrap();

        let Resolution::Blocked(msg) = resolved else {
            panic!("expected Blocked, got {resolved:?}");
        };
        assert!(msg.contains(MISSING_HINT));
    }
}
