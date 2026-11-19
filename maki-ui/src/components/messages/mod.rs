mod render;
mod scroll;
mod segment;
mod selection;
#[cfg(test)]
mod tests;

pub use self::scroll::ScrollPos;

use self::render::RenderCursor;
use self::scroll::Layout;
use self::segment::{Segment, SegmentCache, body_width};

use super::tool_display::{
    RenderCtx, RoleStyle, ToolLines, assistant_style, done_style, error_style,
    format_timestamp_now, notice_style, search_text_for, thinking_style, truncate_to_header,
    user_style,
};
use super::{
    DisplayMessage, DisplayRole, ToolOutput, ToolRole, ToolStatus, code_view::SectionFlags,
};
use crate::animation::spinner_str;
use crate::markdown::{plain_lines, text_to_lines, truncate_output};
use crate::render_worker::RenderWorker;
use crate::selection::{DocPos, RowPos, Selection};
use crate::splash::{ColorTransition, Splash};
use crate::theme;
use crate::update;
use maki_config::{ClockFormat, ThinkingLines, ToolOutputLines, UiConfig};
use maki_lua::{EventHandle, LuaCommandInfo, WinView};
use maki_storage::id::MakiId;

use std::collections::HashMap;
use std::mem;
use std::sync::Arc;
use std::time::Instant;

use super::scrollbar::{self, render_vertical_scrollbar};
use super::streaming_content::StreamingContent;
use super::thinking;
use ratatui::Frame;
use ratatui::layout::Rect;

use crate::repaint::{Cadence, Dirty};
use ratatui::text::{Line, Span};

const REFLOW_MARGIN_VIEWPORTS: u32 = 1;
/// A click on transcript thinking expands or collapses it.
const CLICKABLE: bool = true;

/// What a click in the transcript asks of the app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClickOutcome {
    None,
    Handled,
}

pub struct MessagesPanel {
    messages: Vec<DisplayMessage>,
    streaming_thinking: StreamingContent,
    streaming_text: StreamingContent,
    started_at: Instant,
    scroll: ScrollPos,
    auto_scroll: bool,
    viewport_height: u16,
    viewport_width: u16,
    cache: SegmentCache,
    hl_worker: RenderWorker,
    theme_generation: u64,
    highlight_segment: Option<usize>,
    idle_splash: Splash,
    accent: ColorTransition,
    expanded_tools: HashMap<String, SectionFlags>,
    tool_output_lines: ToolOutputLines,
    #[allow(dead_code)] // the lua window surface this panel once served
    lua_event_handle: EventHandle,
    thinking_lines: ThinkingLines,
    /// The reader's click on the live thinking, handed to its message by
    /// [`Self::flush_thinking`].
    live_thinking_expanded: bool,
    clock_format: ClockFormat,
    /// The chat this panel shows, stamped so lua windows can address it.
    session_id: Option<MakiId>,
    /// Where the streaming segments start. `view` drops and rebuilds them
    /// every frame, so they always sit at the end of the cache.
    live_start: usize,
}

impl MessagesPanel {
    pub fn new(ui_config: UiConfig, lua_event_handle: EventHandle) -> Self {
        let thinking = thinking_style();
        let assistant = assistant_style();
        let ms = ui_config.typewriter_ms_per_char;
        Self {
            messages: Vec::new(),
            streaming_thinking: StreamingContent::new(
                thinking.label,
                thinking.text_style,
                thinking.label_style(),
                ms,
            ),
            streaming_text: StreamingContent::new(
                assistant.label,
                assistant.text_style,
                assistant.label_style(),
                ms,
            ),
            started_at: Instant::now(),
            scroll: ScrollPos::default(),
            auto_scroll: true,
            viewport_height: 24,
            viewport_width: crossterm::terminal::size().map_or(80, |(w, _)| w.saturating_sub(1)),
            cache: SegmentCache::new(),
            hl_worker: RenderWorker::new(),
            theme_generation: theme::generation(),
            highlight_segment: None,
            idle_splash: Splash::new(ui_config.splash_animation),
            accent: ColorTransition::new(theme::current().mode_build),
            expanded_tools: HashMap::new(),
            tool_output_lines: ui_config.tool_output_lines,
            lua_event_handle,
            thinking_lines: ui_config.thinking_lines,
            live_thinking_expanded: false,
            clock_format: ui_config.clock_format,
            session_id: None,
            live_start: 0,
        }
    }

    pub fn set_chat(&mut self, session_id: MakiId) {
        self.session_id = Some(session_id);
    }

    pub fn session_id(&self) -> Option<MakiId> {
        self.session_id
    }

    /// Hands back the index of the message, which [`Self::replace`] needs to
    /// correct it later.
    pub fn push(&mut self, msg: DisplayMessage) -> usize {
        self.messages.push(msg);
        self.messages.len() - 1
    }

    pub fn replace(&mut self, index: usize, msg: DisplayMessage) {
        let Some(slot) = self.messages.get_mut(index) else {
            return;
        };
        *slot = msg;
        self.cache.clear();
    }

    pub fn load_messages(&mut self, msgs: Vec<DisplayMessage>) {
        self.messages = msgs;
        self.cache.clear();
        self.expanded_tools.clear();
        self.highlight_segment = None;
        self.live_thinking_expanded = false;
    }

    pub fn thinking_delta(&mut self, text: &str) {
        self.streaming_thinking.push(text);
    }

    pub fn text_delta(&mut self, text: &str) {
        self.flush_thinking();
        self.streaming_text.push(text);
    }

    /// Opens a tool chip: header line, spinner gutter, body to follow.
    pub fn tool_call(&mut self, id: String, name: &str, header: String) {
        self.flush();
        if self.find_tool_msg(&id).is_some() {
            self.update_tool(&id, |msg| {
                msg.text = header;
            });
            return;
        }
        let role = DisplayRole::Tool(Box::new(ToolRole {
            id,
            status: ToolStatus::InProgress,
            name: Arc::from(name),
        }));
        let mut msg = DisplayMessage::new(role, header);
        msg.timestamp = Some(format_timestamp_now(self.clock_format));
        self.messages.push(msg);
    }

    /// Settles a tool chip with its flattened result text. Node bytes are
    /// external content: capped on entry by the tool plane, rendered inertly
    /// here.
    pub fn tool_result(&mut self, call_id: &str, name: &str, ok: bool, content: String) {
        let max_lines = self.tool_output_lines.get(name);
        let Some(msg) = self.find_tool_msg_mut(call_id) else {
            return;
        };
        if let DisplayRole::Tool(t) = &mut msg.role {
            t.status = if ok {
                ToolStatus::Success
            } else {
                ToolStatus::Error
            };
        }
        truncate_to_header(&mut msg.text);
        let tr = truncate_output(&content, max_lines);
        msg.truncated_lines = tr.skipped;
        if !tr.kept.is_empty() {
            msg.text.push('\n');
            msg.text.push_str(&tr.kept);
        }
        msg.tool_output = Some(std::sync::Arc::new(ToolOutput::Plain(content)));
        self.rebuild_tool_segment(call_id);
    }

    /// Ends a row that no `tool_result` will reach.
    pub fn settle_tool(&mut self, tool_id: &str, status: ToolStatus) {
        self.update_tool(tool_id, |msg| {
            if let DisplayRole::Tool(tool) = &mut msg.role {
                tool.status = status;
            }
        });
    }

    pub fn update_tool_summary(&mut self, tool_id: &str, summary: &str) {
        self.update_tool(tool_id, |msg| msg.text = summary.to_owned());
    }

    pub fn annotate_tool(&mut self, tool_id: &str, note: &str) {
        self.update_tool(tool_id, |msg| match &mut msg.annotation {
            Some(a) => a.push_str(&format!(" · {note}")),
            None => msg.annotation = Some(note.to_owned()),
        });
    }

    pub fn has_tool(&self, tool_id: &str) -> bool {
        self.find_tool_msg(tool_id).is_some()
    }

    fn update_tool(&mut self, tool_id: &str, update_msg: impl FnOnce(&mut DisplayMessage)) {
        let Some(msg) = self.find_tool_msg_mut(tool_id) else {
            return;
        };
        update_msg(msg);
        self.rebuild_tool_segment(tool_id);
    }

    pub fn stream_reset(&mut self) {
        self.streaming_thinking.clear();
        self.streaming_text.clear();
        self.live_thinking_expanded = false;
        self.cancel_in_progress();
    }

    /// Fails every in-progress chip with {message}, e.g. a dropped
    /// connection mid-turn.
    pub fn fail_in_progress_with_message(&mut self, message: String) {
        let ids: Vec<String> = self
            .messages
            .iter()
            .filter_map(|m| {
                if let DisplayRole::Tool(t) = &m.role
                    && t.status == ToolStatus::InProgress
                {
                    Some(t.id.clone())
                } else {
                    None
                }
            })
            .collect();
        for id in ids {
            self.update_tool(&id, |msg| {
                if let DisplayRole::Tool(t) = &mut msg.role {
                    t.status = ToolStatus::Error;
                }
                truncate_to_header(&mut msg.text);
                msg.text.push('\n');
                msg.text.push_str(&message);
            });
        }
    }

    pub fn cancel_in_progress(&mut self) {
        let affected_ids: Vec<String> = self
            .messages
            .iter_mut()
            .filter_map(|msg| {
                if let DisplayRole::Tool(t) = &mut msg.role
                    && t.status == ToolStatus::InProgress
                {
                    t.status = ToolStatus::Error;
                    Some(t.id.clone())
                } else {
                    None
                }
            })
            .collect();

        for id in &affected_ids {
            self.rebuild_tool_segment(id);
        }
    }

    pub fn in_progress_count(&self) -> usize {
        self.messages
            .iter()
            .filter(
                |m| matches!(&m.role, DisplayRole::Tool(t) if t.status == ToolStatus::InProgress),
            )
            .count()
    }

    #[cfg(test)]
    pub fn message_count(&self) -> usize {
        self.messages.len()
    }

    #[cfg(test)]
    pub fn message_at(&self, index: usize) -> Option<&DisplayMessage> {
        self.messages.get(index)
    }

    pub fn has_user_message(&self) -> bool {
        self.messages.iter().any(|m| m.role == DisplayRole::User)
    }

    pub fn last_message_text(&self) -> &str {
        self.messages.last().map(|m| m.text.as_str()).unwrap_or("")
    }

    #[cfg(test)]
    pub fn last_message_role(&self) -> Option<&DisplayRole> {
        self.messages.last().map(|m| &m.role)
    }

    #[cfg(test)]
    pub fn streaming_text_is_empty(&self) -> bool {
        self.streaming_text.is_empty()
    }

    #[cfg(test)]
    pub fn streaming_thinking_is_empty(&self) -> bool {
        self.streaming_thinking.is_empty()
    }

    pub fn flush(&mut self) {
        self.flush_thinking();
        if !self.streaming_text.is_empty() {
            self.messages.push(DisplayMessage::new(
                DisplayRole::Assistant,
                self.streaming_text.take_all(),
            ));
        }
    }

    fn layout(&self) -> Layout<'_> {
        Layout::new(&self.cache, self.viewport_width)
    }

    /// Positive scrolls up. Clamping is immediate rather than deferred to the
    /// next `view`, so scrolling back up starts from the bottom row and not
    /// from wherever an overscroll left the position.
    pub fn scroll(&mut self, delta: i32) {
        let rows = delta.unsigned_abs();
        let layout = self.layout();
        let moved = if delta >= 0 {
            layout.retreat(self.scroll, rows)
        } else {
            layout.advance(self.scroll, rows)
        };
        let clamped = moved.min(layout.bottom(self.viewport_height));
        self.scroll_to(clamped);
    }

    /// Always unpins, and the next `view` re-pins if this lands on the
    /// bottom line.
    fn scroll_to(&mut self, pos: ScrollPos) {
        self.scroll = pos;
        self.auto_scroll = false;
    }

    pub fn auto_scroll(&self) -> bool {
        self.auto_scroll
    }

    /// Backs `maki.fn.winrestview`, the one caller that still speaks in
    /// document rows.
    pub fn scroll_to_row(&mut self, doc_row: u32) {
        self.scroll_to(self.layout().at_row(doc_row));
    }

    pub fn scroll_to_top(&mut self) {
        self.scroll_to(ScrollPos::default());
    }

    pub fn enable_auto_scroll(&mut self) {
        self.auto_scroll = true;
    }

    pub fn scroll_to_segment(&mut self, segment_index: usize) {
        self.scroll_to(ScrollPos {
            seg: segment_index,
            row: 0,
        });
    }

    pub fn restore_scroll(&mut self, scroll: ScrollPos, auto_scroll: bool) {
        self.scroll = scroll;
        self.auto_scroll = auto_scroll;
    }

    pub fn set_highlight_segment(&mut self, idx: Option<usize>) {
        self.highlight_segment = idx;
    }

    pub fn half_page(&self) -> i32 {
        self.viewport_height as i32 / 2
    }

    pub fn page(&self) -> i32 {
        self.viewport_height.max(1) as i32
    }

    pub fn set_accent(&mut self, color: ratatui::style::Color) {
        self.accent.set(color);
    }

    pub fn handle_click(&mut self, row: u16, _col: u16, area: Rect) -> ClickOutcome {
        if self.toggle_click(row, area) {
            ClickOutcome::Handled
        } else {
            ClickOutcome::None
        }
    }

    pub fn toggle_click(&mut self, row: u16, area: Rect) -> bool {
        self.toggle_at(row, area)
    }

    fn toggle_at(&mut self, row: u16, area: Rect) -> bool {
        if area.height == 0 {
            return false;
        }
        let pos = self
            .layout()
            .advance(self.scroll, u32::from(row.saturating_sub(area.y)));
        let width = self.viewport_width;
        if pos.seg >= self.live_start {
            return self.try_toggle_live_thinking(pos);
        }
        let Some(seg) = self.cache.get(pos.seg) else {
            return false;
        };
        let Some(tool_id) = seg.tool_id.as_deref() else {
            return self.try_toggle_cached_thinking(pos.seg, width);
        };
        let exp = self
            .expanded_tools
            .get(tool_id)
            .copied()
            .unwrap_or_default();
        if !seg.truncation.any() && !exp.any() {
            return false;
        }
        let tool_id = tool_id.to_owned();
        let truncation = seg.truncation;

        let entry = self.expanded_tools.entry(tool_id.clone()).or_default();
        if truncation.output || entry.output {
            entry.output = !entry.output;
        } else if truncation.script || entry.script {
            entry.script = !entry.script;
        }
        self.rebuild_tool_segment(&tool_id);
        true
    }

    /// Drains the highlight worker; the splash animation too while the
    /// transcript is empty.
    pub fn tick(&mut self) -> Dirty {
        let mut dirty = self.drain_highlights();
        if self.show_idle_splash() {
            dirty |= self.idle_splash.poll_update(update::latest_version());
        }
        dirty
    }

    pub fn cadence(&self) -> Cadence {
        let smooth = self.streaming_text.is_animating()
            || self.accent.is_animating()
            || (self.streaming_thinking.is_animating()
                && thinking::reveals_text(self.thinking_lines, self.live_thinking_expanded));
        Cadence::any([
            // A running tool draws a spinner. Its output arriving is data, and
            // `tick` reports that separately.
            Cadence::when(self.in_progress_count() > 0, Cadence::SPINNER),
            Cadence::when(smooth, Cadence::SMOOTH),
            Cadence::when(self.show_idle_splash(), self.idle_splash.cadence()),
        ])
    }

    fn show_idle_splash(&self) -> bool {
        self.messages.is_empty()
            && self.streaming_thinking.is_empty()
            && self.streaming_text.is_empty()
    }

    pub fn view(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        has_selection: bool,
        lua_commands: &[LuaCommandInfo],
    ) {
        self.viewport_height = area.height;
        let width = area.width.saturating_sub(1);
        let theme_gen = theme::generation();
        let needs_reflow = self.viewport_width != width || self.theme_generation != theme_gen;
        if needs_reflow {
            self.viewport_width = width;
            self.theme_generation = theme_gen;
        }

        // Last frame's streaming segments go first. Otherwise a message that
        // `flush` just committed would be built after them and show up twice.
        self.cache.truncate(self.live_start);
        if self.show_idle_splash() {
            let accent = self.accent.resolve();
            self.idle_splash
                .render(area, frame.buffer_mut(), accent, lua_commands);
            return;
        }

        if needs_reflow {
            self.cache.mark_all_stale();
            let thinking = thinking_style();
            let assistant = assistant_style();
            self.streaming_thinking.set_style(
                thinking.label,
                thinking.text_style,
                thinking.label_style(),
            );
            self.streaming_text.set_style(
                assistant.label,
                assistant.text_style,
                assistant.label_style(),
            );
        }
        self.rebuild_line_cache();
        if self.in_progress_count() > 0 {
            self.update_spinners();
        }

        self.rebuild_live_segments(width);

        // The reflow window is picked from `scroll` and the bottom pin, and
        // the reflow changes the heights both are derived from: resolve
        // before to aim the window, and after to place the result.
        self.resolve_scroll(has_selection);
        self.reflow_viewport(width);
        self.resolve_scroll(has_selection);

        let viewport = Rect::new(area.x, area.y, width, area.height);
        let mut cursor = RenderCursor::new(self.scroll.row, viewport);

        for (i, seg) in self
            .cache
            .segments_mut()
            .iter_mut()
            .enumerate()
            .skip(self.scroll.seg)
        {
            if cursor.past_bottom() {
                break;
            }
            let highlight = self.highlight_segment == Some(i);
            let style = if seg.tool_id.is_some() {
                Some(theme::current().tool_bg)
            } else if seg.msg_index.is_some_and(|idx| {
                self.messages
                    .get(idx)
                    .is_some_and(|m| matches!(&m.role, DisplayRole::User))
            }) {
                Some(theme::current().user_bg)
            } else {
                None
            };
            cursor.render_segment(seg, style, highlight, frame);
        }

        // Both walks are O(transcript) and a resize makes each one re-wrap
        // every segment it touches, so they stay behind the toggle that
        // decides whether anything is drawn from them.
        if scrollbar::is_enabled() {
            let layout = self.layout();
            let total_rows = layout.total_rows();
            if total_rows > u32::from(area.height) {
                render_vertical_scrollbar(frame, area, total_rows, layout.doc_row(self.scroll));
            }
        }
    }

    /// Same order and spacers as `rebuild_line_cache` builds once they flush,
    /// so a [`ScrollPos`] still points at the same text after the flush.
    fn rebuild_live_segments(&mut self, width: u16) {
        self.live_start = self.cache.len();
        let width = body_width(width);
        if !self.streaming_thinking.is_empty() {
            let collapsed = thinking::collapsed_view(
                thinking::tail_rows(self.thinking_lines, self.live_thinking_expanded),
                self.streaming_thinking.line_count(),
                CLICKABLE,
                |rows| {
                    self.streaming_thinking
                        .render_tail(rows, width)
                        .map(<[_]>::to_vec)
                },
            );
            let drawn_collapsed = collapsed.is_some();
            let lines =
                collapsed.unwrap_or_else(|| self.streaming_thinking.render_lines(width).to_vec());
            self.push_live_segment(lines, drawn_collapsed, thinking_style().gutter());
        }
        if !self.streaming_text.is_empty() {
            let lines = self.streaming_text.render_lines(width).to_vec();
            self.push_live_segment(lines, false, assistant_style().gutter());
        }
    }

    fn push_live_segment(
        &mut self,
        lines: Vec<Line<'static>>,
        collapsed: bool,
        gutter: Span<'static>,
    ) {
        self.cache.push_spacer_if_needed();
        let mut seg = Segment::with_lines(lines, None);
        seg.gutter = Some(gutter);
        seg.truncation.output = collapsed;
        self.cache.push(seg);
    }

    pub fn scroll_pos(&self) -> ScrollPos {
        self.scroll
    }

    /// Where a click at `rel_row` rows into the viewport lands, as a place a
    /// resize cannot move.
    pub fn doc_pos_at(&self, rel_row: u16, col: u16) -> DocPos {
        let pos = self.layout().advance(self.scroll, u32::from(rel_row));
        DocPos {
            seg: pos.seg,
            row: pos.row,
            col,
        }
    }

    /// Walks at most a viewport worth of segments, so a live selection costs
    /// O(viewport) per frame rather than O(transcript).
    pub fn project_row(&self, pos: DocPos) -> RowPos {
        let at = ScrollPos {
            seg: pos.seg,
            row: pos.row,
        };
        if at < self.scroll {
            return RowPos::Above;
        }
        let rows = self.layout().rows_from(self.scroll, at);
        if rows >= u32::from(self.viewport_height) {
            RowPos::Below
        } else {
            RowPos::At(rows as u16)
        }
    }

    /// Backs `maki.fn.winsaveview`. The clamp matters: a pinned or restored
    /// scroll position can sit past the end until the next `view` resolves it
    /// against the current line count.
    pub fn win_view(&self) -> WinView {
        let layout = self.layout();
        let bottom = layout.bottom(self.viewport_height);
        WinView {
            scroll_top: layout.doc_row(layout.clamp(self.scroll.min(bottom))),
            line_count: layout.total_rows(),
            height: self.viewport_height,
            auto_scroll: self.auto_scroll,
        }
    }

    /// Built on demand rather than retained: a plain-text copy of the whole
    /// transcript, sitting beside the rendered one, was the second largest
    /// thing the panel held. Entry `i` must describe segment `i`, because
    /// `SearchAction::Select` feeds the index straight back to
    /// [`Self::scroll_to_segment`].
    pub fn segment_search_texts(&self) -> Vec<String> {
        let by_tool: HashMap<&str, &DisplayMessage> = self
            .messages
            .iter()
            .filter_map(|m| match &m.role {
                DisplayRole::Tool(t) => Some((t.id.as_str(), m)),
                _ => None,
            })
            .collect();
        self.cache
            .segments()
            .iter()
            .map(|seg| {
                match seg.tool_id.as_deref() {
                    Some(id) => by_tool.get(id).map(|m| search_text_for(m)),
                    None => seg
                        .msg_index
                        .and_then(|i| self.messages.get(i))
                        .map(message_search_text),
                }
                .unwrap_or_default()
            })
            .collect()
    }

    pub fn extract_selection_text(&self, sel: &Selection, msg_area: Rect) -> String {
        selection::extract_selection_text(&self.cache, self.viewport_width, sel, msg_area)
    }

    fn find_tool_msg(&self, tool_id: &str) -> Option<&DisplayMessage> {
        self.messages
            .iter()
            .rfind(|m| matches!(&m.role, DisplayRole::Tool(t) if t.id == tool_id))
    }

    fn find_tool_msg_mut(&mut self, tool_id: &str) -> Option<&mut DisplayMessage> {
        self.messages
            .iter_mut()
            .rfind(|m| matches!(&m.role, DisplayRole::Tool(t) if t.id == tool_id))
    }

    fn rctx(&self) -> RenderCtx<'_> {
        RenderCtx {
            started_at: self.started_at,
            width: body_width(self.viewport_width),
            tool_output_lines: &self.tool_output_lines,
        }
    }

    fn build_tool_segment_lines(
        msg: &DisplayMessage,
        status: ToolStatus,
        rctx: &RenderCtx,
        exp: SectionFlags,
    ) -> ToolLines {
        let mut tl = super::tool_display::build_tool_lines(msg, status, rctx, exp);
        if let Some(header) = tl.lines.first_mut() {
            super::tool_display::append_right_info(
                header,
                None,
                msg.timestamp.as_deref(),
                rctx.width,
            );
        }
        tl
    }

    fn flush_thinking(&mut self) {
        if self.streaming_thinking.is_empty() {
            return;
        }
        let mut msg =
            DisplayMessage::new(DisplayRole::Thinking, self.streaming_thinking.take_all());
        msg.thinking_expanded = mem::take(&mut self.live_thinking_expanded);
        self.messages.push(msg);
    }

    fn try_toggle_live_thinking(&mut self, pos: ScrollPos) -> bool {
        // Thinking is always the first live segment, so it sits right after
        // the spacer that separates it from whatever came before.
        let thinking_seg = self.live_start + usize::from(self.live_start > 0);
        if pos.seg != thinking_seg || self.streaming_thinking.is_empty() {
            return false;
        }
        let drawn_collapsed = self.cache.get(pos.seg).is_some_and(|s| s.truncation.output);
        if !thinking::click_toggles(self.live_thinking_expanded, drawn_collapsed) {
            return false;
        }
        self.live_thinking_expanded = !self.live_thinking_expanded;
        true
    }

    fn try_toggle_cached_thinking(&mut self, seg_idx: usize, width: u16) -> bool {
        let drawn_collapsed = self.cache.get(seg_idx).is_some_and(|s| s.truncation.output);
        let Some(msg) = self
            .cache
            .get(seg_idx)
            .and_then(|seg| seg.msg_index)
            .and_then(|idx| self.messages.get_mut(idx))
            .filter(|m| matches!(m.role, DisplayRole::Thinking))
        else {
            return false;
        };
        if !thinking::click_toggles(msg.thinking_expanded, drawn_collapsed) {
            return false;
        }
        msg.thinking_expanded = !msg.thinking_expanded;
        self.reflow_text_segment(seg_idx, width);
        true
    }

    fn update_spinners(&mut self) {
        let spinner_span = Span::styled(
            spinner_str(self.started_at.elapsed().as_millis()),
            theme::current().spinner,
        );
        for seg in self.cache.segments_mut() {
            seg.update_spinners(&spinner_span);
        }
    }

    fn drain_highlights(&mut self) -> Dirty {
        let mut dirty = Dirty::NO;
        while let Some(result) = self.hl_worker.try_recv() {
            if let Some(seg) = self
                .cache
                .segments_mut()
                .iter_mut()
                .find(|s| s.matches_pending_highlight(result.id))
            {
                seg.apply_highlight_result(result.lines);
                dirty = Dirty::YES;
            }
        }
        dirty
    }

    fn rebuild_tool_segment(&mut self, tool_id: &str) {
        let Some(msg) = self
            .messages
            .iter()
            .rfind(|m| matches!(&m.role, DisplayRole::Tool(t) if t.id == tool_id))
        else {
            return;
        };
        let DisplayRole::Tool(t) = &msg.role else {
            return;
        };
        let status = t.status;
        let Some(seg_idx) = self.cache.find_by_tool_id(tool_id) else {
            return;
        };

        let exp = self
            .expanded_tools
            .get(tool_id)
            .copied()
            .unwrap_or_default();
        let rctx = self.rctx();
        let tl = Self::build_tool_segment_lines(msg, status, &rctx, exp);

        let seg = self.cache.get_mut(seg_idx).unwrap();
        seg.update_with_reuse(tl, &self.hl_worker);
    }

    fn rebuild_line_cache(&mut self) {
        if !self.cache.needs_rebuild(self.messages.len()) {
            return;
        }
        for i in self.cache.msg_count()..self.messages.len() {
            let msg = &self.messages[i];

            if let DisplayRole::Tool(t) = &msg.role {
                let exp = self.expanded_tools.get(&t.id).copied().unwrap_or_default();
                let status = t.status;
                let tl = Self::build_tool_segment_lines(msg, status, &self.rctx(), exp);
                let id = t.id.clone();
                self.cache.push_spacer_if_needed();
                let mut seg = Segment::with_tool(id);
                seg.apply_highlight(tl, &self.hl_worker);
                self.cache.push(seg);
            } else {
                let view = message_view(msg, self.thinking_lines, self.viewport_width);
                self.cache.push_spacer_if_needed();
                let mut seg = Segment::with_lines(view.lines, Some(i));
                seg.gutter = view.gutter;
                seg.truncation.output = view.collapsed;
                self.cache.push(seg);
            }
        }
        self.cache.mark_built(self.messages.len());
    }

    /// Clamps the scroll position against the document end and applies the
    /// bottom pin.
    fn resolve_scroll(&mut self, has_selection: bool) {
        let bottom = self.layout().bottom(self.viewport_height);
        self.scroll = self.layout().clamp(self.scroll.min(bottom));
        if !has_selection {
            if self.scroll >= bottom {
                self.auto_scroll = true;
            }
            if self.auto_scroll {
                self.scroll = bottom;
            }
        }
    }

    /// Re-lays out the stale segments the viewport plus its margin reaches.
    /// The scroll position names a segment, so reflowing cannot slide the
    /// content the reader is looking at; the window only has to cover what is
    /// on screen. Skipping the rest only costs fidelity, see
    /// [`Segment::stale`].
    ///
    /// Heights are counted after each segment is reflowed, so the window is
    /// right the first time.
    fn reflow_viewport(&mut self, width: u16) {
        let viewport = u32::from(self.viewport_height);
        let margin = viewport.saturating_mul(REFLOW_MARGIN_VIEWPORTS);
        // The first visible row sits `scroll.row` rows into the starting
        // segment, so the downward window has to clear those before it starts
        // covering the viewport.
        let below = viewport
            .saturating_add(margin)
            .saturating_add(u32::from(self.scroll.row));
        let start = self.scroll.seg.min(self.cache.len().saturating_sub(1));

        self.reflow_run(start..self.cache.len(), below, width);
        self.reflow_run((0..start).rev(), margin, width);
    }

    fn reflow_run(&mut self, indices: impl Iterator<Item = usize>, row_budget: u32, width: u16) {
        let mut rows = 0;
        for i in indices {
            if rows >= row_budget {
                return;
            }
            rows += self.reflowed_height(i, width);
        }
    }

    /// Reflows `seg_idx` if it is stale, then reports the height it draws at.
    fn reflowed_height(&mut self, seg_idx: usize, width: u16) -> u32 {
        if self.cache.get(seg_idx).is_some_and(|s| s.stale) {
            self.reflow_segment(seg_idx, width);
        }
        self.cache
            .get(seg_idx)
            .map_or(0, |s| s.height(width) as u32)
    }

    fn reflow_segment(&mut self, seg_idx: usize, width: u16) {
        // Clear up front so a reflow that bails early costs one frame of
        // old-width lines instead of retrying forever.
        let Some(seg) = self.cache.get_mut(seg_idx) else {
            return;
        };
        seg.stale = false;
        let Some(tid) = seg.tool_id.clone() else {
            self.reflow_text_segment(seg_idx, width);
            return;
        };
        self.rebuild_tool_segment(&tid);
    }

    fn reflow_text_segment(&mut self, seg_idx: usize, width: u16) {
        let Some(msg_idx) = self.cache.get(seg_idx).and_then(|s| s.msg_index) else {
            return;
        };
        let Some(msg) = self.messages.get(msg_idx) else {
            return;
        };
        let view = message_view(msg, self.thinking_lines, width);
        let Some(seg) = self.cache.get_mut(seg_idx) else {
            return;
        };
        seg.gutter = view.gutter;
        seg.set_lines(view.lines);
        seg.truncation.output = view.collapsed;
    }
}

fn message_style(role: &DisplayRole) -> RoleStyle {
    match role {
        DisplayRole::User => user_style(),
        DisplayRole::Assistant => assistant_style(),
        DisplayRole::Thinking => thinking_style(),
        DisplayRole::Error => error_style(),
        DisplayRole::Done => done_style(),
        DisplayRole::Notice => notice_style(),
        DisplayRole::Tool(_) => unreachable!(),
    }
}

fn message_gutter(style: &RoleStyle) -> Option<Span<'static>> {
    Some(style.gutter())
}

/// Starts with the marker and label the reader sees, so a query for "❯" or
/// "thinking>" lands on them too. A blank marker trims down to nothing, and
/// collapsed thinking draws its header from the same label, so neither needs
/// a case of its own.
fn message_search_text(msg: &DisplayMessage) -> String {
    let style = message_style(&msg.role);
    let marker = if message_gutter(&style).is_some() {
        style.marker.trim_start()
    } else {
        ""
    };
    format!("{marker}{}{}", style.label, msg.text)
}

struct MessageView {
    lines: Vec<Line<'static>>,
    gutter: Option<Span<'static>>,
    /// Thinking drawn collapsed, which is what makes a click on it expand it.
    collapsed: bool,
}

/// A non-Tool message's lines at the body width of `width`, with the gutter
/// they sit behind.
fn message_view(msg: &DisplayMessage, mode: ThinkingLines, width: u16) -> MessageView {
    let style = message_style(&msg.role);
    let gutter = message_gutter(&style);
    let body = if gutter.is_some() {
        body_width(width)
    } else {
        width
    };
    if matches!(msg.role, DisplayRole::Thinking)
        && let Some(lines) = thinking::collapsed_view(
            thinking::tail_rows(mode, msg.thinking_expanded),
            thinking::line_count(&msg.text),
            CLICKABLE,
            |rows| thinking::render_tail(&msg.text, rows, body, style.text_style, None),
        )
    {
        return MessageView {
            lines,
            gutter,
            collapsed: true,
        };
    }
    let lines = build_message_lines(msg, &style, body);
    MessageView {
        lines,
        gutter,
        collapsed: false,
    }
}

/// Builds ratatui lines for a non-Tool message shown in full at the given
/// width. Shared by `rebuild_line_cache` (new messages) and
/// `reflow_text_segment` (stale-on-resize messages) so both paths produce
/// identical segments.
fn build_message_lines(msg: &DisplayMessage, style: &RoleStyle, body: u16) -> Vec<Line<'static>> {
    let prefix = style.label;
    if style.use_markdown {
        text_to_lines(
            &msg.text,
            prefix,
            style.text_style,
            style.label_style(),
            body,
            style.max_line_bytes,
        )
    } else {
        plain_lines(&msg.text, prefix, style.text_style, style.label_style())
    }
}
