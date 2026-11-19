mod decor;
mod layout;

use std::borrow::Cow;
use std::cell::{RefCell, RefMut};
use std::time::{SystemTime, UNIX_EPOCH};

use self::decor::{LangCache, LineDecor};
use self::layout::{Layout, Piece, Row};
use crate::text_buffer::{EditResult, Mark, Marks, Prompt, TextBuffer, Writers, is_newline_key};
use crate::theme;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use maki_lua::{InputMarks, InputMode, TextEdit};
use maki_storage::input_history::InputHistory;
use std::mem;

use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};

use super::apply_scroll_delta;
use super::scrollbar::render_vertical_scrollbar;
use crate::selection::ScreenSelection;

const CHEVRON: &str = super::CHEVRON;
const NEWLINE_PAD: &str = "  ";
const PREFIX_WIDTH: u16 = 2;
/// The box's top and bottom border. A box shorter than this plus one row
/// shows the user none of what it holds.
pub(crate) const BORDER_ROWS: u16 = 2;
const PLACEHOLDER_SUGGESTIONS: &[&str] = &[
    "research how something works",
    "fix a bug",
    "add a feature",
    "add a database migration",
    "create a helm chart",
    "simplify some function",
    "remove trivial comments",
    "analyze data",
    "profile and improve performance",
    "add tests",
    "add benchmarks",
    "refactor a module",
    "remove dead code",
];
const ASK_PREFIX: &str = "Ask maki to ";
const ASK_SUFFIX: &str = "...";
const BLANK_PLACEHOLDER: &str = " ";

/// What opens the command palette at the start of [`sent_text`].
pub(crate) const COMMAND_PREFIX: char = '/';

/// The input as Enter reads it, trimmed. The command palette, the send and
/// history all read it so, which keeps a `/yolo ` from going out as a
/// message and coming back from history as a command.
pub(crate) fn sent_text(value: &str) -> &str {
    value.trim()
}

#[derive(Clone, Copy)]
pub enum Placeholder {
    Suggestion,
    Blank,
}

pub enum InputAction {
    /// Enter sends. The input is still there, so whoever sends it can look at
    /// it first.
    Submit,
    ContinueLine,
    Changed,
    Passthrough(KeyEvent),
    None,
}

#[derive(Default)]
#[allow(dead_code)] // the structured submit surface; the box keeps it
pub struct Submission {
    pub text: String,
}

/// What a chat keeps of the input while another chat has the box.
#[allow(dead_code)] // the parked draft one chat may need again
#[derive(Default)]
pub struct Draft {
    pub text: String,
    marks: Marks,
    writers: Writers,
}

impl Draft {
    #[allow(dead_code)]
    pub fn clear_plugin_marks(&mut self, plugin: &str) {
        self.marks.clear_plugin(plugin);
    }

    #[allow(dead_code)]
    pub fn writers(&self) -> &Writers {
        &self.writers
    }
}

pub struct InputBox {
    pub(crate) buffer: TextBuffer,
    history: InputHistory,
    history_index: Option<usize>,
    /// What a history recall covers, marks and all, put back
    /// when the user walks past the newest entry.
    draft: Prompt,
    scroll_y: u16,
    follow_cursor: bool,
    placeholder_hint: &'static str,
    max_input_lines: u16,
    last_total_lines: u16,
    last_content_height: u16,
    /// `height` only gets `&self`, yet it has to bring this cache up to date
    /// too, hence the `RefCell`.
    layout: RefCell<Layout>,
    langs: LangCache,
}

impl InputBox {
    /// In normal mode a key never types or deletes. It still moves the
    /// cursor, walks the history and sends, and `Enter` always sends, since a
    /// `\` before the cursor only means a new line while typing.
    ///
    /// Shift with a motion selects in either mode, since selecting edits
    /// nothing. It runs before the history keys, so Shift+Up on the first
    /// line never swaps out the text being selected. `Esc` with a selection
    /// only drops it, so it never arms the rewind.
    pub fn handle_key(&mut self, key: KeyEvent, mode: InputMode) -> InputAction {
        self.follow_cursor = true;
        let insert = mode == InputMode::Insert;

        if key.modifiers.contains(KeyModifiers::SHIFT)
            && self.buffer.handle_motion_key(key, true) == EditResult::Moved
        {
            return InputAction::None;
        }
        match key.code {
            KeyCode::Esc if self.buffer.deselect() => return InputAction::None,
            KeyCode::Up if self.is_at_first_line() => {
                self.history_up();
                return InputAction::None;
            }
            KeyCode::Down if self.is_at_last_line() => {
                self.history_down();
                return InputAction::None;
            }
            KeyCode::Tab | KeyCode::Esc => return InputAction::Passthrough(key),
            _ if is_newline_key(&key) => {
                if !insert {
                    return InputAction::None;
                }
                self.newline();
                return InputAction::ContinueLine;
            }
            KeyCode::Enter if insert && self.char_before_cursor_is_backslash() => {
                self.continue_line();
                return InputAction::ContinueLine;
            }
            KeyCode::Enter => return InputAction::Submit,
            _ => {}
        }

        let result = if insert {
            self.buffer.handle_key(key)
        } else {
            self.buffer.handle_motion_key(key, false)
        };
        match result {
            EditResult::Changed => InputAction::Changed,
            EditResult::Moved | EditResult::Ignored => InputAction::None,
        }
    }

    pub fn handle_paste(&mut self, text: &str) -> InputAction {
        self.follow_cursor = true;
        self.buffer.insert_text(text);
        InputAction::Changed
    }

    /// Inserting a file path mid-word looks broken ("read/tmp/x" instead of
    /// "read /tmp/x"). This adds spaces around the paste only when needed.
    #[allow(dead_code)]
    pub fn handle_paste_with_spaces(&mut self, text: &str) -> InputAction {
        let value = self.buffer.value();
        let at = self.buffer.cursor_byte();

        let char_before = value[..at].chars().next_back();
        let char_after = value[at..].chars().next();

        let is_word_boundary =
            |c: char| -> bool { c.is_alphanumeric() || c == '_' || ")]}>".contains(c) };

        let needs_leading = char_before.is_some_and(is_word_boundary) && !text.starts_with(' ');
        let needs_trailing = char_after.is_some_and(is_word_boundary) && !text.ends_with(' ');

        if !needs_leading && !needs_trailing {
            return self.handle_paste(text);
        }

        let mut spaced = String::with_capacity(
            text.len() + usize::from(needs_leading) + usize::from(needs_trailing),
        );

        if needs_leading {
            spaced.push(' ');
        }
        spaced.push_str(text);
        if needs_trailing {
            spaced.push(' ');
        }

        self.handle_paste(&spaced)
    }

    /// Rewinds one editing step. The paste a mistimed `Cmd+V` drops in the
    /// middle of a sentence is one step back; a run of typing rewinds word
    /// by word. Cmd+Z is also answered inside the buffer itself, for the
    /// terminals that deliver it; Alt+Z is the portable spelling.
    pub fn undo(&mut self) -> bool {
        self.follow_cursor = true;
        self.buffer.undo()
    }

    pub fn redo(&mut self) -> bool {
        self.follow_cursor = true;
        self.buffer.redo()
    }

    pub fn newline(&mut self) {
        self.follow_cursor = true;
        self.buffer.add_line();
    }

    pub fn new(history: InputHistory, max_input_lines: u32) -> Self {
        let max_input_lines = max_input_lines.clamp(1, u16::MAX as u32 - 2) as u16;
        Self {
            buffer: TextBuffer::new(String::new()),
            history,
            history_index: None,
            draft: Prompt::default(),
            scroll_y: 0,
            follow_cursor: true,
            placeholder_hint: random_placeholder_hint(),
            max_input_lines,
            last_total_lines: 1,
            last_content_height: 1,
            layout: RefCell::default(),
            langs: LangCache::default(),
        }
    }

    pub fn copy_text(&self) -> String {
        self.buffer
            .lines()
            .enumerate()
            .map(|(i, l)| {
                let prefix = if i == 0 { CHEVRON } else { NEWLINE_PAD };
                format!("{prefix}{l}")
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The selected source text, never what a conceal draws over it, and
    /// the selection goes with it.
    pub fn take_selected_text(&mut self) -> Option<String> {
        let selected = self.buffer.selection()?;
        self.buffer.deselect();
        Some(self.buffer.value()[selected].to_owned())
    }

    fn layout(&self, width: u16) -> RefMut<'_, Layout> {
        let mut layout = self.layout.borrow_mut();
        layout.sync(&self.buffer, effective_width(width.into()));
        layout
    }

    /// The source text under a selection dragged over the box at {area},
    /// laid out like [`Self::copy_text`]. Reading the cells instead would
    /// copy virtual text and the text drawn over conceals, which are not
    /// part of the prompt. A conceal or a char cut by either end is taken
    /// whole.
    pub fn selected_text(&self, area: Rect, selection: &ScreenSelection) -> String {
        if selection.covers_rect(area) {
            return self.copy_text();
        }
        let len = self.buffer.value().len();
        let (start_row, start_col) = if selection.start_row < area.y {
            (area.y, area.x)
        } else {
            (selection.start_row, selection.start_col)
        };
        let start_hit = self.hit(area, start_row, start_col);
        let prefix = match &start_hit {
            Some(hit) if matches!(hit.spot, Spot::Prefix) && hit.row.index == 0 => {
                if hit.row.line == 0 {
                    CHEVRON
                } else {
                    NEWLINE_PAD
                }
            }
            _ => "",
        };
        let start = start_hit.map_or(len, |hit| hit.byte(|piece, _| piece.bytes.start));
        let end = self
            .hit(area, selection.end_row, selection.end_col)
            .map_or(len, |hit| {
                hit.byte(|piece, col| {
                    if col < piece.virt {
                        piece.bytes.start
                    } else {
                        piece.bytes.end
                    }
                })
            });
        if start > end {
            return String::new();
        }
        let text = self.buffer.value()[start..end].replace('\n', &format!("\n{NEWLINE_PAD}"));
        format!("{prefix}{text}")
    }

    pub fn height(&self, width: u16) -> u16 {
        let rows = self.layout(width).total_rows();
        let capped = rows.min(self.max_input_lines.into()) as u16;
        capped.saturating_add(BORDER_ROWS)
    }

    pub fn is_at_first_line(&self) -> bool {
        self.buffer.display_lines(self.buffer.y()).start == 0
    }

    pub fn is_at_last_line(&self) -> bool {
        self.buffer.display_lines(self.buffer.y()).end == self.buffer.line_count()
    }

    pub fn char_before_cursor_is_backslash(&self) -> bool {
        self.buffer.value()[..self.buffer.cursor_byte()].ends_with('\\')
    }

    pub fn continue_line(&mut self) {
        self.buffer.undoable(|buf| {
            buf.deselect();
            buf.remove_char();
            buf.add_line();
        });
    }

    /// A recall opens the command palette on a `/` text with nobody holding
    /// it to the plugins that wrote it, and the palette matches loosely, so
    /// a `/yol` a plugin wrote would come back as the user's `/yolo`. Such a
    /// text stays out of history.
    pub fn submit(&mut self) -> Option<Submission> {
        let text = sent_text(self.buffer.value()).to_string();
        if text.is_empty() {
            return None;
        }
        if !(text.starts_with(COMMAND_PREFIX) && !self.buffer.writers().is_empty()) {
            self.history.push(text.clone());
        }
        self.discard();
        Some(Submission { text })
    }

    /// The marks go with the text, so ghost text pushed for it never turns
    /// up in the next prompt.
    pub fn discard(&mut self) {
        self.history_index = None;
        self.draft = Prompt::default();
        self.buffer.clear();
        self.scroll_y = 0;
    }

    pub fn is_empty(&self) -> bool {
        self.buffer.value().trim().is_empty()
    }

    /// The text the user is composing. Under an untouched history recall that
    /// is the draft it covers, since the recall is still in history. An edited
    /// recall is new work and wins over the draft, as it does on submit.
    #[allow(dead_code)]
    pub fn draft_text(&self) -> String {
        if self.is_untouched_recall() {
            self.draft.text.clone()
        } else {
            self.buffer.value().to_owned()
        }
    }

    /// The plugins that wrote [`Self::draft_text`].
    #[allow(dead_code)]
    pub fn draft_writers(&self) -> &Writers {
        if self.is_untouched_recall() {
            &self.draft.writers
        } else {
            self.buffer.writers()
        }
    }

    fn is_untouched_recall(&self) -> bool {
        self.history_index
            .and_then(|i| self.history.get(i))
            .is_some_and(|entry| entry == self.buffer.value())
    }

    /// The marks go with the text they were made for, so under an untouched
    /// history recall the draft it covers is parked with its own marks.
    #[allow(dead_code)]
    pub fn swap_draft(&mut self, draft: Draft) -> Draft {
        let parks_the_draft = self.is_untouched_recall();
        let shown = self.buffer.swap_prompt(Prompt {
            text: draft.text,
            marks: draft.marks,
            writers: draft.writers,
        });
        let covered = mem::take(&mut self.draft);
        self.history_index = None;
        self.scroll_y = 0;
        let Prompt {
            text,
            marks,
            writers,
        } = if parks_the_draft { covered } else { shown };
        Draft {
            text,
            marks,
            writers,
        }
    }

    /// Marks leave the text and its version alone, so the layout would never
    /// notice a line growing or shrinking by its virtual text or conceals. It
    /// is told here. See [`TextBuffer::set_marks`] for {version}.
    pub fn set_marks(
        &mut self,
        version: u64,
        plugin: &str,
        ns: &str,
        marks: Vec<Mark>,
    ) -> Result<(), String> {
        let moved = self.buffer.set_marks(version, plugin, ns, marks)?;
        self.layout.get_mut().relayout(&self.buffer, &moved);
        Ok(())
    }

    pub fn clear_plugin_marks(&mut self, plugin: &str) {
        let moved = self.buffer.clear_plugin_marks(plugin);
        self.layout.get_mut().relayout(&self.buffer, &moved);
        self.draft.marks.clear_plugin(plugin);
    }

    /// Takes in what the worker finished for this input, and recounts the
    /// lines whose marks it changed. `true` when the frame has to show it.
    pub fn poll_decor(&mut self) -> bool {
        let syntax = self.buffer.poll_syntax();
        if let Some(changed) = &syntax {
            self.layout.get_mut().relayout(&self.buffer, changed);
        }
        self.langs.poll() | syntax.is_some()
    }

    /// The way a plugin writes to the input. It carries the box's own state
    /// the way the paste path does: the view follows the cursor again, and a
    /// value the edits changed stops counting as a recalled history entry, so
    /// the next history key does not throw the edit away. A call that changes
    /// no text leaves the recall and the draft under it alone.
    ///
    /// See [`TextBuffer::apply_edits`] for what the offsets refuse.
    pub fn apply_edits(
        &mut self,
        version: u64,
        edits: Vec<TextEdit>,
        cursor: Option<usize>,
        anchor: Option<usize>,
        marks: Option<(&str, InputMarks)>,
    ) -> Result<usize, String> {
        let before = self.buffer.version();
        let (caret, moved) = self
            .buffer
            .apply_edits(version, edits, cursor, anchor, marks)?;
        self.layout.get_mut().relayout(&self.buffer, &moved);
        self.follow_cursor = true;
        if self.buffer.version() != before {
            self.history_index = None;
            self.draft = Prompt::default();
        }
        Ok(caret)
    }

    /// A whole prompt no plugin wrote: a restored draft, a rewind prompt. The
    /// text lands verbatim, because the user composed it elsewhere and it is
    /// what the model would be sent. It is another prompt even when it reads
    /// the same, so the marks of the old one go, see
    /// [`TextBuffer::swap_prompt`].
    pub fn set_input(&mut self, s: &str) {
        self.buffer.set_value(s);
    }

    /// [`Self::set_input`] for text made from what the input holds: a
    /// completed command, what `$EDITOR` changed. The plugins that wrote the
    /// text it came from still count as its writers.
#[allow(dead_code)] // the editor bridge restores this path
    pub fn rewrite_input(&mut self, s: &str) {
        let writers = self.buffer.writers().clone();
        self.buffer.swap_prompt(Prompt {
            text: s.to_owned(),
            writers,
            ..Prompt::default()
        });
    }

    /// `false` when there is no older entry to recall.
    pub fn history_up(&mut self) -> bool {
        self.follow_cursor = true;
        let Some(Some(new_index)) = self.recall_target(true) else {
            return false;
        };
        let entry = self.history_entry(new_index);
        let shown = self.buffer.swap_prompt(entry);
        if self.history_index.replace(new_index).is_none() {
            self.draft = shown;
        }
        self.buffer.move_to_end();
        true
    }

    /// History keeps text only, so a recalled entry comes in with no marks,
    /// and as the user's, who sent it. It holds only what went out, in the
    /// form Enter read it, so it reads the same way again, see
    /// [`sent_text`].
    fn history_entry(&self, index: usize) -> Prompt {
        Prompt {
            text: self.history.get(index).unwrap().to_owned(),
            ..Prompt::default()
        }
    }

    /// `false` when no entry is recalled, so there is nothing newer to go to.
    pub fn history_down(&mut self) -> bool {
        self.follow_cursor = true;
        let Some(target) = self.recall_target(false) else {
            return false;
        };
        self.history_index = target;
        let next = match target {
            Some(index) => self.history_entry(index),
            None => mem::take(&mut self.draft),
        };
        self.buffer.swap_prompt(next);
        true
    }

    /// Where a history key goes from here, older or newer: `Some` entry, or
    /// `None` for the draft a recall covers. The outer `None` when it goes
    /// nowhere.
    fn recall_target(&self, older: bool) -> Option<Option<usize>> {
        match (older, self.history_index) {
            (true, None) => self.history.len().checked_sub(1).map(Some),
            (true, Some(index)) => index.checked_sub(1).map(Some),
            (false, None) => None,
            (false, Some(index)) => Some(Some(index + 1).filter(|&next| next < self.history.len())),
        }
    }

    /// Returns the screen cell the caret is on. The terminal draws the cursor
    /// there, so nothing is painted for it here.
    pub fn view(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        placeholder: Placeholder,
        border_style: Style,
        top_right_hint: Option<Line<'_>>,
    ) -> Option<Position> {
        let content_height = area.height.saturating_sub(BORDER_ROWS);
        let layout = self.layout.get_mut();
        layout.sync(&self.buffer, effective_width(area.width.into()));
        let cursor_row = layout.cursor_row(&self.buffer);
        let text_rows = layout.total_rows();

        if self.follow_cursor {
            let cursor_row = u16::try_from(cursor_row).unwrap_or(u16::MAX);
            if cursor_row < self.scroll_y {
                self.scroll_y = cursor_row;
            } else if cursor_row >= self.scroll_y.saturating_add(content_height) {
                self.scroll_y = cursor_row.saturating_sub(content_height.saturating_sub(1));
            }
        }

        let total_vl = u16::try_from(text_rows).unwrap_or(u16::MAX);
        self.last_total_lines = total_vl;
        self.last_content_height = content_height.max(1);
        let max_scroll = self.max_scroll();
        self.scroll_y = self.scroll_y.min(max_scroll);
        let is_empty = self.buffer.is_empty();
        let mut cursor_cell: Option<Position> = None;
        let styled_lines: Vec<Line> = if is_empty {
            let base = theme::current().input_placeholder;
            let (head, tail): (Cow<str>, _) = match placeholder {
                Placeholder::Suggestion => (
                    ASK_PREFIX.into(),
                    vec![
                        Span::styled(self.placeholder_hint, base.add_modifier(Modifier::ITALIC)),
                        Span::styled(ASK_SUFFIX, base),
                    ],
                ),

                Placeholder::Blank => (BLANK_PLACEHOLDER.into(), Vec::new()),
            };
            let mut spans = vec![super::chevron_span()];
            cursor_cell = Some(Position::new(display_width(&spans), 0));
            spans.push(Span::styled(head, base));
            spans.extend(tail);
            vec![Line::from(spans)]
        } else {
            let rows: Vec<Row> = self
                .layout
                .get_mut()
                .rows(&self.buffer, self.scroll_y.into(), content_height.into())
                .collect();
            let window = match (rows.first(), rows.last()) {
                (Some(first), Some(last)) => first.text.start..=last.text.end,
                _ => 0..=0,
            };
            let marks = self.buffer.overlapping(window.clone());
            let conceals = self.buffer.concealed(window);
            self.langs.next_frame(&self.buffer);
            let caret = self.buffer.cursor_byte();
            let selected = self.buffer.selection();
            let mut lines = Vec::with_capacity(rows.len() + 1);
            for line_rows in rows.chunk_by(|a, b| a.line == b.line) {
                let text = line_rows[0].text.clone();
                let line = &self.buffer.value()[text.clone()];
                let decor = LineDecor::new(
                    self.buffer.value(),
                    text.clone(),
                    &marks,
                    &conceals,
                    selected.as_ref(),
                    &mut self.langs,
                );
                for row in line_rows {
                    let screen_row = lines.len() as u16;
                    let owns_caret = u32::from(self.scroll_y) + u32::from(screen_row) == cursor_row;
                    let caret = owns_caret.then(|| caret - text.start);
                    let (drawn, col) = draw_row(line, row, &decor, caret);
                    if let Some(col) = col {
                        cursor_cell = Some(Position::new(col, screen_row));
                    }
                    lines.push(drawn);
                }
            }
            lines
        };

        let text = Text::from(styled_lines);
        let mut block = Block::default()
            .borders(Borders::TOP | Borders::BOTTOM)
            .border_type(BorderType::Plain)
            .border_style(border_style);
        if let Some(hint) = top_right_hint {
            block = block.title_top(hint.right_aligned());
        }
        let paragraph = Paragraph::new(text)
            .style(Style::new().fg(theme::current().foreground))
            .block(block);
        frame.render_widget(paragraph, area);

        if max_scroll > 0 {
            let inner = area.inner(ratatui::layout::Margin::new(0, 1));
            render_vertical_scrollbar(frame, inner, u32::from(total_vl), u32::from(self.scroll_y));
        }

        let cell = cursor_cell?;
        (cell.y < content_height && cell.x < area.width)
            .then(|| Position::new(area.x + cell.x, area.y + 1 + cell.y))
    }

    fn max_scroll(&self) -> u16 {
        self.last_total_lines
            .saturating_sub(self.last_content_height)
    }

    pub fn scroll_y(&self) -> u16 {
        self.scroll_y
    }

    #[allow(dead_code)]
    pub fn history(&self) -> &InputHistory {
        &self.history
    }

    #[allow(dead_code)]
    pub fn set_history(&mut self, history: InputHistory) {
        self.history = history;
        self.history_index = None;
    }

    pub fn scroll(&mut self, delta: i32) {
        self.scroll_y = apply_scroll_delta(self.scroll_y, delta).min(self.max_scroll());
        self.follow_cursor = false;
    }

    /// Move the text cursor to the position corresponding to a mouse click at
    /// the terminal coordinates (row, col) within the input content area.
    /// A drag from there is the screen's copy selection, never the input's,
    /// so the click drops the input's selection like any plain motion.
    pub fn handle_click(&mut self, area: Rect, row: u16, col: u16) {
        let Some((y, x)) = self.click_position(area, row, col) else {
            return;
        };
        self.buffer.set_cursor(y, x);
        self.buffer.deselect();
        self.follow_cursor = true;
    }

    /// Convert a mouse click at terminal (row, col) within the input content
    /// area into a (line_index, char_index) in the text buffer, accounting
    /// for scroll offset, word-wrap, and the chevron/padding prefix.
    ///
    /// A click on virtual text lands on the char it sits before, and one on
    /// the text drawn over a conceal on the nearer edge of the conceal.
    fn click_position(&self, area: Rect, row: u16, col: u16) -> Option<(usize, usize)> {
        let hit = self.hit(area, row, col)?;
        let at = hit.byte(|piece, col| {
            if piece.concealed && col >= piece.virt && (col - piece.virt) * 2 >= piece.width {
                piece.bytes.end
            } else {
                piece.bytes.start
            }
        });
        let y = self.buffer.line_of(at);
        let line_start = self.buffer.line_range(y).start;
        Some((y, self.buffer.value()[line_start..at].chars().count()))
    }

    /// The drawn row under terminal cell (row, col) of the content area, and
    /// what of it the cell shows.
    fn hit(&self, area: Rect, row: u16, col: u16) -> Option<Hit> {
        let content_y = row.checked_sub(area.y)?;
        let content_x = col.checked_sub(area.x)?;
        let row = self
            .layout(area.width)
            .rows(
                &self.buffer,
                u32::from(self.scroll_y) + u32::from(content_y),
                1,
            )
            .next()?;
        let prefix = if row.index == 0 { PREFIX_WIDTH } else { 0 };
        let Some(col) = content_x.checked_sub(prefix) else {
            return Some(Hit {
                row,
                spot: Spot::Prefix,
            });
        };
        let mut col = usize::from(col);
        let line = &self.buffer.value()[row.text.clone()];
        let mut spot = Spot::PastEnd;
        for found in row.shape.pieces(line, row.bytes.clone()) {
            let width = found.virt + found.width;
            if width > col {
                spot = Spot::Piece(found, col);
                break;
            }
            col -= width;
        }
        Some(Hit { row, spot })
    }
}

/// A screen cell of the box: the row it is on and what of the row it shows.
struct Hit {
    row: Row,
    spot: Spot,
}

enum Spot {
    /// The chevron or the pad before the text.
    Prefix,
    /// A piece of the row, and how many columns into it.
    Piece(Piece, usize),
    PastEnd,
}

impl Hit {
    /// The byte of the value it lands on: the row's start in the prefix, its
    /// end past it, and inside a piece what {pick} chooses.
    fn byte(&self, pick: impl FnOnce(&Piece, usize) -> usize) -> usize {
        let at = match &self.spot {
            Spot::Prefix => self.row.bytes.start,
            Spot::Piece(piece, col) => pick(piece, *col),
            Spot::PastEnd => self.row.bytes.end,
        };
        self.row.text.start + at
    }
}

fn random_placeholder_hint() -> &'static str {
    let idx = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as usize % PLACEHOLDER_SUGGESTIONS.len())
        .unwrap_or(0);
    PLACEHOLDER_SUGGESTIONS[idx]
}

const fn effective_width(content_width: usize) -> usize {
    content_width.saturating_sub(PREFIX_WIDTH as usize)
}

/// Draws one row of {line}. When the row owns the caret, {caret} is its byte in
/// the line and the column it lands in comes back too. Only this function
/// knows where the caret was drawn, so a terminal cursor placed from that
/// column can never drift off the cell.
fn draw_row(
    line: &str,
    row: &Row,
    decor: &LineDecor,
    caret: Option<usize>,
) -> (Line<'static>, Option<u16>) {
    let mut spans = match row.index {
        0 if row.line == 0 => vec![super::chevron_span()],
        0 => vec![Span::raw(NEWLINE_PAD)],
        _ => Vec::new(),
    };
    let prefix_width = display_width(&spans);
    let (content, caret_chars) = decor.draw(line, row, caret);
    let Some(caret_chars) = caret_chars else {
        spans.extend(content);
        return (Line::from(spans), None);
    };
    let col = caret_column(&content, caret_chars);
    spans.extend(content);
    (Line::from(spans), Some(prefix_width + col))
}

fn display_width(spans: &[Span<'_>]) -> u16 {
    spans.iter().map(Span::width).sum::<usize>() as u16
}

/// The display column of the {caret_chars}th char of {spans}, measured from
/// the spans actually emitted so wide chars cannot throw it off.
fn caret_column(spans: &[Span<'_>], caret_chars: usize) -> u16 {
    let mut left = caret_chars;
    let mut col = 0;
    for span in spans {
        let len = span.content.chars().count();
        if left < len {
            let byte = TextBuffer::char_to_byte(&span.content, left);
            return col + Span::raw(&span.content[..byte]).width() as u16;
        }
        col += span.width() as u16;
        left -= len;
    }
    col
}

#[cfg(test)]
mod tests {
    #[allow(dead_code)]
    struct Rendered {
        terminal: ratatui::Terminal<ratatui::backend::TestBackend>,
        cursor: Option<Position>,
    }

    fn shift(code: KeyCode) -> crossterm::event::KeyEvent {
        crossterm::event::KeyEvent {
            code,
            modifiers: crossterm::event::KeyModifiers::SHIFT,
            kind: crossterm::event::KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        }
    }

    use super::*;
    use crate::components::scrollbar::SCROLLBAR_THUMB;
    use crate::text_buffer::test_support::text_edit;
    use ratatui::layout::{Position, Rect};
    use test_case::test_case;

    fn type_text(input: &mut InputBox, text: &str) {
        for c in text.chars() {
            input.buffer.push_char(c);
        }
    }

    fn submit_text(input: &mut InputBox, text: &str) {
        type_text(input, text);
        input.submit();
    }

    #[test]
    fn submit() {
        let mut input = InputBox::new(InputHistory::default(), 20);
        assert!(input.submit().is_none());

        type_text(&mut input, " ");
        assert!(input.submit().is_none());

        type_text(&mut input, " x ");
        let sub = input.submit().unwrap();
        assert_eq!(sub.text, "x");
        assert_eq!(input.buffer.value(), "");

        type_text(&mut input, "line1");
        input.buffer.add_line();
        type_text(&mut input, "line2");
        assert_eq!(input.submit().unwrap().text, "line1\nline2");
    }

    #[test_case("hello\\",  0, false => "hello\n"  ; "at_the_end")]
    #[test_case("asd\\asd", 3, false => "asd\nasd" ; "mid_line")]
    #[test_case("hello\\",  0, true  => "hello\n"  ; "over_a_selection")]
    fn backslash_continuation(text: &str, left: usize, select_line: bool) -> String {
        let mut input = InputBox::new(InputHistory::default(), 20);
        type_text(&mut input, text);
        for _ in 0..left {
            input.buffer.move_left();
        }
        if select_line {
            input.handle_key(KeyCode::Home.into(), InputMode::Insert);
            input.handle_key(shift(KeyCode::End), InputMode::Insert);
        }
        assert!(input.char_before_cursor_is_backslash());
        input.handle_key(KeyCode::Enter.into(), InputMode::Insert);
        input.buffer.value().to_owned()
    }

    const TEST_WIDTH: u16 = 80;

    #[test]
    fn height_capped_at_max() {
        let mut input = InputBox::new(InputHistory::default(), 20);
        let base = input.height(TEST_WIDTH);
        for _ in 0..20 {
            input.buffer.add_line();
        }
        assert!(input.height(TEST_WIDTH) > base);
        assert!(input.height(TEST_WIDTH) <= 20 + 2);
    }

    #[test]
    fn height_respects_configured_max() {
        let mut input = InputBox::new(InputHistory::default(), 3);
        for _ in 0..10 {
            input.buffer.add_line();
        }
        assert_eq!(input.height(TEST_WIDTH), 3 + 2);
    }

    #[test]
    fn first_last_line() {
        let mut input = InputBox::new(InputHistory::default(), 20);
        assert!(input.is_at_first_line());
        assert!(input.is_at_last_line());

        input.buffer.add_line();
        assert!(!input.is_at_first_line());
        assert!(input.is_at_last_line());

        input.buffer.move_up();
        assert!(input.is_at_first_line());
        assert!(!input.is_at_last_line());
    }

    #[test]
    fn history() {
        let mut input = InputBox::new(InputHistory::default(), 20);

        input.history_up();
        input.history_down();
        assert_eq!(input.buffer.value(), "");

        submit_text(&mut input, "a");
        submit_text(&mut input, "b");
        type_text(&mut input, "draft");

        input.history_up();
        assert_eq!(input.buffer.value(), "b");
        input.history_up();
        assert_eq!(input.buffer.value(), "a");
        input.history_up();
        assert_eq!(input.buffer.value(), "a");

        input.history_down();
        assert_eq!(input.buffer.value(), "b");
        input.history_down();
        assert_eq!(input.buffer.value(), "draft");

        input.buffer.clear();
        type_text(&mut input, "line1");
        input.buffer.add_line();
        type_text(&mut input, "line2");
        assert!(input.is_at_last_line());
        input.history_up();
        input.history_down();
        assert_eq!(input.buffer.value(), "line1\nline2");
        assert!(input.is_at_first_line());

        input.submit();
        input.history_up();
        assert_eq!(input.buffer.value(), "line1\nline2");
        assert!(input.is_at_last_line());

        input.history_down();
        assert_eq!(input.buffer.value(), "");

        input.set_input("alpha\nbeta");
        input.submit();
        input.set_input("gamma\ndelta");
        input.submit();

        input.history_up();
        input.history_up();
        assert_eq!(input.buffer.value(), "alpha\nbeta");
        assert!(input.is_at_last_line());

        input.history_down();
        assert_eq!(input.buffer.value(), "gamma\ndelta");
        assert!(input.is_at_first_line());

        input.history_down();
        assert_eq!(input.buffer.value(), "");
    }

    const DRAFT: &str = "draft";
    const RECALLED: &str = "recalled";
    const EDITED_RECALL: &str = "edited";

    #[test_case(None, DRAFT ; "untouched_recall_keeps_the_draft")]
    #[test_case(Some(EDITED_RECALL), EDITED_RECALL ; "edited_recall_keeps_the_edit")]
    fn draft_while_browsing_history(edit: Option<&str>, parked: &str) {
        let mut input = InputBox::new(InputHistory::default(), 20);
        submit_text(&mut input, RECALLED);
        type_text(&mut input, DRAFT);
        input.history_up();
        if let Some(edit) = edit {
            input.buffer.clear();
            type_text(&mut input, edit);
        }
        assert_eq!(input.draft_text(), parked);

        let swapped = input.swap_draft(Draft::default());

        assert_eq!(swapped.text, parked);
    }

    #[test_case(Vec::new(), Some(1), DRAFT ; "a_cursor_move_keeps_the_recall")]
    #[test_case(vec![text_edit(0, RECALLED.len(), EDITED_RECALL)], None, EDITED_RECALL ; "an_edit_is_new_work")]
    fn plugin_write_over_a_history_recall(
        edits: Vec<TextEdit>,
        cursor: Option<usize>,
        parked: &str,
    ) {
        let mut input = InputBox::new(InputHistory::default(), 20);
        submit_text(&mut input, RECALLED);
        type_text(&mut input, DRAFT);
        input.history_up();

        let version = input.buffer.version();
        input
            .apply_edits(version, edits, cursor, None, None)
            .unwrap();

        assert_eq!(input.draft_text(), parked);
    }

    #[test]
    fn cursor_adds_extra_wrap_row_at_boundary() {
        let width: u16 = 12;
        let ew = effective_width(width as usize);

        let mut at_boundary = InputBox::new(InputHistory::default(), 20);
        type_text(&mut at_boundary, &"x".repeat(ew));

        let mut before_boundary = InputBox::new(InputHistory::default(), 20);
        type_text(&mut before_boundary, &"x".repeat(ew - 1));

        assert_eq!(
            at_boundary.height(width),
            before_boundary.height(width) + 1,
            "cursor at boundary should cause one extra visual line"
        );
    }

    fn draw_input(
        input: &mut InputBox,
        width: u16,
        height: u16,
        placeholder: Placeholder,
    ) -> Rendered {
        let border_style = Style::new().fg(theme::current().mode_build);
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut cursor = None;
        terminal
            .draw(|frame| {
                let area = Rect::new(0, 0, width, height);
                cursor = input.view(frame, area, placeholder, border_style, None);
            })
            .unwrap();
        Rendered { terminal, cursor }
    }

    fn render_input_with(
        input: &mut InputBox,
        width: u16,
        height: u16,
        placeholder: Placeholder,
    ) -> ratatui::Terminal<ratatui::backend::TestBackend> {
        draw_input(input, width, height, placeholder).terminal
    }

    fn render_input(
        input: &mut InputBox,
        width: u16,
        height: u16,
    ) -> ratatui::Terminal<ratatui::backend::TestBackend> {
        render_input_with(input, width, height, Placeholder::Suggestion)
    }

    fn has_scrollbar_thumb(terminal: &ratatui::Terminal<ratatui::backend::TestBackend>) -> bool {
        let buf = terminal.backend().buffer();
        (0..buf.area.height).any(|y| {
            buf.cell((buf.area.width - 1, y))
                .is_some_and(|c| c.symbol() == SCROLLBAR_THUMB)
        })
    }

    #[test_case(20, true  ; "visible_when_content_overflows")]
    #[test_case(0,  false ; "hidden_when_content_fits")]
    fn scrollbar_visibility(extra_lines: usize, expect_visible: bool) {
        let mut input = InputBox::new(InputHistory::default(), 20);
        for _ in 0..extra_lines {
            input.buffer.add_line();
        }
        let terminal = render_input(&mut input, 40, 20 + 2);
        assert_eq!(has_scrollbar_thumb(&terminal), expect_visible);
    }

    #[test]
    fn scroll_clamped_on_content_shrink() {
        let mut input = InputBox::new(InputHistory::default(), 20);
        for _ in 0..20 {
            input.buffer.add_line();
        }
        let area_height = 5_u16;
        let _ = render_input(&mut input, 40, area_height);
        let scroll_before = input.scroll_y;
        assert!(scroll_before > 0);

        input.set_input("short");
        let _ = render_input(&mut input, 40, area_height);
        assert_eq!(input.scroll_y, 0);
    }

    /// Past `u16::MAX` rows, the scroll and the height of a box that grew
    /// under a view scrolled to the end no longer add up in a `u16`.
    #[test]
    fn a_tall_input_scrolled_to_its_end_renders_as_the_box_grows() {
        const GROWN_ROWS: u16 = 20;
        let mut input = InputBox::new(InputHistory::default(), u32::MAX);
        input.set_input(&"\n".repeat(usize::from(u16::MAX) + 1));
        input.buffer.move_to_end();
        let _ = render_input(&mut input, 40, BORDER_ROWS + 1);
        let _ = render_input(&mut input, 40, BORDER_ROWS + GROWN_ROWS);
    }

    #[test]
    fn multibyte_input_renders_without_panic() {
        let mut input = InputBox::new(InputHistory::default(), 20);
        type_text(&mut input, "● grep> hello");
        input.buffer.move_home();
        input.buffer.move_right();
        input.buffer.move_right();
        let _ = render_input(&mut input, 40, 5);
    }

    #[test_case("●\\", true  ; "after_multibyte")]
    #[test_case("●", false   ; "inside_multibyte_would_be_false")]
    fn char_before_cursor_backslash(input: &str, expected: bool) {
        let mut input_box = InputBox::new(InputHistory::default(), 20);
        type_text(&mut input_box, input);
        assert_eq!(input_box.char_before_cursor_is_backslash(), expected);
    }

    fn rendered_row(
        terminal: &ratatui::Terminal<ratatui::backend::TestBackend>,
        row: u16,
    ) -> String {
        let buf = terminal.backend().buffer();
        (0..buf.area.width)
            .map(|col| buf.cell((col, row)).unwrap().symbol().to_string())
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    #[test]
    fn prefix_on_single_line() {
        let mut input = InputBox::new(InputHistory::default(), 20);
        type_text(&mut input, "hello");
        let terminal = render_input(&mut input, 20, 4);
        let row = rendered_row(&terminal, 1);
        assert!(row.starts_with(CHEVRON), "row: {row:?}");
        assert!(row.contains("hello"));
    }

    #[test]
    fn prefix_on_multiline() {
        let mut input = InputBox::new(InputHistory::default(), 20);
        type_text(&mut input, "aaa");
        input.buffer.add_line();
        type_text(&mut input, "bbb");
        let terminal = render_input(&mut input, 20, 5);
        let row0 = rendered_row(&terminal, 1);
        let row1 = rendered_row(&terminal, 2);
        assert!(row0.starts_with(CHEVRON), "row0: {row0:?}");
        assert!(row1.starts_with(NEWLINE_PAD), "row1: {row1:?}");
    }

    #[test]
    fn wrapped_line_gets_no_padding() {
        let mut input = InputBox::new(InputHistory::default(), 20);
        let ew = effective_width(14);
        type_text(&mut input, &"x".repeat(ew + 3));
        let terminal = render_input(&mut input, 14, 5);
        let row0 = rendered_row(&terminal, 1);
        let row1 = rendered_row(&terminal, 2);
        assert!(row0.starts_with(CHEVRON), "row0: {row0:?}");
        assert!(
            !row1.starts_with(CHEVRON) && !row1.starts_with(NEWLINE_PAD),
            "wrapped row should have no padding: {row1:?}"
        );
        assert!(
            row1.starts_with("x"),
            "wrapped row should start with content: {row1:?}"
        );
    }

    #[test]
    fn copy_text_includes_prefix() {
        let input = InputBox::new(InputHistory::default(), 20);
        assert_eq!(input.copy_text(), CHEVRON);

        let mut input = InputBox::new(InputHistory::default(), 20);
        type_text(&mut input, "line1");
        input.buffer.add_line();
        type_text(&mut input, "line2");
        assert_eq!(input.copy_text(), "❯ line1\n  line2");
    }

    /// Eleven double width chars take three rows of an eleven column line, one
    /// more than dividing width by columns gives, because the char that no
    /// longer fits leaves a gap behind.
    #[test]
    fn a_partial_selection_of_a_wide_char_input_breaks_where_the_box_wrapped() {
        const WIDTH: u16 = 13;
        const HEIGHT: u16 = 8;
        const WIDE_LINE: &str = "一二三四五六七八九十百";
        const TAIL: &str = "tail";

        let mut input = InputBox::new(InputHistory::default(), 20);
        type_text(&mut input, WIDE_LINE);
        input.buffer.add_line();
        type_text(&mut input, TAIL);

        // Leaving the first drawn row out keeps the box partly selected, so
        // the copy maps the cells back to the text.
        let selection = ScreenSelection {
            start_row: 2,
            start_col: 0,
            end_row: 4,
            end_col: WIDTH - 1,
        };

        let text = input.selected_text(Rect::new(0, 1, WIDTH, HEIGHT - 2), &selection);
        assert_eq!(text, format!("六七八九十百\n{NEWLINE_PAD}{TAIL}"));
    }

    #[test_case(Placeholder::Blank, "" ; "blank_shows_only_the_chevron")]
    fn placeholder_row(placeholder: Placeholder, expected: &str) {
        let mut input = InputBox::new(InputHistory::default(), 20);
        let terminal = render_input_with(&mut input, 40, 4, placeholder);
        assert_eq!(
            rendered_row(&terminal, 1),
            format!("{CHEVRON}{expected}").trim_end()
        );
    }
}
