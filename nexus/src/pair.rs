//! Pairing (tui-pairing plan): a web-minted pairing URL —
//! `http://boss.home.arpa:8080/pair/<code>` — exchanged at the PUBLIC
//! `POST /nodes/pair` route for a durable node credential. The code is
//! its own auth; the minted token persists in the state file
//! ([`crate::state`]).
//!
//! Route contract: `{"code": …, "name": …, "thread": …}` in, `201
//! {id, name, token, thread_id, created_at}` out; expired, revoked,
//! and unknown codes answer one merged 404 (no code oracle); a live
//! code with a taken name answers 409. `thread` is optional — the
//! `NEXUS_THREAD` override, binding the minted row to the thread the
//! TUI will actually join; absent, the grant's pin rides.

use std::time::Duration;

use serde::Deserialize;
use serde::Serialize;

/// The two halves of a pairing URL: the server base (origin only) and
/// the code (the last `/pair/` segment).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairUrl {
    pub base: String,
    pub code: String,
}

/// One exchange outcome: a minted credential plus the thread the row
/// is bound to (the grant's pin, unless overridden).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Paired {
    pub id: String,
    pub name: String,
    pub token: String,
    #[serde(rename = "thread_id")]
    pub thread_id: String,
    pub created_at: i64,
}

/// How long the exchange may take; a homelab box answers in seconds.
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(15);

/// Split a pairing URL into base + code. The code is the LAST
/// `/pair/` segment up to the next `/`, `?`, or `#`; the base is the
/// URL's origin (scheme + authority) — never a path.
pub fn parse_pair_url(url: &str) -> Result<PairUrl, PairError> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(invalid(format!(
            "pairing url must start with http:// or https://: {url}"
        )));
    }
    let (before_pair, after_pair) = match url.rsplit_once("/pair/") {
        Some(split) => split,
        None => {
            return Err(invalid(format!(
                "pairing url must carry a /pair/<code> path: {url}"
            )));
        }
    };
    let code: String = after_pair
        .chars()
        .take_while(|c| !matches!(c, '/' | '?' | '#'))
        .collect();
    if code.is_empty() {
        return Err(invalid(format!(
            "pairing url must carry a /pair/<code> path: {url}"
        )));
    }
    let scheme = if before_pair.starts_with("https://") {
        "https"
    } else {
        "http"
    };
    let authority: String = before_pair[format!("{scheme}://").len()..]
        .chars()
        .take_while(|c| *c != '/')
        .collect();
    if authority.is_empty() {
        return Err(invalid(format!(
            "pairing url must include the server origin: {url}"
        )));
    }
    Ok(PairUrl {
        base: format!("{scheme}://{authority}"),
        code,
    })
}

fn invalid(message: String) -> PairError {
    PairError::Invalid(message)
}

/// Validate a proposed node name before the exchange (the server's
/// `env.valid_target_name` rule, same wording): non-empty, `workspace`
/// reserved, `^[a-zA-Z0-9_-]+$`.
pub fn validate_node_name(name: &str) -> Result<(), String> {
    match name {
        "" => Err("target name must not be empty".to_owned()),
        "workspace" => Err("target name 'workspace' is reserved".to_owned()),
        _ => {
            if name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
            {
                Ok(())
            } else {
                Err(format!(
                    "target name must match ^[a-zA-Z0-9_-]+$ (got \"{name}\")"
                ))
            }
        }
    }
}

/// The exchange request body; `thread` rides only when overridden.
#[derive(Serialize)]
struct PairBody<'a> {
    code: &'a str,
    name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    thread: Option<&'a str>,
}

/// Redeem a pairing code at `POST /nodes/pair`. Multi-use by design:
/// every exchange mints a NEW node row (several hosts from one URL).
/// A code must never authorize taking over an existing name — that is
/// the server's 409.
pub async fn exchange(
    base: &str,
    code: &str,
    name: &str,
    thread_override: Option<&str>,
) -> Result<Paired, PairError> {
    let route = format!("{base}/nodes/pair");
    let client = reqwest::Client::builder()
        .timeout(EXCHANGE_TIMEOUT)
        .build()
        .map_err(|e| PairError::Unreachable(e.to_string()))?;
    let response = client
        .post(&route)
        .json(&PairBody {
            code,
            name,
            thread: thread_override,
        })
        .send()
        .await
        .map_err(|e| PairError::Unreachable(format!("cannot reach {base}: {e}")))?;
    let status = response.status();
    match status.as_u16() {
        201 => response
            .json::<Paired>()
            .await
            .map_err(|e| PairError::Status {
                status: status.as_u16(),
                body: e.to_string(),
            }),
        404 => Err(PairError::CodeRejected),
        409 => Err(PairError::NameTaken),
        _ => {
            let body = response.text().await.unwrap_or_default();
            Err(PairError::Status {
                status: status.as_u16(),
                body,
            })
        }
    }
}

/// Why the exchange failed; each wording is the friendly path the
/// launcher surfaces.
#[derive(Debug, thiserror::Error)]
pub enum PairError {
    /// The merged 404: unknown, expired, or revoked — mint a fresh
    /// pairing URL.
    #[error(
        "pairing code rejected (unknown, expired, or revoked) — mint a fresh pairing URL in the web UI"
    )]
    CodeRejected,
    /// 409 with a live code: the name is taken on this thread.
    #[error(
        "node name is already paired on this thread — revoke it in the thread info pane (Environments) or via DELETE /nodes, then retry"
    )]
    NameTaken,
    #[error("{0}")]
    Unreachable(String),
    #[error("pairing failed (HTTP {status}): {body}")]
    Status { status: u16, body: String },
    #[error("{0}")]
    Invalid(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test]
    fn parse_pins_the_launcher_shape() {
        let url = parse_pair_url("http://boss.home.arpa:8080/pair/AbC_123").unwrap();
        assert_eq!(
            url,
            PairUrl {
                base: "http://boss.home.arpa:8080".to_owned(),
                code: "AbC_123".to_owned(),
            }
        );
        // Query strings and fragments do not leak into the code.
        assert_eq!(
            parse_pair_url("https://boss:8080/pair/c0de?x=1#frag")
                .unwrap()
                .code,
            "c0de"
        );
        // A path prefix before /pair/ never leaks into the base.
        assert_eq!(
            parse_pair_url("https://boss:8080/app/pair/c0de")
                .unwrap()
                .base,
            "https://boss:8080"
        );
    }

    #[test_case("ftp://boss/pair/c0de"; "wrong scheme")]
    #[test_case("boss/pair/c0de"; "no scheme")]
    #[test_case("http://boss:8080/other/c0de"; "no /pair/ path")]
    #[test_case("http://boss:8080/pair/"; "empty code")]
    #[test_case("http:///pair/c0de"; "no authority")]
    fn parse_refuses_malformed_urls(url: &str) {
        assert!(parse_pair_url(url).is_err());
    }

    /// The last /pair/ wins (the launcher's greedy regex): a code that
    /// itself contains the word pair stays a code.
    #[test]
    fn parse_takes_the_last_pair_segment() {
        let url = parse_pair_url("http://boss:8080/pair/pairwise-code").unwrap();
        assert_eq!(url.code, "pairwise-code");
    }

    #[test]
    fn node_name_validation_matches_the_server_rule() {
        assert_eq!(validate_node_name("proxmox-2"), Ok(()));
        assert_eq!(
            validate_node_name(""),
            Err("target name must not be empty".to_owned())
        );
        assert_eq!(
            validate_node_name("workspace"),
            Err("target name 'workspace' is reserved".to_owned())
        );
        assert_eq!(
            validate_node_name("proxmox 2"),
            Err("target name must match ^[a-zA-Z0-9_-]+$ (got \"proxmox 2\")".to_owned())
        );
    }

    /// Drive the exchange against a hand-rolled loopback responder:
    /// pins the request body (the optional thread override omitted
    /// when absent) and each status's mapping. Offline beyond loopback.
    #[tokio::test]
    async fn exchange_pins_request_body_and_201_parse() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let body = serde_json::json!({
                "id": "n01",
                "name": "proxmox-2",
                "token": "tok-1",
                "thread_id": "th-9",
                "created_at": 1_737_000_000,
            })
            .to_string();
            let (socket, request) = read_http_request(listener).await;
            respond(socket, 201, &body).await;
            request
        });

        let paired = exchange(
            &format!("http://127.0.0.1:{port}"),
            "the-code",
            "proxmox-2",
            Some("th-override"),
        )
        .await
        .unwrap();

        assert_eq!(
            paired,
            Paired {
                id: "n01".to_owned(),
                name: "proxmox-2".to_owned(),
                token: "tok-1".to_owned(),
                thread_id: "th-9".to_owned(),
                created_at: 1_737_000_000,
            }
        );
        let request = server.await.unwrap();
        assert!(request.starts_with("POST /nodes/pair "));
        assert!(
            request.contains(
                "{\"code\":\"the-code\",\"name\":\"proxmox-2\",\"thread\":\"th-override\"}"
            )
        );
    }

    #[tokio::test]
    async fn exchange_omits_the_thread_field_without_override() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (socket, request) = read_http_request(listener).await;
            respond(socket, 409, "{\"error\":\"node name is already paired\"}").await;
            request
        });

        let result = exchange(
            &format!("http://127.0.0.1:{port}"),
            "the-code",
            "proxmox-2",
            None,
        )
        .await;

        let request = server.await.unwrap();
        assert!(request.contains("{\"code\":\"the-code\",\"name\":\"proxmox-2\"}"));
        assert!(matches!(result, Err(PairError::NameTaken)));
    }

    #[tokio::test]
    async fn exchange_maps_404_to_the_merged_code_rejection() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (socket, _) = read_http_request(listener).await;
            respond(
                socket,
                404,
                "{\"error\":\"unknown or expired pairing code\"}",
            )
            .await;
        });

        let result = exchange(
            &format!("http://127.0.0.1:{port}"),
            "the-code",
            "proxmox-2",
            None,
        )
        .await;
        assert!(matches!(result, Err(PairError::CodeRejected)));
    }

    #[tokio::test]
    async fn exchange_maps_transport_failure_to_unreachable() {
        // Port 1 on loopback refuses: the honest transport error.
        let result = exchange("http://127.0.0.1:1", "c", "n", None).await;
        assert!(matches!(result, Err(PairError::Unreachable(_))));
    }

    /// Read one request off the listener (connection per exchange —
    /// each call builds its own reqwest client), returning the socket
    /// for the response.
    async fn read_http_request(
        listener: tokio::net::TcpListener,
    ) -> (tokio::net::TcpStream, String) {
        use tokio::io::AsyncReadExt;
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buffer = vec![0_u8; 4096];
        let mut request = Vec::new();
        loop {
            let read = socket.read(&mut buffer).await.unwrap_or(0);
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..read]);
            let text = String::from_utf8_lossy(&request);
            if let Some(head_end) = text.find("\r\n\r\n") {
                let length = text[..head_end]
                    .lines()
                    .find_map(|line| {
                        line.strip_prefix("content-length:")
                            .map(|n| n.trim().parse::<usize>().unwrap_or(0))
                    })
                    .unwrap_or(0);
                if request.len() >= head_end + 4 + length {
                    break;
                }
            }
        }
        (socket, String::from_utf8_lossy(&request).into_owned())
    }

    async fn respond(mut socket: tokio::net::TcpStream, status: u16, body: &str) {
        use tokio::io::AsyncWriteExt;
        let response = format!(
            "HTTP/1.1 {status} TEST\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = socket.write_all(response.as_bytes()).await;
        let _ = socket.shutdown().await;
    }
}
