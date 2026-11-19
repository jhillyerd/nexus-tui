//! The one transcript: the node's thread as rendered rows, streaming
//! segments, tool chips, and the scroll state over them.

pub use crate::components::messages::ScrollPos;

use super::components::messages::ClickOutcome;
use crate::components::messages::MessagesPanel;
use crate::components::tool_display::format_timestamp_now;
use crate::components::{DisplayMessage, DisplayRole};
use crate::node::NodeEvent;
use crate::node::SYSTEM_AUTHOR;
use crate::selection::{DocPos, RowPos, Selection};
use maki_config::{ClockFormat, UiConfig};
use maki_lua::{EventHandle, LuaCommandInfo, WinView};
use maki_storage::id::MakiId;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Color;

use crate::repaint::{Cadence, Dirty};

/// How a turn ended, for the app's status handling.
#[derive(Debug, Clone, PartialEq)]
pub enum TurnEnd {
    Done,
    Stopped,
    Failed(String),
}

pub struct Chat {
    pub name: String,
    messages_panel: MessagesPanel,
}

impl Chat {
    pub fn new(
        session_id: MakiId,
        name: String,
        ui_config: UiConfig,
        lua_event_handle: EventHandle,
    ) -> Self {
        let mut messages_panel = MessagesPanel::new(ui_config, lua_event_handle);
        messages_panel.set_chat(session_id);
        Self {
            name,
            messages_panel,
        }
    }

    /// One event-plane frame, projected, landed on the transcript. The
    /// replay and mid-turn joins both ride this path: `hello` clears, and
    /// every committed row appends in wire order.
    pub fn handle_node_event(&mut self, event: NodeEvent, clock_format: ClockFormat) {
        let panel = &mut self.messages_panel;
        match event {
            NodeEvent::Connected => {
                // A (re)join replays the whole thread; start from empty so
                // the replay alone paints it.
                panel.load_messages(Vec::new());
            }
            NodeEvent::Hello { .. } => {}
            NodeEvent::User { text, author } => {
                panel.flush();
                if author.as_deref() == Some(SYSTEM_AUTHOR) {
                    // Roster lines are status lines, never chat bubbles.
                    panel.push(DisplayMessage::new(DisplayRole::Notice, text));
                } else {
                    panel.push(user_message(text, clock_format));
                }
            }
            NodeEvent::Assistant { text } => {
                panel.flush();
                panel.push(DisplayMessage::new(DisplayRole::Assistant, text));
            }
            NodeEvent::Reasoning { text } => {
                panel.flush();
                panel.push(DisplayMessage::new(DisplayRole::Thinking, text));
            }
            NodeEvent::Token { text } => panel.text_delta(&text),
            NodeEvent::ReasoningDelta { text } => panel.thinking_delta(&text),
            NodeEvent::ToolCallDelta { id, name } => {
                if let Some(name) = name {
                    panel.tool_call(id, &name, String::new());
                }
            }
            NodeEvent::ToolCall {
                id,
                name,
                arguments,
            } => {
                panel.tool_call(id.clone(), &name, tool_header(&name, &arguments));
            }
            NodeEvent::ToolResult {
                call_id,
                name,
                ok,
                content,
            } => {
                panel.tool_result(&call_id, &name, ok, content);
            }
            NodeEvent::TurnDone { .. } | NodeEvent::TurnStopped { .. } => panel.flush(),
            NodeEvent::TurnFailed { reason } => {
                panel.flush();
                panel.fail_in_progress_with_message(reason.clone());
                panel.push(DisplayMessage::new(DisplayRole::Error, reason));
            }
            NodeEvent::ProtocolError { message } => {
                panel.flush();
                panel.push(DisplayMessage::new(DisplayRole::Notice, message));
            }
            // The status bar's data; the transcript is silent.
            NodeEvent::Reconnecting { .. }
            | NodeEvent::LoopStatus { .. }
            | NodeEvent::Meta { .. }
            | NodeEvent::Attached { .. }
            | NodeEvent::Terminated(_) => {}
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn set_name(&mut self, name: String) {
        self.name = name;
    }

    /// The tool chip a cancel leaves dangling settles as cancelled.
    pub fn cancel_running_tools(&mut self) {
        self.messages_panel.cancel_in_progress();
    }

    pub fn stream_reset(&mut self) {
        self.messages_panel.stream_reset();
    }

    pub fn messages_panel(&mut self) -> &mut MessagesPanel {
        &mut self.messages_panel
    }

    /// The chat id the hints filter on.
    pub fn session_id(&self) -> Option<MakiId> {
        self.messages_panel.session_id()
    }

    pub fn scroll(&mut self, delta: i32) {
        self.messages_panel.scroll(delta);
    }

    pub fn scroll_to_row(&mut self, doc_row: u32) {
        self.messages_panel.scroll_to_row(doc_row);
    }

    pub fn page(&self) -> i32 {
        self.messages_panel.page()
    }

    pub fn win_view(&self) -> WinView {
        self.messages_panel.win_view()
    }

    pub fn auto_scroll(&self) -> bool {
        self.messages_panel.auto_scroll()
    }

    pub fn scroll_to_top(&mut self) {
        self.messages_panel.scroll_to_top();
    }

    pub fn enable_auto_scroll(&mut self) {
        self.messages_panel.enable_auto_scroll();
    }

    pub fn scroll_to_segment(&mut self, segment_index: usize) {
        self.messages_panel.scroll_to_segment(segment_index);
    }

    pub fn restore_scroll(&mut self, scroll: ScrollPos, auto_scroll: bool) {
        self.messages_panel.restore_scroll(scroll, auto_scroll);
    }

    pub fn set_highlight_segment(&mut self, idx: Option<usize>) {
        self.messages_panel.set_highlight_segment(idx);
    }

    pub fn set_accent(&mut self, color: Color) {
        self.messages_panel.set_accent(color);
    }

    pub fn tick(&mut self) -> Dirty {
        self.messages_panel.tick()
    }

    pub fn cadence(&self) -> Cadence {
        self.messages_panel.cadence()
    }

    pub fn view(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        has_selection: bool,
        lua_commands: &[LuaCommandInfo],
    ) {
        self.messages_panel
            .view(frame, area, has_selection, lua_commands);
    }

    pub fn scroll_pos(&self) -> ScrollPos {
        self.messages_panel.scroll_pos()
    }

    pub fn doc_pos_at(&self, rel_row: u16, col: u16) -> DocPos {
        self.messages_panel.doc_pos_at(rel_row, col)
    }

    pub fn project_row(&self, pos: DocPos) -> RowPos {
        self.messages_panel.project_row(pos)
    }

    pub fn segment_search_texts(&self) -> Vec<String> {
        self.messages_panel.segment_search_texts()
    }

    pub fn extract_selection_text(&self, sel: &Selection, msg_area: Rect) -> String {
        self.messages_panel.extract_selection_text(sel, msg_area)
    }

    pub fn handle_click(&mut self, row: u16, col: u16, area: Rect) -> ClickOutcome {
        self.messages_panel.handle_click(row, col, area)
    }

    pub fn toggle_click(&mut self, row: u16, area: Rect) -> bool {
        self.messages_panel.toggle_click(row, area)
    }

    pub fn has_tool(&self, tool_id: &str) -> bool {
        self.messages_panel.has_tool(tool_id)
    }

    pub fn last_message_text(&self) -> &str {
        self.messages_panel.last_message_text()
    }

    #[cfg(test)]
    pub fn message_count(&self) -> usize {
        self.messages_panel.message_count()
    }

    #[cfg(test)]
    pub fn message_at(&self, index: usize) -> Option<&DisplayMessage> {
        self.messages_panel.message_at(index)
    }

    #[cfg(test)]
    pub fn last_message_role(&self) -> Option<&DisplayRole> {
        self.messages_panel.last_message_role()
    }

    #[cfg(test)]
    pub fn streaming_text_is_empty(&self) -> bool {
        self.messages_panel.streaming_text_is_empty()
    }

    pub fn has_user_message(&self) -> bool {
        self.messages_panel.has_user_message()
    }

    pub fn in_progress_count(&self) -> usize {
        self.messages_panel.in_progress_count()
    }
}

fn user_message(text: String, clock_format: ClockFormat) -> DisplayMessage {
    let mut msg = DisplayMessage::new(DisplayRole::User, text);
    msg.timestamp = Some(format_timestamp_now(clock_format));
    msg
}

/// The chip's header from the call's arguments: the command for `bash`,
/// the path for the file tools, the directory for `list_dir`.
fn tool_header(name: &str, arguments: &str) -> String {
    let parsed: Option<serde_json::Value> = serde_json::from_str(arguments).ok();
    let value = parsed.as_ref();
    let path = || {
        value
            .and_then(|v| v.get("path"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    };
    match name {
        "bash" => value
            .and_then(|v| v.get("command"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        "read_file" | "write_file" => path(),
        "list_dir" => path().or_else(|| Some(".".to_owned())),
        _ => None,
    }
    .unwrap_or_default()
}
