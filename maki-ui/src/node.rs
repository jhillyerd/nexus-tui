//! The nexus node seam: one connection, one thread, one transcript.
//!
//! The UI sees only [`NodeEvent`] — a [`nexus::protocol::ServerFrame`]
//! projected onto what a transcript renders — plus the connection's
//! lifecycle ([`ClientEvent::Connected`]/[`Reconnecting`]/[`Terminated`]).
//! The projection is pure and offline-testable; the wire shapes it reads
//! are pinned byte-exact in the `nexus` crate.

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

pub use nexus::client;
pub use nexus::client::ClientEvent;
use nexus::client::NodeConfig;
use nexus::client::Termination;
pub use nexus::protocol;
use nexus::protocol::ServerFrame;
use tokio::sync::mpsc::UnboundedReceiver;

/// The `user` frame's display-only sidecar value for roster lines, which
/// render as status lines, never chat bubbles (they must never trigger a
/// turn of their own either — the server guarantees that).
pub const SYSTEM_AUTHOR: &str = "system";

/// One thing the transcript or the status bar reacts to.
#[derive(Debug, Clone, PartialEq)]
pub enum NodeEvent {
    /// The socket is up; a `hello` and the full replay follow. The UI
    /// clears its transcript on receipt, so every (re)join renders from
    /// the replay alone.
    Connected,
    /// A live connection dropped; retrying after `after`.
    Reconnecting { attempt: u32, after: Duration },
    /// First frame of a (re)join: the thread's model and message count.
    Hello { model: String, messages: i64 },
    /// A committed user row. `author` is the display-only sidecar:
    /// [`SYSTEM_AUTHOR`] marks a roster line, which renders as a status
    /// line, not a chat bubble.
    User {
        text: String,
        author: Option<String>,
    },
    /// A committed assistant message.
    Assistant { text: String },
    /// A committed reasoning block.
    Reasoning { text: String },
    /// A streaming assistant delta.
    Token { text: String },
    /// A streaming reasoning delta.
    ReasoningDelta { text: String },
    /// A streaming tool-call fragment. The first fragment carrying an id
    /// and a name opens its chip; later ones only accumulate server-side.
    /// Fragments without an id project to nothing — the assembled
    /// [`NodeEvent::ToolCall`] opens the chip.
    ToolCallDelta { id: String, name: Option<String> },
    /// An assembled tool call: the chip's header. `arguments` is
    /// JSON-encoded as a string on the wire; display-only here.
    ToolCall {
        id: String,
        name: String,
        arguments: String,
    },
    /// A committed tool result: the chip settles.
    ToolResult {
        call_id: String,
        name: String,
        ok: bool,
        content: String,
    },
    /// The turn committed; per-turn stream metrics ride `stats`.
    TurnDone { stats: protocol::TurnStats },
    /// The turn was stopped after `chars` emitted.
    TurnStopped { chars: i64 },
    /// The turn failed; the reason is display text.
    TurnFailed { reason: String },
    /// Agent-loop iteration/phase for the status bar.
    LoopStatus {
        iteration: i64,
        max: i64,
        phase: String,
    },
    /// Thread title/tags for the status bar.
    Meta {
        title: Option<String>,
        tags: Vec<String>,
    },
    /// This node's roster target is live and routable.
    Attached { target: String },
    /// A protocol-level refusal (refused send, unknown frame):
    /// informational only, shown as a status line.
    ProtocolError { message: String },
    /// The loop ended; the process exits with its code.
    Terminated(Termination),
}

impl NodeEvent {
    /// The event-plane projection: one decoded frame in, one renderable
    /// event out. Unknown KINDs arrive as [`ServerFrame::Unknown`] and
    /// project to nothing (`None`) — skipped, never fatal.
    pub fn project(frame: ServerFrame) -> Option<NodeEvent> {
        let event = match frame {
            ServerFrame::Hello { model, messages } => NodeEvent::Hello { model, messages },
            ServerFrame::User {
                text,
                at: _,
                author,
            } => NodeEvent::User { text, author },
            ServerFrame::Assistant { text, at: _ } => NodeEvent::Assistant { text },
            ServerFrame::ReasoningFull { text } => NodeEvent::Reasoning { text },
            ServerFrame::Token { text } => NodeEvent::Token { text },
            ServerFrame::Reasoning { text } => NodeEvent::ReasoningDelta { text },
            ServerFrame::TurnDone { at: _, stats } => NodeEvent::TurnDone { stats },
            ServerFrame::TurnStopped { chars, at: _ } => NodeEvent::TurnStopped { chars },
            ServerFrame::TurnFailed { reason, at: _ } => NodeEvent::TurnFailed { reason },
            ServerFrame::ToolCallDelta {
                index: _,
                id,
                name,
                fragment: _,
            } => {
                // An id-less delta can never rendezvous with the assembled
                // `tool_call`'s chip; skip it — the assembled frame opens it.
                NodeEvent::ToolCallDelta { id: id?, name }
            }
            ServerFrame::ToolCall {
                id,
                name,
                arguments,
                at: _,
            } => NodeEvent::ToolCall {
                id,
                name,
                arguments,
            },
            ServerFrame::ToolResult {
                call_id,
                name,
                ok,
                content,
            } => NodeEvent::ToolResult {
                call_id,
                name,
                ok,
                content,
            },
            ServerFrame::LoopStatus {
                iteration,
                max,
                phase,
            } => NodeEvent::LoopStatus {
                iteration,
                max,
                phase,
            },
            ServerFrame::Meta { title, tags } => NodeEvent::Meta { title, tags },
            ServerFrame::Attached { target } => NodeEvent::Attached { target },
            ServerFrame::ProtocolError { message } => NodeEvent::ProtocolError { message },
            ServerFrame::Pong | ServerFrame::Unknown => return None,
            // Tool-plane frames are consumed inside the connection and
            // never forwarded; seeing one here is a client bug.
            ServerFrame::CallRequest { .. } | ServerFrame::Cancelled { .. } => return None,
        };
        Some(event)
    }
}

/// The UI's end of the node connection: queue user turns, and drop the
/// handle's shutdown on quit.
#[derive(Clone)]
pub struct NodeHandle {
    client: client::NodeClient,
    events: Arc<Mutex<Option<UnboundedReceiver<ClientEvent>>>>,
}

impl NodeHandle {
    /// A handle with no connection behind it, for benches and tests. Sends
    /// go nowhere; the event stream closes at once.
    pub fn detached() -> Self {
        let (client, events) = client::spawn_detached();
        Self {
            client,
            events: Arc::new(Mutex::new(Some(events))),
        }
    }

    /// Spawn the connection loop on the current runtime; the caller takes
    /// the event stream through [`Self::take_events`] and hands it to the
    /// event loop.
    pub fn spawn(config: NodeConfig) -> Self {
        let (client, events) = client::spawn(config);
        Self {
            client,
            events: Arc::new(Mutex::new(Some(events))),
        }
    }

    /// Take the event stream (once; the UI loop owns it from here).
    pub fn take_events(&self) -> Option<UnboundedReceiver<ClientEvent>> {
        self.events.lock().expect("events mutex").take()
    }

    /// Queue a user turn as a `send` frame. Dropped silently while
    /// disconnected — the caller decides (connected flag from
    /// [`NodeEvent::Connected`]/[`NodeEvent::Reconnecting`]).
    pub fn send(&self, text: &str) {
        self.client.send(text);
    }

    /// Begin a clean shutdown; the loop ends with
    /// [`Termination::Shutdown`] (exit 0).
    pub fn shutdown(&self) {
        self.client.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CALL_ID: &str = "call_1";
    const TOOL_NAME: &str = "bash";

    #[test]
    fn id_less_tool_call_delta_projects_to_nothing() {
        let frame = ServerFrame::ToolCallDelta {
            index: 0,
            id: None,
            name: Some(TOOL_NAME.to_owned()),
            fragment: String::new(),
        };
        assert_eq!(NodeEvent::project(frame), None);
    }

    #[test]
    fn tool_call_delta_keeps_the_wire_id() {
        let frame = ServerFrame::ToolCallDelta {
            index: 0,
            id: Some(CALL_ID.to_owned()),
            name: Some(TOOL_NAME.to_owned()),
            fragment: String::new(),
        };
        assert_eq!(
            NodeEvent::project(frame),
            Some(NodeEvent::ToolCallDelta {
                id: CALL_ID.to_owned(),
                name: Some(TOOL_NAME.to_owned()),
            })
        );
    }
}
