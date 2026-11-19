//! The node connection (environments plan, remote nodes): an outbound
//! WebSocket to `/node/ws?node=&token=&thread=&role=` — nexus never
//! dials a node — with the real reconnect story the Gleam PoC skipped
//! (backoff after a live connection drops; the first connect and the
//! exit-code contract stay terminal).
//!
//! Two planes, one socket:
//!
//! - **Event plane**: every decoded server frame lands in the caller's
//!   channel (tool-plane frames are handled here, not forwarded — the
//!   transcript's `tool_call`/`tool_result` chips already carry the
//!   display story).
//! - **Tool plane**: a `tools/call` runs the four env tools locally
//!   ([`crate::env::serve_call`]) in a spawned task and answers
//!   `call_response`; `notifications/cancelled` re-signals the
//!   in-flight call's token.
//!
//! Exit-code contract: a WS 401 (credential revoked or swept) ends the
//! loop with [`ClientEvent::Terminated`] → exit **4**; a failed FIRST
//! connect ends with exit **2**; after a live connection, drops retry
//! with backoff until shutdown.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;

use futures::SinkExt;
use futures::StreamExt;
use serde_json::Value;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::error::ProtocolError;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::env;
use crate::protocol;

/// Liveness cadence for app-level `ping` frames (answered `pong`).
const PING_INTERVAL: Duration = Duration::from_secs(30);

/// Reconnect backoff: doubling from this, capped, with jitter.
const BACKOFF_INITIAL: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// The backoff jitter band (± half this fraction).
const BACKOFF_JITTER: f64 = 0.25;

/// How long to keep serving after a graceful close before assuming the
/// peer will never echo the close handshake.
const CLOSE_LINGER: Duration = Duration::from_secs(5);

/// The join role. `viewer` rides a read-only join (events + send + ping,
/// no tool plane); anything else joins as the environment role — the
/// server's rule, mirrored here so a typo cannot silently escalate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Role {
    #[default]
    Environment,
    Viewer,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Environment => "env",
            Role::Viewer => "viewer",
        }
    }

    /// The server accepts exactly "viewer"; every other value joins as
    /// the environment role.
    pub fn parse(raw: &str) -> Role {
        match raw {
            "viewer" => Role::Viewer,
            _ => Role::Environment,
        }
    }
}

/// Everything one node connection needs, read from config/state once
/// at boot.
#[derive(Debug, Clone)]
pub struct NodeConfig {
    /// The server's BASE url (`http://host:port`) — `/node/ws` rides on it.
    pub server: String,
    pub node: String,
    pub token: String,
    pub thread: String,
    pub role: Role,
    /// The node's LOCAL workspace root (the per-thread dir derives from it).
    pub workspace: PathBuf,
    pub timeout_ms: u64,
    pub max_output_bytes: usize,
}

/// Why the connection loop ended. Maps onto the exit-code contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Termination {
    /// WS 401: the credential was revoked or swept. Exit 4 — re-pair
    /// if a pairing URL is held.
    CredentialRejected,
    /// The first connect failed (server down, wrong path, refused
    /// env-role join). Exit 2.
    ConnectFailed(String),
    /// The shutdown token fired; clean exit 0.
    Shutdown,
}

impl Termination {
    /// The process exit code the termination maps to (the contract:
    /// 4 credential rejected, 2 other connect failures, 0 clean).
    pub fn exit_code(&self) -> i32 {
        match self {
            Termination::CredentialRejected => 4,
            Termination::ConnectFailed(_) => 2,
            Termination::Shutdown => 0,
        }
    }
}

/// What the connection loop tells the caller.
#[derive(Debug, Clone, PartialEq)]
pub enum ClientEvent {
    /// The socket is up; `hello` + replay follow as frames.
    Connected,
    /// A live connection dropped; retrying after `after`.
    Reconnecting { attempt: u32, after: Duration },
    /// One decoded event-plane frame (tool-plane frames are consumed
    /// here and never forwarded).
    Frame(protocol::ServerFrame),
    /// The loop ended — the process should exit with its code.
    Terminated(Termination),
}

/// One outbound command from the caller's UI thread.
#[derive(Debug)]
enum Command {
    /// A human turn — encoded as a `send` frame.
    Send(String),
    /// Liveness.
    Ping,
}

/// Anything the socket may write: encoded protocol frames and the
/// graceful-close marker.
#[derive(Debug)]
enum Outbound {
    Frame(String),
    Close,
}

/// The handle a caller keeps: queue user turns and pings, and cancel
/// for a clean exit.
#[derive(Clone)]
pub struct NodeClient {
    shutdown: CancellationToken,
    commands: mpsc::UnboundedSender<Command>,
}

impl NodeClient {
    /// Queue a user turn (a `send` frame when connected; dropped
    /// silently while disconnected — the UI keeps it in its input
    /// buffer's care).
    pub fn send(&self, text: impl Into<String>) {
        let _ = self.commands.send(Command::Send(text.into()));
    }

    /// Queue a liveness ping.
    pub fn ping(&self) {
        let _ = self.commands.send(Command::Ping);
    }

    /// Begin a clean shutdown: the socket closes gracefully and the
    /// event loop ends with [`Termination::Shutdown`].
    pub fn shutdown(&self) {
        self.shutdown.cancel();
    }
}

/// Build the node connection and run its loop on the current runtime.
/// Returns the caller's handle and the event stream; the loop ends
/// (and the event channel closes) when a [`Termination`] lands.
pub fn spawn(config: NodeConfig) -> (NodeClient, mpsc::UnboundedReceiver<ClientEvent>) {
    let shutdown = CancellationToken::new();
    let (commands, commands_rx) = mpsc::unbounded_channel();
    let (events, events_rx) = mpsc::unbounded_channel();
    let client = NodeClient {
        shutdown: shutdown.clone(),
        commands,
    };
    tokio::spawn(run(config, shutdown, commands_rx, events));
    (client, events_rx)
}

/// The connection loop: connect → serve → (drop → backoff → connect)*,
/// until the first connect fails, the credential is rejected, or the
/// shutdown token fires.
async fn run(
    config: NodeConfig,
    shutdown: CancellationToken,
    mut commands_rx: mpsc::UnboundedReceiver<Command>,
    events: mpsc::UnboundedSender<ClientEvent>,
) {
    let mut first_connect = true;
    let mut backoff_attempt: u32 = 0;
    loop {
        if shutdown.is_cancelled() {
            terminate(&events, Termination::Shutdown);
            return;
        }
        match connect(&config).await {
            Ok(ws) => {
                first_connect = false;
                backoff_attempt = 0;
                let _ = events.send(ClientEvent::Connected);
                let outcome = session(&config, ws, &shutdown, &mut commands_rx, &events).await;
                match outcome {
                    SessionEnd::Shutdown => {
                        terminate(&events, Termination::Shutdown);
                        return;
                    }
                    SessionEnd::Dropped(detail) => {
                        tracing::warn!(server = %config.server, detail = %detail, "connection dropped; reconnecting");
                    }
                }
            }
            Err(ConnectError::Unauthorized { server }) => {
                tracing::warn!(server = %server, "credential rejected (401)");
                terminate(&events, Termination::CredentialRejected);
                return;
            }
            Err(ConnectError::Failed { server, detail }) => {
                if first_connect {
                    // The plan: exit 2 on connect failures — a first
                    // connect that fails is terminal, not a retry
                    // treadmill over a wrong URL.
                    tracing::warn!(server = %server, detail = %detail, "cannot connect");
                    terminate(&events, Termination::ConnectFailed(detail));
                    return;
                }
                backoff_attempt += 1;
                let after = backoff(backoff_attempt);
                tracing::info!(server = %server, attempt = backoff_attempt, after_ms = after.as_millis() as u64, detail = %detail, "reconnecting");
                let _ = events.send(ClientEvent::Reconnecting {
                    attempt: backoff_attempt,
                    after,
                });
                if tokio::select! {
                    () = tokio::time::sleep(after) => false,
                    () = shutdown.cancelled() => true,
                } {
                    terminate(&events, Termination::Shutdown);
                    return;
                }
            }
        }
    }
}

fn terminate(events: &mpsc::UnboundedSender<ClientEvent>, termination: Termination) {
    let _ = events.send(ClientEvent::Terminated(termination));
}

/// Doubling backoff with jitter, capped.
fn backoff(attempt: u32) -> Duration {
    let doubling = 1u32 << attempt.saturating_sub(1).min(8);
    let base = BACKOFF_INITIAL.saturating_mul(doubling).min(BACKOFF_MAX);
    let jitter = 1.0 - BACKOFF_JITTER / 2.0 + BACKOFF_JITTER * fastrand::f64();
    base.mul_f64(jitter)
}

/// Why a session ended (one socket's lifetime).
enum SessionEnd {
    /// The shutdown token fired; the loop exits cleanly.
    Shutdown,
    /// The socket died; the loop reconnects.
    Dropped(String),
}

/// One socket: reads decode into the event plane; `tools/call` runs
/// locally; every write goes through the writer task (the single
/// owner of the sink half).
async fn session(
    config: &NodeConfig,
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    shutdown: &CancellationToken,
    commands_rx: &mut mpsc::UnboundedReceiver<Command>,
    events: &mpsc::UnboundedSender<ClientEvent>,
) -> SessionEnd {
    let (mut sink, mut stream) = ws.split();
    let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel::<Outbound>();
    let writer = tokio::spawn(async move {
        while let Some(outbound) = outbound_rx.recv().await {
            let result = match outbound {
                Outbound::Frame(frame) => sink.send(Message::text(frame)).await,
                // The close frame goes out, then the writer keeps the
                // sink until the read side observes the peer's echo —
                // `stop()`-style abort would kill the handshake
                // mid-flight (the Gleam PoC's graceful-exit lesson).
                Outbound::Close => {
                    let _ = sink.close().await;
                    break;
                }
            };
            if result.is_err() {
                break;
            }
        }
    });

    // In-flight `tools/call` tokens, keyed by request id: a
    // `notifications/cancelled` re-signals one; a session drop cancels
    // them all (the server answers its side through the adapter
    // monitor).
    let mut calls: HashMap<i64, CancellationToken> = HashMap::new();
    let workspace = env::Workspace::new(config.workspace.clone(), config.thread.clone());
    let mut pings = tokio::time::interval(PING_INTERVAL);
    pings.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Align the first tick: intervals fire immediately at creation.
    pings.reset();
    // A graceful close was sent; stop serving after this linger. Both
    // flags ARM the select: a cancelled token (or a closed command
    // channel) completes instantly, and an unguarded arm would spin
    // the loop hot, starving the runtime.
    let mut close_deadline: Option<Instant> = None;
    let mut closing = false;
    let mut caller_done = false;

    loop {
        tokio::select! {
            () = shutdown.cancelled(), if !closing => {
                closing = true;
                let _ = outbound_tx.send(Outbound::Close);
                close_deadline.get_or_insert(Instant::now() + CLOSE_LINGER);
                // Fall through: keep reading until the echo or the linger.
            }
            _ = pings.tick() => {
                let _ = outbound_tx.send(Outbound::Frame(protocol::encode_ping()));
            }
            command = commands_rx.recv(), if !caller_done => match command {
                Some(Command::Send(text)) => {
                    let _ = outbound_tx.send(Outbound::Frame(protocol::encode_send(&text)));
                }
                Some(Command::Ping) => {
                    let _ = outbound_tx.send(Outbound::Frame(protocol::encode_ping()));
                }
                None => {
                    // The caller dropped its handle: as good as shutdown.
                    caller_done = true;
                    let _ = outbound_tx.send(Outbound::Close);
                    close_deadline.get_or_insert(Instant::now() + CLOSE_LINGER);
                }
            },
            frame = stream.next() => match frame {
                Some(Ok(Message::Text(text))) => {
                    match protocol::decode_server_frame(&text) {
                        Ok(frame) => on_frame(config, frame, &mut calls, &workspace, &outbound_tx, events),
                        // Malformed wire: skip, never fatal — the
                        // server's own policy for OUR garbage is an
                        // error frame, and it keeps the socket.
                        Err(error) => {
                            tracing::warn!(error = %error, bytes = text.len(), "undecodable frame skipped")
                        }
                    }
                }
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => {}
                Some(Err(_)) => break, // reported below as a drop
            },
            _ = linger_expired(close_deadline) => break,
        }
    }

    // The socket is done: cancel every in-flight call, then drop the
    // outbound channel so the writer finishes (its last frame already
    // went out on the graceful path).
    for token in calls.values() {
        token.cancel();
    }
    drop(outbound_tx);
    let _ = writer.await;
    if shutdown.is_cancelled() || caller_done {
        SessionEnd::Shutdown
    } else {
        SessionEnd::Dropped("socket closed by peer".to_owned())
    }
}

/// The select arm for the close linger: pending until the deadline
/// exists AND passes.
async fn linger_expired(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => {
            tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
        }
        None => std::future::pending().await,
    }
}

/// Dispatch one decoded server frame: the tool plane is served here;
/// everything else forwards to the caller.
fn on_frame(
    config: &NodeConfig,
    frame: protocol::ServerFrame,
    calls: &mut HashMap<i64, CancellationToken>,
    workspace: &env::Workspace,
    outbound_tx: &mpsc::UnboundedSender<Outbound>,
    events: &mpsc::UnboundedSender<ClientEvent>,
) {
    match frame {
        protocol::ServerFrame::CallRequest {
            id,
            tool,
            arguments,
        } => {
            dispatch_call(config, id, tool, arguments, calls, workspace, outbound_tx);
        }
        protocol::ServerFrame::Cancelled { id } => {
            // Best-effort by contract: an already-answered or unknown
            // id finds no token and the notice drops.
            if let Some(token) = calls.get(&id) {
                token.cancel();
            }
        }
        protocol::ServerFrame::ProtocolError { message } => {
            tracing::warn!(message = %message, "server refused a frame");
            let _ = events.send(ClientEvent::Frame(protocol::ServerFrame::ProtocolError {
                message,
            }));
        }
        other => {
            let _ = events.send(ClientEvent::Frame(other));
        }
    }
}

/// Run one `tools/call` in a task and answer `call_response`. The
/// cancellation token registers BEFORE the task spawns, so a
/// `notifications/cancelled` landing during the arm-up cannot miss.
fn dispatch_call(
    config: &NodeConfig,
    id: i64,
    tool: String,
    arguments: Value,
    calls: &mut HashMap<i64, CancellationToken>,
    workspace: &env::Workspace,
    outbound_tx: &mpsc::UnboundedSender<Outbound>,
) {
    let token = CancellationToken::new();
    calls.insert(id, token.clone());
    let workspace = workspace.clone();
    let outbound_tx = outbound_tx.clone();
    let timeout_ms = config.timeout_ms;
    let max_output_bytes = config.max_output_bytes;
    tokio::spawn(async move {
        let outcome = env::serve_call(
            &workspace,
            &tool,
            &arguments,
            timeout_ms,
            max_output_bytes,
            &token,
        )
        .await;
        let (is_error, text) = match outcome {
            Ok(text) => (false, text),
            Err(message) => (true, message),
        };
        let _ = outbound_tx.send(Outbound::Frame(protocol::encode_call_response(
            id, is_error, &text,
        )));
    });
}

/// Why the upgrade failed.
enum ConnectError {
    /// 401 pre-upgrade: credential rejected (the node row is gone).
    Unauthorized { server: String },
    /// Everything else.
    Failed { server: String, detail: String },
}

/// Dial outbound to `/node/ws` with the node's credential. Auth rides
/// the query string (the name is charset-validated, tokens are
/// URL-safe base64); the upgrade answers plain HTTP statuses on
/// refusal — 401 splits from the rest for the exit-code contract.
async fn connect(
    config: &NodeConfig,
) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>, ConnectError> {
    let url = ws_url(config).map_err(|detail| ConnectError::Failed {
        server: config.server.clone(),
        detail,
    })?;
    match connect_async(url.as_str()).await {
        Ok((ws, _response)) => Ok(ws),
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
            let status = response.status();
            if status.as_u16() == 401 {
                Err(ConnectError::Unauthorized {
                    server: config.server.clone(),
                })
            } else {
                Err(ConnectError::Failed {
                    server: config.server.clone(),
                    detail: format!("handshake failed with status {status}"),
                })
            }
        }
        Err(tokio_tungstenite::tungstenite::Error::Protocol(ProtocolError::SendAfterClosing)) => {
            Err(ConnectError::Failed {
                server: config.server.clone(),
                detail: "socket closed during handshake".to_owned(),
            })
        }
        Err(error) => Err(ConnectError::Failed {
            server: config.server.clone(),
            detail: error.to_string(),
        }),
    }
}

fn ws_url(config: &NodeConfig) -> Result<Url, String> {
    let mut url = Url::parse(&config.server).map_err(|e| format!("invalid server url: {e}"))?;
    // The upgrade request must carry the ws(s) scheme; the stored base
    // is the server's http(s) origin.
    url.set_scheme(match url.scheme() {
        "https" => "wss",
        _ => "ws",
    })
    .map_err(|()| format!("cannot rewrite scheme for {}", config.server))?;
    url.set_path("/node/ws");
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("node", &config.node);
        query.append_pair("token", &config.token);
        query.append_pair("thread", &config.thread);
        query.append_pair("role", config.role.as_str());
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_async;

    const LOOPBACK: &str = "127.0.0.1";
    /// Generous bounds for CI boxes; each bound only guards a
    /// would-hang test, never times a real work item.
    const AWAIT_TIMEOUT: Duration = Duration::from_secs(5);

    fn config(port: u16) -> NodeConfig {
        NodeConfig {
            server: format!("http://{LOOPBACK}:{port}"),
            node: "proxmox-2".to_owned(),
            token: "tok-1".to_owned(),
            thread: "th-1".to_owned(),
            role: Role::Environment,
            workspace: std::env::temp_dir().join("nexus-client-test-workspace"),
            timeout_ms: 10_000,
            max_output_bytes: 4096,
        }
    }

    /// Accept one client and return its socket + the request path
    /// (pinning the auth query the route expects).
    async fn accept(
        listener: &TcpListener,
    ) -> (WebSocketStream<MaybeTlsStream<TcpStream>>, String) {
        let (socket, _) = listener.accept().await.unwrap();
        let mut buffer = [0_u8; 4096];
        // Peek (non-destructive) until the full header block is visible
        // — reading would consume bytes accept_async still needs.
        let mut seen = 0;
        let path = loop {
            let read = socket
                .peek(&mut buffer[seen..])
                .await
                .expect("handshake readable");
            assert!(read > 0, "client hung up before the handshake");
            seen += read;
            let visible = String::from_utf8_lossy(&buffer[..seen]);
            if visible.contains("\r\n\r\n") {
                let request_line = visible.lines().next().unwrap();
                break request_line.split_whitespace().nth(1).unwrap().to_owned();
            }
        };
        let ws = accept_async(MaybeTlsStream::Plain(socket)).await.unwrap();
        (ws, path)
    }

    async fn next_event(events: &mut mpsc::UnboundedReceiver<ClientEvent>) -> ClientEvent {
        tokio::time::timeout(AWAIT_TIMEOUT, events.recv())
            .await
            .expect("event arrives in time")
            .expect("event channel stays open")
    }

    async fn next_frame(ws: &mut WebSocketStream<MaybeTlsStream<TcpStream>>) -> String {
        loop {
            let message = tokio::time::timeout(AWAIT_TIMEOUT, ws.next())
                .await
                .expect("frame arrives in time")
                .expect("socket stays open")
                .expect("frame decodes");
            if let Message::Text(text) = message {
                return text.to_string();
            }
        }
    }

    #[tokio::test]
    async fn connect_pins_the_route_query() {
        let listener = TcpListener::bind((LOOPBACK, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (client, mut events) = spawn(config(port));
        let (mut ws, path) = accept(&listener).await;

        assert!(
            path.starts_with("/node/ws?node=proxmox-2&token=tok-1&thread=th-1&role=env"),
            "unexpected request line: {path}"
        );
        assert_eq!(next_event(&mut events).await, ClientEvent::Connected);

        // hello + replay flow through to the caller untouched.
        tokio::time::timeout(
            AWAIT_TIMEOUT,
            ws.send(Message::text(
                "{\"v\":1,\"type\":\"hello\",\"model\":\"smart\",\"messages\":2}",
            )),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            next_event(&mut events).await,
            ClientEvent::Frame(protocol::ServerFrame::Hello {
                model: "smart".to_owned(),
                messages: 2,
            })
        );

        client.shutdown();
    }

    #[tokio::test]
    async fn send_and_ping_reach_the_wire() {
        let listener = TcpListener::bind((LOOPBACK, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (client, mut events) = spawn(config(port));
        let (mut ws, _) = accept(&listener).await;
        let _ = next_event(&mut events).await; // Connected

        client.send("case:bash now");
        client.ping();
        assert_eq!(
            next_frame(&mut ws).await,
            "{\"v\":1,\"type\":\"send\",\"text\":\"case:bash now\"}"
        );
        assert_eq!(next_frame(&mut ws).await, "{\"v\":1,\"type\":\"ping\"}");

        client.shutdown();
    }

    #[tokio::test]
    async fn tool_call_round_trip_executes_locally() {
        let listener = TcpListener::bind((LOOPBACK, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let workspace = tempfile::tempdir().unwrap();
        let mut cfg = config(port);
        cfg.workspace = workspace.path().to_owned();
        let (_client, mut events) = spawn(cfg);
        let (mut ws, _) = accept(&listener).await;
        let _ = next_event(&mut events).await; // Connected

        let request = serde_json::json!({"target": "proxmox-2", "command": "echo node-ran"});
        let frame = serde_json::json!({
            "v": 1,
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {"name": "bash", "arguments": request},
        })
        .to_string();
        ws.send(Message::text(frame)).await.unwrap();

        let response = next_frame(&mut ws).await;
        assert_eq!(
            response,
            "{\"v\":1,\"jsonrpc\":\"2.0\",\"id\":4,\"result\":{".to_owned()
                + "\"content\":[{\"type\":\"text\",\"text\":\"node-ran\"}],"
                + "\"isError\":false}}"
        );

        _client.shutdown();
    }

    #[tokio::test]
    async fn failed_call_answers_is_error_true() {
        let listener = TcpListener::bind((LOOPBACK, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (_client, mut events) = spawn(config(port));
        let (mut ws, _) = accept(&listener).await;
        let _ = next_event(&mut events).await; // Connected

        let frame = serde_json::json!({
            "v": 1,
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/call",
            "params": {"name": "web_search", "arguments": {}},
        })
        .to_string();
        ws.send(Message::text(frame)).await.unwrap();

        let response = next_frame(&mut ws).await;
        assert!(response.contains("\"isError\":true"));
        assert!(response.contains("this node does not serve the tool"));

        _client.shutdown();
    }

    #[tokio::test]
    async fn cancelled_notice_stops_a_long_call() {
        let listener = TcpListener::bind((LOOPBACK, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let workspace = tempfile::tempdir().unwrap();
        let mut cfg = config(port);
        cfg.workspace = workspace.path().to_owned();
        let (_client, mut events) = spawn(cfg);
        let (mut ws, _) = accept(&listener).await;
        let _ = next_event(&mut events).await; // Connected

        let call = serde_json::json!({
            "v": 1,
            "jsonrpc": "2.0",
            "id": 9,
            "method": "tools/call",
            "params": {"name": "bash", "arguments": {"target": "node", "command": "sleep 30"}},
        })
        .to_string();
        ws.send(Message::text(call)).await.unwrap();
        let cancel = serde_json::json!({
            "v": 1,
            "jsonrpc": "2.0",
            "method": "notifications/cancelled",
            "params": {"requestId": 9},
        })
        .to_string();
        ws.send(Message::text(cancel)).await.unwrap();

        let response = next_frame(&mut ws).await;
        assert!(response.contains("\"isError\":true"));
        assert!(response.contains("cancelled. partial output:"));

        _client.shutdown();
    }

    #[tokio::test]
    async fn shutdown_closes_gracefully_with_zero() {
        let listener = TcpListener::bind((LOOPBACK, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (client, mut events) = spawn(config(port));
        let (mut ws, _) = accept(&listener).await;
        let _ = next_event(&mut events).await; // Connected

        // A reader drives the peer side: tungstenite echoes the close
        // frame only while the peer is reading.
        let reader = tokio::spawn(async move {
            loop {
                match tokio::time::timeout(AWAIT_TIMEOUT, ws.next()).await {
                    Ok(Some(Ok(Message::Close(_)))) | Err(_) => break true,
                    Ok(Some(Ok(_))) => continue,
                    Ok(Some(Err(_))) | Ok(None) => break false,
                }
            }
        });

        client.shutdown();
        assert_eq!(
            next_event(&mut events).await,
            ClientEvent::Terminated(Termination::Shutdown)
        );
        // The peer observed a real close handshake, not an abort.
        let saw_close = tokio::time::timeout(AWAIT_TIMEOUT, reader)
            .await
            .expect("reader finishes")
            .expect("reader task joins");
        assert!(saw_close);
    }

    #[tokio::test]
    async fn dropped_first_connect_ends_with_exit_two() {
        // Nothing listens here: the FIRST connect fails terminally.
        let port = free_port().await;
        let (_client, mut events) = spawn(config(port));
        match next_event(&mut events).await {
            ClientEvent::Terminated(Termination::ConnectFailed(_)) => {}
            other => panic!("expected ConnectFailed, got {other:?}"),
        }
    }

    /// A raw 401 pre-upgrade response is the credential-rejected
    /// signal: exit 4, the launcher's re-pair trigger.
    #[tokio::test]
    async fn unauthorized_handshake_ends_with_exit_four() {
        let listener = TcpListener::bind((LOOPBACK, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            refuse_with_401(socket).await;
        });
        let (_client, mut events) = spawn(config(port));
        assert_eq!(
            next_event(&mut events).await,
            ClientEvent::Terminated(Termination::CredentialRejected)
        );
    }

    #[tokio::test]
    async fn dropped_session_reconnects_with_backoff() {
        let listener = TcpListener::bind((LOOPBACK, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (client, mut events) = spawn(config(port));

        let (ws, _) = accept(&listener).await;
        assert_eq!(next_event(&mut events).await, ClientEvent::Connected);

        // The server vanishes mid-session AND its listener closes: the
        // next attempt fails (Reconnecting), then a fresh listener
        // lets the following attempt land (Connected again).
        drop(ws);
        drop(listener);
        assert!(matches!(
            next_event(&mut events).await,
            ClientEvent::Reconnecting { .. }
        ));
        let listener = TcpListener::bind((LOOPBACK, port)).await.unwrap();
        let _ws2 = accept(&listener).await;
        assert_eq!(next_event(&mut events).await, ClientEvent::Connected);

        client.shutdown();
    }

    #[test]
    fn termination_maps_the_exit_code_contract() {
        assert_eq!(Termination::CredentialRejected.exit_code(), 4);
        assert_eq!(
            Termination::ConnectFailed("refused".to_owned()).exit_code(),
            2
        );
        assert_eq!(Termination::Shutdown.exit_code(), 0);
    }

    #[test]
    fn role_parses_like_the_server_rule() {
        assert_eq!(Role::parse("viewer"), Role::Viewer);
        assert_eq!(Role::parse("env"), Role::Environment);
        assert_eq!(Role::parse(""), Role::Environment);
        assert_eq!(Role::Viewer.as_str(), "viewer");
        assert_eq!(Role::Environment.as_str(), "env");
    }

    #[test]
    fn backoff_doubles_caps_and_stays_positive() {
        assert!(backoff(1) <= BACKOFF_MAX);
        assert!(backoff(1) <= backoff(2) + BACKOFF_INITIAL);
        // Past the cap the value stays bounded.
        assert!(
            backoff(50)
                <= BACKOFF_MAX
                    + Duration::from_millis(
                        (BACKOFF_MAX.as_millis() as f64 * BACKOFF_JITTER) as u64
                    )
        );
    }

    async fn refuse_with_401(socket: TcpStream) {
        use tokio::io::AsyncReadExt;
        use tokio::io::AsyncWriteExt;
        let mut socket = socket;
        // Read the upgrade request (headers only), then answer plain.
        let mut buffer = vec![0_u8; 4096];
        let mut request = Vec::new();
        loop {
            let read = socket.read(&mut buffer).await.unwrap_or(0);
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..read]);
            if String::from_utf8_lossy(&request).contains("\r\n\r\n") {
                break;
            }
        }
        let response: Vec<u8> = [
            &b"HTTP/1.1 401 Unauthorized\r\ncontent-type: application/json\r\n"[..],
            &b"content-length: 37\r\nconnection: close\r\n\r\n"[..],
            &b"{\"error\":\"unknown node or bad token\"}"[..],
        ]
        .concat();
        let _ = socket.write_all(&response).await;
        let _ = socket.shutdown().await;
    }

    async fn free_port() -> u16 {
        let listener = TcpListener::bind((LOOPBACK, 0)).await.unwrap();
        listener.local_addr().unwrap().port()
    }
}
