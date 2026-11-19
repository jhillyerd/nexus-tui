//! The `/node/ws` client protocol, version 1: the pure frame codec.
//!
//! Two planes share one WebSocket (see `nexus/docs/plans/environments.md`):
//!
//! - **Event plane** (server→client): a JSON projection of thread events —
//!   transcript lines, streaming deltas, tool chips, turn lifecycle,
//!   metrics — plus the connection frames (`attached`, `pong`, `error`).
//! - **Tool plane** (bidirectional, MCP-shaped): `tools/call` requests
//!   flow server→client with `params.arguments` carried as a RAW JSON
//!   object, `notifications/cancelled` flows server→client, and the
//!   client answers `{"v":1,"jsonrpc":"2.0","id":N,"result":{…}}` with
//!   the flattened text plus the wire's `isError` flag.
//!
//! Client→server user input rides `send` frames through the same `Send`
//! path the web UI uses; `ping` is answered `pong`.
//!
//! Versioning is `"v":1`, additive-only: an unknown event KIND — or a
//! wrong version — decodes as [`ServerFrame::Unknown`] (forward
//! compatible, the renderer skips it); malformed JSON and a malformed
//! KNOWN shape are errors the connection policy may treat as fatal.
//!
//! Every wire shape here is pinned byte-exact against the server's
//! `test/node_protocol_test.gleam` (the additive-only contract means a
//! byte change there is a protocol change, never a refactor).

use serde::Deserialize;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

/// The protocol version. Every frame carries `"v":1`; other versions
/// decode as [`ServerFrame::Unknown`] (a newer peer talking to an older
/// client is the additive contract's forward direction).
const VERSION: i64 = 1;

/// JSON-RPC marker carried by the tool plane's frames.
const JSONRPC: &str = "2.0";

// ---------------------------------------------------------------------------
// Client → server: the encoding half
// ---------------------------------------------------------------------------

/// `{"v":1,"type":"send","text":…}` — a human turn from the node's TUI.
/// Hostile text escapes through serde, so a send cannot shape the frame.
pub fn encode_send(text: &str) -> String {
    serde_json::to_string(&SendFrame {
        v: VERSION,
        r#type: "send",
        text,
    })
    .expect("SendFrame serializes infallibly")
}

#[derive(Serialize)]
struct SendFrame<'a> {
    v: i64,
    r#type: &'static str,
    text: &'a str,
}

/// `{"v":1,"type":"ping"}` — liveness, answered `pong`.
pub fn encode_ping() -> String {
    serde_json::to_string(&PingFrame {
        v: VERSION,
        r#type: "ping",
    })
    .expect("PingFrame serializes infallibly")
}

#[derive(Serialize)]
struct PingFrame {
    v: i64,
    r#type: &'static str,
}

/// The `tools/call` response half: `{"v":1,"jsonrpc":"2.0","id":N,
/// "result":{"content":[{"type":"text","text":…}],"isError":b}}`. The
/// `is_error` argument is the WIRE's own flag (a failed call answers
/// `true`); the text escapes through serde so node bytes cannot shape
/// the frame.
pub fn encode_call_response(id: i64, is_error: bool, text: &str) -> String {
    serde_json::to_string(&CallResponseFrame {
        v: VERSION,
        jsonrpc: JSONRPC,
        id,
        result: CallResponseResult {
            content: [TextPart {
                r#type: "text",
                text,
            }],
            is_error,
        },
    })
    .expect("CallResponseFrame serializes infallibly")
}

#[derive(Serialize)]
struct CallResponseFrame<'a> {
    v: i64,
    jsonrpc: &'static str,
    id: i64,
    result: CallResponseResult<'a>,
}

#[derive(Serialize)]
struct CallResponseResult<'a> {
    content: [TextPart<'a>; 1],
    #[serde(rename = "isError")]
    is_error: bool,
}

#[derive(Serialize)]
struct TextPart<'a> {
    r#type: &'static str,
    text: &'a str,
}

// ---------------------------------------------------------------------------
// Server → client: the decoding half
// ---------------------------------------------------------------------------

/// One decoded server→client frame: the event plane's projection, the
/// connection-level frames, and the tool plane's request half. Unknown
/// event KINDs (and wrong versions) decode as `Unknown` — the renderer
/// skips them, never fatally.
#[derive(Debug, Clone, PartialEq)]
pub enum ServerFrame {
    /// `hello` — first frame after the upgrade: the thread's model and
    /// message count, before the replay.
    Hello { model: String, messages: i64 },
    /// `user` — a committed user row (replay and echoes). `author`
    /// carries the display-only sidecar (`system` for roster lines).
    User {
        text: String,
        at: Option<i64>,
        author: Option<String>,
    },
    /// `assistant` — a committed assistant message.
    Assistant { text: String, at: Option<i64> },
    /// `reasoning_full` — a committed reasoning block.
    ReasoningFull { text: String },
    /// `token` — a streaming assistant delta.
    Token { text: String },
    /// `reasoning` — a streaming reasoning delta.
    Reasoning { text: String },
    /// `turn_done` — the turn committed; per-turn stream metrics ride
    /// `stats`.
    TurnDone { at: Option<i64>, stats: TurnStats },
    /// `turn_stopped` — the turn was stopped after `chars` emitted.
    TurnStopped { chars: i64, at: Option<i64> },
    /// `turn_failed` — the turn failed; the reason is display text.
    TurnFailed { reason: String, at: Option<i64> },
    /// `tool_call_delta` — a streaming tool-call argument fragment.
    ToolCallDelta {
        index: i64,
        id: Option<String>,
        name: Option<String>,
        fragment: String,
    },
    /// `tool_call` — an assembled tool call (transcript chip). The
    /// arguments ride JSON-encoded as a STRING here.
    ToolCall {
        id: String,
        name: String,
        arguments: String,
        at: Option<i64>,
    },
    /// `tool_result` — a committed tool result.
    ToolResult {
        call_id: String,
        name: String,
        ok: bool,
        content: String,
    },
    /// `loop_status` — agent-loop iteration/phase for the status bar.
    LoopStatus {
        iteration: i64,
        max: i64,
        phase: String,
    },
    /// `meta` — thread title/tags.
    Meta {
        title: Option<String>,
        tags: Vec<String>,
    },
    /// `attached` — this node's roster target is live and routable
    /// (env role only, once the server's adapter accepts the join).
    Attached { target: String },
    /// `pong` — the answer to a `ping`.
    Pong,
    /// `error` — a protocol-level refusal (refused send, unknown
    /// frame). Informational; the connection policy decides closure.
    ProtocolError { message: String },
    /// `tools/call` — execute this tool locally and answer with
    /// [`encode_call_response`]. The arguments are the RAW JSON object
    /// the server embedded (a proven object per the protocol's
    /// `valid_arguments` gate).
    CallRequest {
        id: i64,
        tool: String,
        arguments: Value,
    },
    /// `notifications/cancelled` — best-effort notice that the call
    /// `id` was cancelled; stop the local execution if in flight.
    Cancelled { id: i64 },
    /// An unrecognized KIND, or a frame whose version is not 1. Skipped,
    /// never fatal.
    Unknown,
}

/// The per-turn stream metrics a `turn_done` frame carries — the same
/// numbers the web UI's metrics panel shows. `ttft_ms` is optional at
/// the wire (absent/null until a first token exists).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnStats {
    #[serde(rename = "ttft_ms", default)]
    pub ttft_ms: Option<i64>,
    #[serde(rename = "duration_ms")]
    pub duration_ms: i64,
    #[serde(rename = "token_deltas")]
    pub token_deltas: i64,
    #[serde(rename = "reasoning_deltas")]
    pub reasoning_deltas: i64,
    #[serde(rename = "tool_call_deltas")]
    pub tool_call_deltas: i64,
}

/// A decode failure: malformed JSON, or a malformed KNOWN shape. The
/// additive-only contract makes unknown shapes non-errors, so anything
/// landing here is genuinely broken wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("malformed frame")]
pub struct MalformedFrame;

/// Decode one server→client frame. Mirrors the server's decode
/// discipline: discriminate the shape once (`type` vs `method` +
/// `jsonrpc`), then decode the known shape's fields; anything
/// unrecognized decodes as [`ServerFrame::Unknown`].
pub fn decode_server_frame(text: &str) -> Result<ServerFrame, MalformedFrame> {
    let value: Value = serde_json::from_str(text).map_err(|_| MalformedFrame)?;
    let object = value.as_object().ok_or(MalformedFrame)?;

    let version = match object.get("v") {
        // A present-but-non-integer version is malformed, not unknown
        // (the mirror of the server's decode.field(decode.int)).
        None | Some(Value::String(_)) => return Err(MalformedFrame),
        Some(v) => v.as_i64().ok_or(MalformedFrame)?,
    };
    if version != VERSION {
        return Ok(ServerFrame::Unknown);
    }
    // A present-but-non-string discriminator is malformed, not unknown
    // (the mirror of decode.optional_field(_, "", decode.string): a
    // present value runs the string decoder, and null refuses).
    let kind = discriminator(object, "type")?;
    let method = discriminator(object, "method")?;
    let jsonrpc = discriminator(object, "jsonrpc")?;

    match (kind.as_str(), method.as_str(), jsonrpc.as_str()) {
        // The tool plane validates its JSON-RPC marker: a frame that
        // falsifies or omits it cannot arrive as an executable request.
        ("", "tools/call", JSONRPC) => decode_call_request(&value),
        ("", "notifications/cancelled", JSONRPC) => decode_cancelled(&value),
        (kind, "", "") => decode_event(kind, &value),
        _ => Ok(ServerFrame::Unknown),
    }
}

fn discriminator(
    object: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<String, MalformedFrame> {
    match object.get(key) {
        None => Ok(String::new()),
        Some(Value::String(s)) => Ok(s.clone()),
        Some(_) => Err(MalformedFrame),
    }
}

fn decode_event(kind: &str, value: &Value) -> Result<ServerFrame, MalformedFrame> {
    let frame = match kind {
        "hello" => {
            let shape: HelloShape = shape(value)?;
            ServerFrame::Hello {
                model: shape.model,
                messages: shape.messages,
            }
        }
        "user" => {
            let shape: UserShape = shape(value)?;
            ServerFrame::User {
                text: shape.text,
                at: shape.at,
                author: shape.author,
            }
        }
        "assistant" => {
            let shape: AssistantShape = shape(value)?;
            ServerFrame::Assistant {
                text: shape.text,
                at: shape.at,
            }
        }
        "reasoning_full" => ServerFrame::ReasoningFull {
            text: shape::<TextShape>(value)?.text,
        },
        "token" => ServerFrame::Token {
            text: shape::<TextShape>(value)?.text,
        },
        "reasoning" => ServerFrame::Reasoning {
            text: shape::<TextShape>(value)?.text,
        },
        "turn_done" => {
            let shape: TurnDoneShape = shape(value)?;
            ServerFrame::TurnDone {
                at: shape.at,
                stats: shape.stats,
            }
        }
        "turn_stopped" => {
            let shape: TurnStoppedShape = shape(value)?;
            ServerFrame::TurnStopped {
                chars: shape.chars,
                at: shape.at,
            }
        }
        "turn_failed" => {
            let shape: TurnFailedShape = shape(value)?;
            ServerFrame::TurnFailed {
                reason: shape.reason,
                at: shape.at,
            }
        }
        "tool_call_delta" => {
            let shape: ToolCallDeltaShape = shape(value)?;
            ServerFrame::ToolCallDelta {
                index: shape.index,
                id: shape.id,
                name: shape.name,
                fragment: shape.fragment,
            }
        }
        "tool_call" => {
            let shape: ToolCallShape = shape(value)?;
            ServerFrame::ToolCall {
                id: shape.id,
                name: shape.name,
                arguments: shape.arguments,
                at: shape.at,
            }
        }
        "tool_result" => {
            let shape: ToolResultShape = shape(value)?;
            ServerFrame::ToolResult {
                call_id: shape.call_id,
                name: shape.name,
                ok: shape.ok,
                content: shape.content,
            }
        }
        "loop_status" => {
            let shape: LoopStatusShape = shape(value)?;
            ServerFrame::LoopStatus {
                iteration: shape.iteration,
                max: shape.max,
                phase: shape.phase,
            }
        }
        "meta" => {
            let shape: MetaShape = shape(value)?;
            ServerFrame::Meta {
                title: shape.title,
                tags: shape.tags,
            }
        }
        "attached" => ServerFrame::Attached {
            target: shape::<AttachedShape>(value)?.target,
        },
        "pong" => ServerFrame::Pong,
        "error" => ServerFrame::ProtocolError {
            message: shape::<ErrorShape>(value)?.message,
        },
        // Additive growth: an event KIND this client predates decodes
        // as Unknown — forward compatible by rule.
        _ => ServerFrame::Unknown,
    };
    Ok(frame)
}

fn shape<T: DeserializeOwned>(value: &Value) -> Result<T, MalformedFrame> {
    serde_json::from_value(value.clone()).map_err(|_| MalformedFrame)
}

#[derive(Deserialize)]
struct HelloShape {
    model: String,
    messages: i64,
}

#[derive(Deserialize)]
struct UserShape {
    text: String,
    #[serde(default)]
    at: Option<i64>,
    #[serde(default)]
    author: Option<String>,
}

#[derive(Deserialize)]
struct AssistantShape {
    text: String,
    #[serde(default)]
    at: Option<i64>,
}

#[derive(Deserialize)]
struct TextShape {
    text: String,
}

#[derive(Deserialize)]
struct TurnDoneShape {
    #[serde(default)]
    at: Option<i64>,
    stats: TurnStats,
}

#[derive(Deserialize)]
struct TurnStoppedShape {
    chars: i64,
    #[serde(default)]
    at: Option<i64>,
}

#[derive(Deserialize)]
struct TurnFailedShape {
    reason: String,
    #[serde(default)]
    at: Option<i64>,
}

#[derive(Deserialize)]
struct ToolCallDeltaShape {
    index: i64,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    fragment: String,
}

#[derive(Deserialize)]
struct ToolCallShape {
    id: String,
    name: String,
    arguments: String,
    #[serde(default)]
    at: Option<i64>,
}

#[derive(Deserialize)]
struct ToolResultShape {
    call_id: String,
    name: String,
    ok: bool,
    content: String,
}

#[derive(Deserialize)]
struct LoopStatusShape {
    iteration: i64,
    max: i64,
    phase: String,
}

#[derive(Deserialize)]
struct MetaShape {
    #[serde(default)]
    title: Option<String>,
    tags: Vec<String>,
}

#[derive(Deserialize)]
struct AttachedShape {
    target: String,
}

#[derive(Deserialize)]
struct ErrorShape {
    message: String,
}

fn decode_call_request(value: &Value) -> Result<ServerFrame, MalformedFrame> {
    let object = value.as_object().ok_or(MalformedFrame)?;
    let id = object
        .get("id")
        .and_then(Value::as_i64)
        .ok_or(MalformedFrame)?;
    let params = object.get("params").ok_or(MalformedFrame)?;
    let arguments = params.get("arguments").ok_or(MalformedFrame)?;
    if !arguments.is_object() {
        // The server proves `params.arguments` a JSON object before
        // dispatch (`valid_arguments`); anything else is broken wire.
        return Err(MalformedFrame);
    }
    Ok(ServerFrame::CallRequest {
        id,
        tool: params
            .get("name")
            .and_then(Value::as_str)
            .ok_or(MalformedFrame)?
            .to_owned(),
        arguments: arguments.clone(),
    })
}

fn decode_cancelled(value: &Value) -> Result<ServerFrame, MalformedFrame> {
    let params = value.get("params").ok_or(MalformedFrame)?;
    Ok(ServerFrame::Cancelled {
        id: params
            .get("requestId")
            .and_then(Value::as_i64)
            .ok_or(MalformedFrame)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    // -------------------------------------------------------------
    // Client → server encode (byte-exact wire shapes)
    // -------------------------------------------------------------

    #[test]
    fn encode_send_pins_exact_bytes() {
        assert_eq!(
            encode_send("run the deploy"),
            "{\"v\":1,\"type\":\"send\",\"text\":\"run the deploy\"}"
        );
    }

    #[test]
    fn encode_send_hostile_text_round_trips() {
        let frame = encode_send("say \"hi\"\nnewline");
        assert!(!frame.contains("\"hi\""));
        // Re-parse through a decoder mirror: the payload survives whole.
        let value: Value = serde_json::from_str(&frame).expect("frame stays one JSON document");
        assert_eq!(value["text"].as_str(), Some("say \"hi\"\nnewline"));
    }

    #[test]
    fn encode_ping_pins_exact_bytes() {
        assert_eq!(encode_ping(), "{\"v\":1,\"type\":\"ping\"}");
    }

    #[test]
    fn encode_call_response_ok_pins_exact_bytes() {
        assert_eq!(
            encode_call_response(3, false, "node says: hi"),
            r#"{"v":1,"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"node says: hi"}],"isError":false}}"#
        );
    }

    #[test]
    fn encode_call_response_error_carries_wire_flag() {
        let frame = encode_call_response(7, true, "boom");
        let value: Value = serde_json::from_str(&frame).unwrap();
        assert_eq!(value["result"]["isError"], Value::Bool(true));
        assert_eq!(value["result"]["content"][0]["text"], "boom");
    }

    // -------------------------------------------------------------
    // Server → client decode (byte-exact wire shapes)
    // -------------------------------------------------------------

    #[test]
    fn decode_hello_pins_shape() {
        assert_eq!(
            decode_server_frame("{\"v\":1,\"type\":\"hello\",\"model\":\"smart\",\"messages\":3}"),
            Ok(ServerFrame::Hello {
                model: "smart".to_owned(),
                messages: 3,
            })
        );
    }

    #[test]
    fn decode_user_pins_optional_fields() {
        assert_eq!(
            decode_server_frame(
                "{\"v\":1,\"type\":\"user\",\"text\":\"hi there\",\"at\":123,\"author\":\"system\"}"
            ),
            Ok(ServerFrame::User {
                text: "hi there".to_owned(),
                at: Some(123),
                author: Some("system".to_owned()),
            })
        );
        assert_eq!(
            decode_server_frame(
                "{\"v\":1,\"type\":\"user\",\"text\":\"x\",\"at\":null,\"author\":null}"
            ),
            Ok(ServerFrame::User {
                text: "x".to_owned(),
                at: None,
                author: None,
            })
        );
        // The roster line shape (author sidecar `system`).
        assert_eq!(
            decode_server_frame(
                "{\"v\":1,\"type\":\"user\",\"text\":\"attached targets: workspace.\",\"at\":null,\"author\":\"system\"}"
            ),
            Ok(ServerFrame::User {
                text: "attached targets: workspace.".to_owned(),
                at: None,
                author: Some("system".to_owned()),
            })
        );
    }

    #[test]
    fn decode_assistant_pins_shape() {
        assert_eq!(
            decode_server_frame("{\"v\":1,\"type\":\"assistant\",\"text\":\"done\",\"at\":5}"),
            Ok(ServerFrame::Assistant {
                text: "done".to_owned(),
                at: Some(5),
            })
        );
    }

    #[test_case("reasoning_full", "thinking"; "reasoning_full")]
    #[test_case("token", "fr"; "token")]
    #[test_case("reasoning", "why"; "reasoning")]
    fn decode_text_frames_pin_shape(kind: &str, text: &str) {
        let frame = format!("{{\"v\":1,\"type\":\"{kind}\",\"text\":\"{text}\"}}");
        let expected = match kind {
            "reasoning_full" => ServerFrame::ReasoningFull {
                text: text.to_owned(),
            },
            "token" => ServerFrame::Token {
                text: text.to_owned(),
            },
            _ => ServerFrame::Reasoning {
                text: text.to_owned(),
            },
        };
        assert_eq!(decode_server_frame(&frame), Ok(expected));
    }

    #[test]
    fn decode_turn_done_pins_stats_shape() {
        assert_eq!(
            decode_server_frame(
                r#"{"v":1,"type":"turn_done","at":77,"stats":{"ttft_ms":42,"duration_ms":1000,"token_deltas":3,"reasoning_deltas":0,"tool_call_deltas":2}}"#
            ),
            Ok(ServerFrame::TurnDone {
                at: Some(77),
                stats: TurnStats {
                    ttft_ms: Some(42),
                    duration_ms: 1000,
                    token_deltas: 3,
                    reasoning_deltas: 0,
                    tool_call_deltas: 2,
                },
            })
        );
        // ttft and at are optional at the wire (null reads None).
        assert_eq!(
            decode_server_frame(
                r#"{"v":1,"type":"turn_done","at":null,"stats":{"ttft_ms":null,"duration_ms":5,"token_deltas":0,"reasoning_deltas":0,"tool_call_deltas":0}}"#
            ),
            Ok(ServerFrame::TurnDone {
                at: None,
                stats: TurnStats {
                    ttft_ms: None,
                    duration_ms: 5,
                    token_deltas: 0,
                    reasoning_deltas: 0,
                    tool_call_deltas: 0,
                },
            })
        );
    }

    #[test]
    fn decode_turn_lifecycle_pins_shapes() {
        assert_eq!(
            decode_server_frame("{\"v\":1,\"type\":\"turn_stopped\",\"chars\":9,\"at\":4}"),
            Ok(ServerFrame::TurnStopped {
                chars: 9,
                at: Some(4),
            })
        );
        assert_eq!(
            decode_server_frame(
                "{\"v\":1,\"type\":\"turn_failed\",\"reason\":\"boom\",\"at\":null}"
            ),
            Ok(ServerFrame::TurnFailed {
                reason: "boom".to_owned(),
                at: None,
            })
        );
    }

    #[test]
    fn decode_tool_call_pins_arguments_as_string() {
        assert_eq!(
            decode_server_frame(
                r#"{"v":1,"type":"tool_call","id":"call_1","name":"bash","arguments":"{\"command\":\"echo hi\"}","at":9}"#
            ),
            Ok(ServerFrame::ToolCall {
                id: "call_1".to_owned(),
                name: "bash".to_owned(),
                arguments: "{\"command\":\"echo hi\"}".to_owned(),
                at: Some(9),
            })
        );
    }

    #[test]
    fn decode_tool_result_pins_shape() {
        assert_eq!(
            decode_server_frame(
                r#"{"v":1,"type":"tool_result","call_id":"call_1","name":"bash","ok":false,"content":"exit status 1"}"#
            ),
            Ok(ServerFrame::ToolResult {
                call_id: "call_1".to_owned(),
                name: "bash".to_owned(),
                ok: false,
                content: "exit status 1".to_owned(),
            })
        );
    }

    #[test]
    fn decode_tool_call_delta_pins_optional_id_name() {
        assert_eq!(
            decode_server_frame(
                r#"{"v":1,"type":"tool_call_delta","index":0,"id":"call_1","name":null,"fragment":"{"}"#
            ),
            Ok(ServerFrame::ToolCallDelta {
                index: 0,
                id: Some("call_1".to_owned()),
                name: None,
                fragment: "{".to_owned(),
            })
        );
    }

    #[test]
    fn decode_loop_status_and_meta_pin_shapes() {
        assert_eq!(
            decode_server_frame(
                "{\"v\":1,\"type\":\"loop_status\",\"iteration\":2,\"max\":8,\"phase\":\"tools\"}"
            ),
            Ok(ServerFrame::LoopStatus {
                iteration: 2,
                max: 8,
                phase: "tools".to_owned(),
            })
        );
        assert_eq!(
            decode_server_frame(
                "{\"v\":1,\"type\":\"meta\",\"title\":\"new title\",\"tags\":[\"a\",\"b\"]}"
            ),
            Ok(ServerFrame::Meta {
                title: Some("new title".to_owned()),
                tags: vec!["a".to_owned(), "b".to_owned()],
            })
        );
        assert_eq!(
            decode_server_frame("{\"v\":1,\"type\":\"meta\",\"title\":null,\"tags\":[]}"),
            Ok(ServerFrame::Meta {
                title: None,
                tags: vec![],
            })
        );
    }

    #[test]
    fn decode_connection_frames_pin_shapes() {
        assert_eq!(
            decode_server_frame("{\"v\":1,\"type\":\"attached\",\"target\":\"node-a\"}"),
            Ok(ServerFrame::Attached {
                target: "node-a".to_owned(),
            })
        );
        assert_eq!(
            decode_server_frame("{\"v\":1,\"type\":\"pong\"}"),
            Ok(ServerFrame::Pong)
        );
        assert_eq!(
            decode_server_frame("{\"v\":1,\"type\":\"error\",\"message\":\"refused\"}"),
            Ok(ServerFrame::ProtocolError {
                message: "refused".to_owned(),
            })
        );
    }

    /// The tool plane's request half: the mirror of the server's
    /// `encode_call_request` — arguments ride VERBATIM as a raw JSON
    /// object, read as a VALUE (noncanonical spacing survives).
    #[test]
    fn decode_call_request_reads_arguments_as_value() {
        let frame = r#"{"v":1,"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"bash","arguments":{"target": "node-a", "command":"echo hi"}}}"#.to_owned();
        assert_eq!(
            decode_server_frame(&frame),
            Ok(ServerFrame::CallRequest {
                id: 4,
                tool: "bash".to_owned(),
                arguments: serde_json::json!({"target": "node-a", "command": "echo hi"}),
            })
        );
    }

    #[test]
    fn decode_call_request_refuses_non_object_arguments() {
        let frame = r#"{"v":1,"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"bash","arguments":[1,2]}}"#;
        assert_eq!(decode_server_frame(frame), Err(MalformedFrame));
    }

    #[test]
    fn decode_cancelled_pins_shape() {
        assert_eq!(
            decode_server_frame(
                r#"{"v":1,"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":9}}"#
            ),
            Ok(ServerFrame::Cancelled { id: 9 })
        );
    }

    #[test]
    fn decode_tool_plane_refuses_falsified_jsonrpc_marker() {
        let frame = r#"{"v":1,"jsonrpc":"1.0","id":1,"method":"tools/call","params":{"name":"bash","arguments":{}}}"#;
        assert_eq!(decode_server_frame(frame), Ok(ServerFrame::Unknown));
    }

    #[test]
    fn decode_unknown_and_malformed_reflect_additive_contract() {
        // A KIND this client predates: Unknown, never fatal.
        assert_eq!(
            decode_server_frame("{\"v\":1,\"type\":\"telemetry_v2\"}"),
            Ok(ServerFrame::Unknown)
        );
        // A wrong VERSION decodes Unknown too (forward direction).
        assert_eq!(
            decode_server_frame("{\"v\":2,\"type\":\"send\"}"),
            Ok(ServerFrame::Unknown)
        );
        // Malformed JSON refuses.
        assert_eq!(decode_server_frame("not json at all"), Err(MalformedFrame));
        // A malformed KNOWN shape refuses.
        assert_eq!(
            decode_server_frame("{\"v\":1,\"type\":\"user\"}"),
            Err(MalformedFrame)
        );
        assert_eq!(
            decode_server_frame(
                r#"{"v":1,"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"bash"}}"#
            ),
            Err(MalformedFrame)
        );
        // Non-object frames and non-string discriminators refuse.
        assert_eq!(decode_server_frame("[1,2]"), Err(MalformedFrame));
        assert_eq!(
            decode_server_frame("{\"v\":1,\"type\":3,\"text\":\"x\"}"),
            Err(MalformedFrame)
        );
        // A missing or non-integer version refuses.
        assert_eq!(
            decode_server_frame("{\"type\":\"pong\"}"),
            Err(MalformedFrame)
        );
    }
}
