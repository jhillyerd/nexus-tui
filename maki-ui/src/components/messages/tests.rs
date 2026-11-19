//! Focused tests for the node-frame-driven panel.

use super::*;
use crate::chat::Chat;
use crate::node::NodeEvent;
use maki_config::ClockFormat;
use maki_lua::EventHandle;
use maki_storage::id::MakiId;

fn chat() -> Chat {
    Chat::new(
        MakiId::generate(),
        "test".into(),
        UiConfig::default(),
        EventHandle::disconnected_for_test(),
    )
}

fn clock() -> ClockFormat {
    ClockFormat::default()
}

fn protocol_stats() -> nexus::protocol::TurnStats {
    serde_json::from_str(
        r#"{"duration_ms":10,"token_deltas":1,"reasoning_deltas":0,"tool_call_deltas":0}"#,
    )
    .unwrap()
}

#[test]
fn user_rows_append_and_roster_lines_stay_notices() {
    let mut chat = chat();
    chat.handle_node_event(
        NodeEvent::User {
            text: "hello".into(),
            author: None,
        },
        clock(),
    );
    chat.handle_node_event(
        NodeEvent::User {
            text: "[system] attached targets: workspace.".into(),
            author: Some(crate::node::SYSTEM_AUTHOR.into()),
        },
        clock(),
    );
    assert_eq!(chat.message_count(), 2);
    assert!(matches!(
        chat.message_at(0).unwrap().role,
        DisplayRole::User
    ));
    assert!(matches!(
        chat.message_at(1).unwrap().role,
        DisplayRole::Notice
    ));
}

#[test]
fn token_deltas_stream_then_flush_into_one_row() {
    let mut chat = chat();
    chat.handle_node_event(NodeEvent::Token { text: "he".into() }, clock());
    chat.handle_node_event(NodeEvent::Token { text: "llo".into() }, clock());
    chat.handle_node_event(
        NodeEvent::Assistant {
            text: "committed".into(),
        },
        clock(),
    );
    assert_eq!(chat.message_count(), 2);
    assert_eq!(chat.message_at(0).unwrap().text, "hello");
    assert!(matches!(
        chat.message_at(0).unwrap().role,
        DisplayRole::Assistant
    ));
    assert!(chat.streaming_text_is_empty());
}

#[test]
fn tool_chip_opens_and_settles_with_its_result() {
    let mut chat = chat();
    chat.handle_node_event(
        NodeEvent::ToolCall {
            id: "t1".into(),
            name: "bash".into(),
            arguments: r#"{"command":"ls"}"#.into(),
        },
        clock(),
    );
    assert!(chat.has_tool("t1"));
    chat.handle_node_event(
        NodeEvent::ToolResult {
            call_id: "t1".into(),
            name: "bash".into(),
            ok: true,
            content: "file.rs".into(),
        },
        clock(),
    );
    let msg = chat.message_at(0).unwrap();
    assert!(matches!(&msg.role, DisplayRole::Tool(t) if t.status == ToolStatus::Success));
    assert!(msg.text.contains("file.rs"));
    assert!(msg.tool_output.is_some());
}

#[test]
fn a_failed_turn_flushes_and_leaves_an_error_row() {
    let mut chat = chat();
    chat.handle_node_event(
        NodeEvent::Token {
            text: "partial".into(),
        },
        clock(),
    );
    chat.handle_node_event(
        NodeEvent::TurnFailed {
            reason: "boom".into(),
        },
        clock(),
    );
    let last = chat.message_at(chat.message_count() - 1).unwrap();
    assert!(matches!(last.role, DisplayRole::Error));
    assert_eq!(last.text, "boom");
    assert!(chat.streaming_text_is_empty());
}

#[test]
fn reasoning_deltas_become_a_thinking_row_on_flush() {
    let mut chat = chat();
    chat.handle_node_event(NodeEvent::ReasoningDelta { text: "hmm".into() }, clock());
    chat.handle_node_event(
        NodeEvent::TurnDone {
            stats: protocol_stats(),
        },
        clock(),
    );
    assert!(matches!(
        chat.message_at(0).unwrap().role,
        DisplayRole::Thinking
    ));
}

#[test]
fn a_rejoin_clears_the_transcript_for_the_replay() {
    let mut chat = chat();
    chat.handle_node_event(
        NodeEvent::User {
            text: "old".into(),
            author: None,
        },
        clock(),
    );
    chat.handle_node_event(NodeEvent::Connected, clock());
    assert_eq!(chat.message_count(), 0);
}
