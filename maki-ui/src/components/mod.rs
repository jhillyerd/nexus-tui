pub(crate) mod alert_modal;
pub(crate) mod code_view;
pub mod command;
pub(crate) mod help_modal;
pub mod input;
pub mod keybindings;
pub(crate) mod list_picker;
pub mod messages;
pub(crate) mod modal;
pub(crate) mod scrollbar;
pub(crate) mod search_modal;
pub mod status_bar;
pub(crate) mod streaming_content;
pub(crate) mod theme_picker;
pub(crate) mod thinking;
pub(crate) mod tool_display;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::buffer::{Buffer, CellWidth};
use ratatui::layout::Position;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

pub(crate) const CHEVRON: &str = "❯ ";

/// The buffer as text, for tests that assert what was painted.
#[cfg(test)]
pub(crate) fn buffer_text(buf: &ratatui::buffer::Buffer) -> String {
    buf.content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<Vec<_>>()
        .concat()
}

/// A bare key press, for tests.
#[cfg(test)]
pub(crate) fn key(code: crossterm::event::KeyCode) -> crossterm::event::KeyEvent {
    crossterm::event::KeyEvent {
        code,
        modifiers: crossterm::event::KeyModifiers::NONE,
        kind: crossterm::event::KeyEventKind::Press,
        state: crossterm::event::KeyEventState::NONE,
    }
}

pub(crate) fn chevron_span() -> ratatui::text::Span<'static> {
    ratatui::text::Span::styled(CHEVRON, crate::theme::current().tool_dim)
}

/// A field's caret: the span it paints and the display column it should land
/// on, measured from the start of its line.
pub(crate) struct Caret {
    pub col: usize,
    pub span: Span<'static>,
}

/// Cells {spans} take once painted, the way ratatui counts them: it drops
/// every grapheme holding a control char, such as a pasted newline.
pub(crate) fn painted_width(spans: &[Span]) -> usize {
    spans
        .iter()
        .flat_map(|span| span.styled_graphemes(Style::default()))
        .map(|grapheme| usize::from(grapheme.symbol.cell_width()))
        .sum()
}

/// Lays {text} out as a field line after {prefix}, the char at {cursor_byte}
/// in {caret_style} and what follows it in {tail_style}. A caret on a control
/// char or at the end is painted as a space, the only thing that shows.
pub(crate) fn field_line(
    prefix: Vec<Span<'static>>,
    text: &str,
    cursor_byte: usize,
    caret_style: Style,
    tail_style: Style,
) -> (Line<'static>, Caret) {
    let (before, rest) = text.split_at(cursor_byte);
    let mut chars = rest.chars();
    let caret_char = chars.next().filter(|c| !c.is_control()).unwrap_or(' ');
    let mut spans = prefix;
    spans.push(Span::raw(before.to_owned()));
    let col = painted_width(&spans);
    let span = Span::styled(caret_char.to_string(), caret_style);
    spans.push(span.clone());
    spans.push(Span::styled(chars.as_str().to_owned(), tail_style));
    (Line::from(spans), Caret { col, span })
}

/// The cell of {caret} on {row} of {area}, but only if {buf} really holds the
/// caret there. ratatui decides whether text clips, wraps or merges into its
/// neighbour, and a copy of those rules would drift. So the cell is read back,
/// and a mismatch reports nothing rather than a guess. An overlay painted over
/// the field later is not seen.
pub(crate) fn painted_caret(
    buf: &Buffer,
    area: Rect,
    caret: &Caret,
    row: usize,
) -> Option<Position> {
    let x = u16::try_from(usize::from(area.x) + caret.col).ok()?;
    let y = u16::try_from(usize::from(area.y) + row).ok()?;
    let pos = Position::new(x, y);
    let cell = buf.cell(pos).filter(|_| area.contains(pos))?;
    let style = cell.style();
    let painted = caret.span.styled_graphemes(Style::default()).next()?;
    (cell.symbol() == painted.symbol && style.patch(caret.span.style) == style).then_some(pos)
}

/// Paints a one row text field into {area} and returns the cell of its caret.
pub(crate) fn render_field(
    frame: &mut Frame,
    area: Rect,
    prefix: Vec<Span<'static>>,
    text: &str,
    cursor_byte: usize,
    caret_style: Style,
) -> Option<Position> {
    let (line, caret) = field_line(prefix, text, cursor_byte, caret_style, Style::default());
    frame.render_widget(Paragraph::new(line), area);
    painted_caret(frame.buffer_mut(), area, &caret, 0)
}

pub(crate) trait Overlay {
    fn is_open(&self) -> bool;
}

pub(crate) fn hint_line<K: AsRef<str>, V: AsRef<str>>(pairs: &[(K, V)]) -> Line<'static> {
    Line::from(hint_spans(pairs, "  "))
}

/// The same hints, sized for a bottom border: tighter gaps, and a space at
/// each end so they never touch a corner.
pub(crate) fn hint_footer<K: AsRef<str>, V: AsRef<str>>(pairs: &[(K, V)]) -> Line<'static> {
    let mut spans = hint_spans(pairs, " ");
    spans.push(Span::raw(" "));
    Line::from(spans).right_aligned()
}

fn hint_spans<K: AsRef<str>, V: AsRef<str>>(
    pairs: &[(K, V)],
    gap: &'static str,
) -> Vec<Span<'static>> {
    let t = crate::theme::current();
    let mut spans = Vec::with_capacity(pairs.len() * 3 + 1);
    for (key, desc) in pairs {
        spans.push(Span::raw(gap));
        for (i, part) in key.as_ref().split('/').enumerate() {
            if i > 0 {
                spans.push(Span::styled("/", t.tool_dim));
            }
            spans.push(Span::styled(part.to_string(), t.keybind_key));
        }
        spans.push(Span::styled(format!(" {}", desc.as_ref()), t.tool_dim));
    }
    spans
}

pub(crate) fn apply_scroll_delta(offset: u16, delta: i32) -> u16 {
    if delta > 0 {
        offset.saturating_sub(delta as u16)
    } else {
        offset.saturating_add(delta.unsigned_abs() as u16)
    }
}

pub fn is_ctrl(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL) && !key.modifiers.contains(KeyModifiers::ALT)
}

pub(crate) struct ModalScroll {
    offset: u16,
    max_offset: u16,
    viewport_h: u16,
    auto_scroll: bool,
}

impl ModalScroll {
    pub fn new() -> Self {
        Self {
            offset: 0,
            max_offset: 0,
            viewport_h: 0,
            auto_scroll: true,
        }
    }

    pub fn new_top() -> Self {
        Self {
            auto_scroll: false,
            ..Self::new()
        }
    }

    pub fn reset(&mut self) {
        let auto_scroll = self.auto_scroll;
        *self = Self::new();
        self.auto_scroll = auto_scroll;
    }

    pub fn offset(&self) -> u16 {
        self.offset
    }

    pub fn update_dimensions(&mut self, total: u16, viewport_h: u16) {
        self.viewport_h = viewport_h;
        self.max_offset = total.saturating_sub(viewport_h);
        if self.auto_scroll {
            self.offset = self.max_offset;
        } else {
            self.clamp();
            if self.offset >= self.max_offset {
                self.auto_scroll = true;
            }
        }
    }

    pub fn scroll(&mut self, delta: i32) {
        self.offset = apply_scroll_delta(self.offset, delta);
        self.clamp();
        self.auto_scroll = self.offset >= self.max_offset;
    }

    pub fn handle_key(&mut self, key_event: KeyEvent) -> bool {
        use keybindings::key;
        match key_event.code {
            KeyCode::Up => self.scroll(1),
            KeyCode::Down => self.scroll(-1),
            _ if key::SCROLL_HALF_UP.matches(key_event) => self.scroll(self.half_page()),
            _ if key::SCROLL_HALF_DOWN.matches(key_event) => self.scroll(-self.half_page()),
            _ if key::SCROLL_PAGE_UP.matches(key_event) => self.scroll(self.page()),
            _ if key::SCROLL_PAGE_DOWN.matches(key_event) => self.scroll(-self.page()),
            _ if key::SCROLL_LINE_UP.matches(key_event) => self.scroll(1),
            _ if key::SCROLL_LINE_DOWN.matches(key_event) => self.scroll(-1),
            _ if key::SCROLL_TOP.matches(key_event) => {
                self.offset = 0;
                self.auto_scroll = false;
            }
            _ if key::SCROLL_BOTTOM.matches(key_event) => {
                self.auto_scroll = true;
                self.offset = self.max_offset;
            }
            _ => return false,
        }
        true
    }

    fn half_page(&self) -> i32 {
        (self.viewport_h / 2).max(1) as i32
    }

    fn page(&self) -> i32 {
        self.viewport_h.max(1) as i32
    }

    fn clamp(&mut self) {
        self.offset = self.offset.min(self.max_offset);
    }
}

pub enum Action {
    ManualExit,
    ShellCommand {
        id: String,
        command: String,
        visible: bool,
    },
    OpenEditor(PathBuf),
    /// A URL to open in the user's browser.
    OpenUrl(String),
    EditInputInEditor,
    Suspend,
}

const ERROR_DISPLAY: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ExitRequest {
    #[default]
    None,
    Success,
    Error,
}

impl ExitRequest {
    pub fn code(&self) -> i32 {
        match self {
            Self::None | Self::Success => 0,
            Self::Error => 1,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Status {
    Idle,
    Streaming,
    Error { message: String, since: Instant },
}

impl Status {
    pub fn error(message: String) -> Self {
        Self::Error {
            message,
            since: Instant::now(),
        }
    }

    pub fn is_error_expired(&self) -> bool {
        matches!(self, Self::Error { since, .. } if since.elapsed() >= ERROR_DISPLAY)
    }
}

impl PartialEq for Status {
    fn eq(&self, other: &Self) -> bool {
        matches!(
            (self, other),
            (Self::Idle, Self::Idle)
                | (Self::Streaming, Self::Streaming)
                | (Self::Error { .. }, Self::Error { .. })
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    InProgress,
    Success,
    Error,
}

/// What the user sees for one tool call: the role line, its body (the
/// header + output), and the timestamps.
#[derive(Debug, Clone)]
pub struct DisplayMessage {
    pub role: DisplayRole,
    pub text: String,
    pub tool_input: Option<Arc<ToolInput>>,
    pub tool_raw_input: Option<Arc<serde_json::Value>>,
    pub tool_output: Option<Arc<ToolOutput>>,
    pub annotation: Option<String>,
    pub timestamp: Option<String>,
    pub truncated_lines: usize,
    /// Only a click sets it, so thinking starts the way `ui.thinking_lines`
    /// says however the message got here.
    pub thinking_expanded: bool,
}

impl DisplayMessage {
    pub fn new(role: DisplayRole, text: String) -> Self {
        Self {
            role,
            text,
            tool_input: None,
            tool_raw_input: None,
            tool_output: None,
            annotation: None,
            timestamp: None,
            truncated_lines: 0,
            thinking_expanded: false,
        }
    }

    pub fn tool_name(&self) -> Option<&str> {
        self.role.tool_name()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolRole {
    pub id: String,
    pub status: ToolStatus,
    pub name: Arc<str>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DisplayRole {
    User,
    Assistant,
    Thinking,
    Tool(Box<ToolRole>),
    Error,
    Done,
    /// One dim status line: roster changes and other `[system]` rows.
    Notice,
}

impl DisplayRole {
    pub fn tool_name(&self) -> Option<&str> {
        match self {
            DisplayRole::Tool(t) => Some(&t.name),
            _ => None,
        }
    }
}

/// The rendered tool call input: an env tool's arguments, shown as a code
/// block on the chip (the command for `bash`, the path for the file tools).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum ToolInput {
    Code { language: String, code: String },
}

/// A tool result the node renders: the tool plane flattens every result to
/// one text, so chips carry plain or markdown text only.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum ToolOutput {
    Plain(String),
    Markdown(String),
}

impl From<String> for ToolOutput {
    fn from(text: String) -> Self {
        ToolOutput::Plain(text)
    }
}

impl From<&str> for ToolOutput {
    fn from(text: &str) -> Self {
        ToolOutput::Plain(text.to_owned())
    }
}

impl ToolOutput {
    /// Short header suffix summarizing the output, e.g. `3 lines`.
    pub fn annotation(&self) -> Option<String> {
        match self {
            ToolOutput::Plain(text) | ToolOutput::Markdown(text) => {
                let lines = text.lines().count();
                (!text.is_empty()).then(|| format!("{lines} lines"))
            }
        }
    }

    pub fn text(&self) -> &str {
        match self {
            ToolOutput::Plain(text) | ToolOutput::Markdown(text) => text,
        }
    }

    /// Node bytes render inertly: tool output is never interpreted as
    /// markup the terminal would execute.
    pub fn is_markdown(&self) -> bool {
        matches!(self, ToolOutput::Markdown(_))
    }
}
