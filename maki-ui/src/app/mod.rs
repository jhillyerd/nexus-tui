//! The app: one nexus thread, one transcript, one input box. An elm-ish
//! update core — `Msg` in, `Action`s out — fed by the event loop.

pub(crate) mod commands;
pub(crate) mod mouse;
pub(crate) mod node_events;
pub(crate) mod shell;
pub(crate) mod typeahead;
mod view;

use std::mem;

use crate::repaint::Watch;
use crossterm::event::{KeyEvent, MouseEvent};
use maki_config::UiConfig;
use maki_lua::{EventHandle, HintReader, HintSnapshot, InputMode, InputRulesReader, KeymapReader};
use maki_storage::StateDir;
use maki_storage::input_history::InputHistory;
use ratatui::layout::Rect;

use crate::chat::Chat;
use crate::clipboard::ClipboardState;
use crate::components::alert_modal::AlertModal;
use crate::components::command::CommandPalette;
use crate::components::help_modal::HelpModal;
use crate::components::input::{InputAction, InputBox};
use crate::components::keybindings::key;
use crate::components::search_modal::{SearchAction, SearchModal};
use crate::components::status_bar::{Link, StatusBar};
use crate::components::theme_picker::{ThemePicker, ThemePickerAction};
use crate::components::{Action, ExitRequest, Overlay, Status};
use crate::node::NodeEvent;
use crate::node::NodeHandle;
use crate::selection::{SelectionState, ZoneRegistry};
use crate::theme;

use self::typeahead::{Sight, Turn};

const INPUT_NOT_LIVE_ERR: &str = "the input box is not on screen";
const WRITER_ANY: &str = "plugin";

/// The fields [`maki_lua::InputDecorate`] carries that we act on, without
/// naming its plugin owner (the plugin is inert here; marks are its legacy).
fn decorate_marks(decorate: maki_lua::InputDecorate) -> (u64, maki_lua::InputMarks) {
    (decorate.version, decorate.marks)
}
const OFFLINE_SEND_HINT: &str = "offline — message kept in the input";
const NO_STOP_HINT: &str = "a node cannot stop a turn; stop it from the web UI";

pub enum Msg {
    Key(KeyEvent),
    Paste(String),
    Mouse(MouseEvent),
    Scroll { column: u16, row: u16, delta: i32 },
    Node(NodeEvent),
}

pub struct App {
    pub(crate) chat: Chat,
    pub(crate) input_box: InputBox,
    pub(crate) command_palette: CommandPalette,
    pub(super) theme_picker: ThemePicker,
    pub(super) alert_modal: AlertModal,
    pub(crate) help_modal: HelpModal,
    pub(super) search_modal: SearchModal,
    pub(crate) status_bar: StatusBar,
    pub status: Status,
    pub exit_request: ExitRequest,
    pub(crate) zones: ZoneRegistry,
    pub(super) selection_state: Option<SelectionState>,
    pub(crate) clipboard: ClipboardState,
    /// The last key was a `Ctrl+C` that copied, see [`Self::handle_ctrl`].
    quit_copied: bool,
    /// The node connection and what it told us last.
    pub(crate) node: NodeHandle,
    pub(crate) link: Link,
    pub(crate) model: String,
    pub(crate) loop_status: Option<String>,
    pub(crate) attached: Option<String>,
    pub(crate) ui_config: UiConfig,
    pub(crate) lua_event_handle: EventHandle,
    /// The model name lua was last told about, so `hello` fires the
    /// autocmd once.
    announced_model: Option<String>,
    input_fired_this_frame: bool,
    pub(super) keymap_reader: KeymapReader,
    pub(super) hint_reader: HintReader,
    hints: Watch<HintSnapshot>,
    pub(super) input_rules_reader: InputRulesReader,
    input_rules: Watch<Vec<maki_lua::InputRule>>,
    /// Actions produced outside key handling, drained by the event loop.
    pub(super) pending_actions: Vec<Action>,
    typeahead: typeahead::Typeahead,
    /// Middle-click/right-click pastes, read off the UI thread.
    pub(crate) mouse_paste_rx: Vec<tokio::sync::oneshot::Receiver<Option<String>>>,
}

impl App {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        session_id: maki_storage::id::MakiId,
        node: NodeHandle,
        storage: StateDir,
        ui_config: UiConfig,
        keymap_reader: KeymapReader,
        hint_reader: HintReader,
        input_rules_reader: InputRulesReader,
        lua_event_handle: EventHandle,
        commands: CommandPalette,
    ) -> Self {
        const INPUT_HISTORY_SIZE: usize = 200;
        let input_history_size = INPUT_HISTORY_SIZE;
        let _ = &ui_config;
        let mut input_box = InputBox::new(
            InputHistory::load(
                &storage,
                &std::env::current_dir().unwrap_or_default(),
                input_history_size,
            ),
            ui_config.max_input_lines,
        );
        let hints_seed = hint_reader.load_full();
        let input_rules = input_rules_reader.load_full();
        input_box
            .buffer
            .set_rules(crate::text_buffer::RuleSet::with_plugins(&input_rules));
        Self {
            chat: Chat::new(
                session_id,
                "main".into(),
                ui_config.clone(),
                lua_event_handle.clone(),
            ),
            input_box,
            command_palette: commands,
            theme_picker: ThemePicker::new(),
            alert_modal: AlertModal::new(),
            help_modal: HelpModal::new(),
            search_modal: SearchModal::new(),
            status_bar: StatusBar::new(ui_config.flash_duration()),
            status: Status::Idle,
            exit_request: ExitRequest::None,
            zones: ZoneRegistry::new(),
            selection_state: None,
            clipboard: ClipboardState::new(),
            quit_copied: false,
            node,
            link: Link::Down,
            model: String::new(),
            loop_status: None,
            attached: None,
            ui_config,
            lua_event_handle,
            announced_model: None,
            input_fired_this_frame: false,
            keymap_reader,
            hint_reader,
            hints: Watch::seeded(hints_seed),
            input_rules: Watch::seeded(input_rules),
            input_rules_reader,
            pending_actions: Vec::new(),
            typeahead: typeahead::Typeahead::default(),
            mouse_paste_rx: Vec::new(),
        }
    }

    /// Whether typed keys reach the chat input.
    pub(crate) fn input_focused(&self) -> bool {
        self.keyboard_owner() == KeyboardOwner::Chat
    }

    pub(crate) fn sync_input_mode(&self) -> InputMode {
        self.keymap_reader.sync_mode(self.input_focused())
    }

    fn holds_keyboard(&self, owner: KeyboardOwner) -> bool {
        match owner {
            KeyboardOwner::Chat => true,
            _ => self.overlay(owner).is_some_and(Overlay::is_open),
        }
    }

    fn keyboard_owner(&self) -> KeyboardOwner {
        KeyboardOwner::iter()
            .find(|&owner| self.holds_keyboard(owner))
            .unwrap_or(KeyboardOwner::Chat)
    }

    /// The mode is global, so the caller keeps {announced} across frames.
    /// Agent autocmds are inert here: nothing is fired.
    pub(crate) fn announce_mode(&self, announced: &mut InputMode) {
        let mode = self.sync_input_mode();
        let _ = mem::replace(announced, mode);
    }

    /// The input moved this frame: one dirty flag for the next paint.
    pub(crate) fn tick_input_changed(&mut self) -> crate::repaint::Dirty {
        let fired = mem::take(&mut self.input_fired_this_frame);
        crate::repaint::Dirty::from(fired)
    }

    /// The session id the hints filter on; one chat for the process.
    pub(crate) fn chat_session_id(&self) -> maki_storage::id::MakiId {
        self.chat
            .session_id()
            .unwrap_or_else(maki_storage::id::MakiId::generate)
    }

    /// Drops whatever the unloaded plugin left in the input.
    pub(crate) fn clear_plugin_marks(&mut self, plugin: &str) {
        self.input_box.clear_plugin_marks(plugin);
    }

    /// Bench door: benches link the crate and cannot see pub(crate).
    pub fn tick_bench(&mut self) -> bool {
        self.tick() == crate::repaint::Dirty::YES
    }

    /// Marks the input as changed this frame, from outside the key path
    /// (an editor edit, a paste).
    pub(crate) fn note_input_changed(&mut self, changed: bool) {
        self.input_fired_this_frame |= changed;
    }

    /// Lands a plugin's edit on the input, the way `maki.ui.input_edit` does.
    pub(crate) fn apply_input_edit(
        &mut self,
        edit: maki_lua::InputEdit,
        area: Rect,
    ) -> Result<serde_json::Value, String> {
        if !self.input_live(area) {
            return Err(INPUT_NOT_LIVE_ERR.to_owned());
        }
        let marks = edit.marks.map(|m| (WRITER_ANY, m));
        let cursor = self.input_box.apply_edits(
            edit.version,
            edit.edits,
            edit.cursor,
            edit.anchor,
            marks,
        )?;
        self.note_input_changed(true);
        Ok(serde_json::json!({ "version": self.input_box.buffer.version(), "cursor": cursor }))
    }

    /// Places a plugin's marks; the box need not be visible.
    pub(crate) fn decorate_input(
        &mut self,
        decorate: maki_lua::InputDecorate,
    ) -> Result<serde_json::Value, String> {
        let (version, marks) = decorate_marks(decorate);
        self.input_box
            .set_marks(version, WRITER_ANY, &marks.ns, marks.marks)?;
        Ok(serde_json::json!({ "version": () }))
    }

    /// A plugin submits prompt text: it rides the same send path as typing.
    pub(crate) fn submit_plugin_text(&mut self, text: &str) -> bool {
        if self.link != Link::Live {
            return false;
        }
        self.node.send(text);
        true
    }

    pub(crate) fn flash(&mut self, msg: String) {
        self.status_bar.flash(msg);
    }

    pub(crate) fn tick_error_expiry(&mut self) -> crate::repaint::Dirty {
        if matches!(self.status, Status::Error { .. }) && self.status.is_error_expired() {
            self.status = Status::Idle;
            crate::repaint::Dirty::YES
        } else {
            crate::repaint::Dirty::NO
        }
    }

    pub(crate) fn is_working(&self) -> bool {
        self.status == Status::Streaming
    }

    pub fn update(&mut self, msg: Msg) -> Vec<Action> {
        if let Msg::Key(key) = msg
            && key::QUIT.matches(key)
            && self.typeahead.abandon()
        {
            self.flash("held keys abandoned".to_owned());
            return vec![];
        }
        match self.typeahead.defer(msg) {
            Some((msg, sight, turn)) => self.handle_msg(msg, sight, &turn),
            None => vec![],
        }
    }

    fn handle_msg(&mut self, msg: Msg, sight: Sight, turn: &Turn) -> Vec<Action> {
        match msg {
            Msg::Key(key) => self.handle_key(key, sight, turn),
            Msg::Paste(text) => {
                if !text.is_empty() {
                    let pressed = typeahead::Pressed::Paste(text.clone());
                    if !self.dispatch_binding(&pressed) {
                        self.paste_text(&text, turn);
                    }
                }
                vec![]
            }
            Msg::Mouse(event) => {
                self.handle_mouse(event);
                vec![]
            }
            Msg::Scroll { column, row, delta } => {
                self.handle_scroll(column, row, delta);
                vec![]
            }
            Msg::Node(event) => {
                self.handle_node_event(event);
                vec![]
            }
        }
    }

    fn paste_text(&mut self, text: &str, _turn: &Turn) {
        match self.keyboard_owner() {
            KeyboardOwner::Chat => {
                self.input_box.handle_paste(text);
                self.input_fired_this_frame = true;
            }
            KeyboardOwner::Search => self.search_modal.handle_paste(text),
            KeyboardOwner::ThemePicker => self.theme_picker.handle_paste(text),
            KeyboardOwner::Help | KeyboardOwner::Alert => {}
        }
    }

    fn input_live(&self, area: Rect) -> bool {
        !self.any_overlay_open() && self.input_box.height(area.width) > 2
    }

    pub(crate) fn scroll_at(&mut self, column: u16, row: u16, delta: i32) {
        if self.help_modal.is_open() {
            self.help_modal.scroll(delta);
            return;
        }

        let Some(zone) = self.zone_at(row, column) else {
            return;
        };
        self.scroll_zone(zone.zone, delta);
    }

    fn handle_ctrl(&mut self, key_event: KeyEvent) -> Option<Vec<Action>> {
        if !crate::components::is_ctrl(&key_event) {
            return None;
        }
        if key::QUIT.matches(key_event) {
            self.command_palette.close();
            // Copying lets go of the selection, so the next press clears the
            // input and the one after quits, just like without one. A press
            // right after a copy never copies.
            if !mem::take(&mut self.quit_copied)
                && let Some(text) = self.input_box.take_selected_text()
            {
                self.copy(&text);
                self.quit_copied = true;
                return Some(vec![]);
            }
            return Some(if self.input_box.is_empty() {
                self.quit()
            } else {
                self.input_box.discard();
                vec![]
            });
        }
        if key::HELP.matches(key_event) {
            return Some(self.run_builtin(maki_lua::BuiltinAction::Help));
        }
        if key::SCROLL_TOP.matches(key_event) {
            self.chat.scroll_to_top();
            return Some(vec![]);
        }
        None
    }

    fn handle_key(&mut self, key_event: KeyEvent, sight: Sight, turn: &Turn) -> Vec<Action> {
        if let Some(actions) = self.handle_ctrl(key_event) {
            return actions;
        }
        // The first holder of the keyboard answers the key, the chat last:
        // its command palette is drawn under every modal.
        for owner in KeyboardOwner::iter() {
            if owner == KeyboardOwner::Chat {
                break;
            }
            if !self.holds_keyboard(owner) {
                continue;
            }
            if let Some(actions) = self.dispatch_owner(owner, key_event, sight, turn) {
                return actions;
            }
        }
        if self.dispatch_binding(&typeahead::Pressed::Key(key_event, sight)) {
            return vec![];
        }
        self.handle_builtin_key(key_event, sight, turn)
    }

    /// The chat's built-in keys that a binding did not claim, before the
    /// input box: PageUp/PageDown page the transcript and `C-Alt-G` re-pins
    /// the bottom, whatever the box holds.
    fn handle_builtin_key(
        &mut self,
        key_event: KeyEvent,
        sight: Sight,
        turn: &Turn,
    ) -> Vec<Action> {
        if key::SCROLL_PAGE_UP.matches(key_event) {
            let page = self.chat.page();
            self.chat.scroll(page);
            return vec![];
        }
        if key::SCROLL_PAGE_DOWN.matches(key_event) {
            let page = self.chat.page();
            self.chat.scroll(-page);
            return vec![];
        }
        if key::SCROLL_BOTTOM.matches(key_event) {
            self.chat.enable_auto_scroll();
            return vec![];
        }
        self.handle_chat_key(key_event, sight, turn)
    }

    fn dispatch_owner(
        &mut self,
        owner: KeyboardOwner,
        key_event: KeyEvent,
        sight: Sight,
        turn: &Turn,
    ) -> Option<Vec<Action>> {
        match owner {
            KeyboardOwner::Alert => {
                self.alert_modal.handle_key(key_event);
                Some(vec![])
            }
            KeyboardOwner::Help => {
                self.help_modal.handle_key(key_event);
                Some(Vec::new())
            }
            KeyboardOwner::Search => Some(self.handle_search_key(key_event)),
            KeyboardOwner::ThemePicker => self.handle_theme_key(key_event),
            KeyboardOwner::Chat => Some(self.handle_chat_key(key_event, sight, turn)),
        }
    }

    fn handle_search_key(&mut self, key_event: KeyEvent) -> Vec<Action> {
        match self.search_modal.handle_key(key_event) {
            SearchAction::Close(restore) => {
                if let Some((scroll, auto)) = restore {
                    self.chat.restore_scroll(scroll, auto);
                }
                self.chat.set_highlight_segment(None);
                vec![]
            }
            SearchAction::Select(segment) => {
                self.chat.scroll_to_segment(segment);
                self.chat.set_highlight_segment(Some(segment));
                vec![]
            }
            SearchAction::Navigate => {
                let corpus = self.chat.segment_search_texts();
                self.search_modal.update_matches(|| corpus);
                if let Some(segment) = self.search_modal.current_segment_index() {
                    self.chat.set_highlight_segment(Some(segment));
                }
                vec![]
            }
            SearchAction::Consumed => vec![],
        }
    }

    fn handle_theme_key(&mut self, key_event: KeyEvent) -> Option<Vec<Action>> {
        match self.theme_picker.handle_key(key_event) {
            ThemePickerAction::Consumed => None,
            ThemePickerAction::Closed => Some(vec![]),
        }
    }

    fn keyboard_focus(&self) -> typeahead::KeyboardFocus {
        typeahead::KeyboardFocus {
            window: None,
            input: self.input_focused(),
            prompts: None,
        }
    }

    fn snapshot_input(&self) -> maki_lua::InputSnapshot {
        let buffer = &self.input_box.buffer;
        maki_lua::InputSnapshot {
            session_id: "main".to_owned(),
            text: buffer.value().to_owned(),
            cursor: buffer.cursor_byte(),
            anchor: buffer.anchor(),
            version: buffer.version(),
        }
    }

    fn input_at(&self) -> typeahead::InputAt {
        typeahead::InputAt {
            version: self.input_box.buffer.version(),
            cursor: self.input_box.buffer.cursor_byte(),
            anchor: self.input_box.buffer.anchor(),
        }
    }

    /// Offers {key_event} to the lua keymap before the built-in input path.
    fn dispatch_binding(&mut self, pressed: &typeahead::Pressed) -> bool {
        let Some(key) = pressed.key().filter(|key| !key.is_reserved()) else {
            return false;
        };
        let mode = self.sync_input_mode();
        let mut hold = None;
        let claimed = self.keymap_reader.dispatch_global(key, mode, |ticket| {
            let mut ticket = ticket.with_input(self.snapshot_input());
            if let typeahead::Pressed::Paste(text) = pressed {
                ticket = ticket.with_text(text.clone());
            }
            let pending = typeahead::Hold::new(
                &ticket,
                self.keyboard_focus(),
                mode,
                pressed.clone(),
                self.input_at(),
            );
            let sent = self.lua_event_handle.run_keybind_callback(ticket);
            hold = sent.then_some(pending);
            sent
        });
        if let Some(hold) = hold {
            self.typeahead.hold(hold);
        }
        claimed
    }

    /// Everything the handler of {ticket} asked of the UI arrived before this,
    /// so the keys typed after it can go now.
    pub(crate) fn keybind_done(&mut self, ticket: u64) -> Vec<Action> {
        if !self.typeahead.held_by(ticket) {
            return vec![];
        }
        self.release_typeahead()
    }

    /// `ev.fallback()` from the handler of {ticket}: the key goes to the
    /// built-in path, if it still holds and nothing moved.
    pub(crate) fn keybind_fallback(&mut self, ticket: u64) -> Vec<Action> {
        let focus = self.keyboard_focus();
        let input = self.input_at();
        let mode = self.sync_input_mode();
        let Some(pressed) = self.typeahead.take_fallback(ticket, focus, mode, &input) else {
            return vec![];
        };
        let actions = match pressed {
            typeahead::Pressed::Key(key, sight) => self.handle_key(key, sight, &typeahead::Turn),
            typeahead::Pressed::Paste(text) => {
                self.builtin_paste(&text);
                vec![]
            }
        };
        self.typeahead.fell_back(ticket, &input, self.input_at());
        actions
    }

    fn release_typeahead(&mut self) -> Vec<Action> {
        let input = self.input_at();
        if self.typeahead.release(&input, self.keyboard_focus()) {
            self.flash("held keys dropped".to_owned());
        }
        self.replay_typeahead()
    }

    /// Replays the held keys up to the first one that hands the event loop
    /// actions, and returns those.
    pub(crate) fn replay_typeahead(&mut self) -> Vec<Action> {
        while let Some((msg, sight, turn)) = self.typeahead.next() {
            let actions = self.handle_msg(msg, sight, &turn);
            if !actions.is_empty() {
                return actions;
            }
        }
        vec![]
    }

    pub(crate) fn tick_typeahead(&mut self) -> crate::repaint::Dirty {
        if !self.typeahead.stale(|| self.keyboard_focus()) {
            return crate::repaint::Dirty::NO;
        }
        let actions = self.release_typeahead();
        self.pending_actions.extend(actions);
        crate::repaint::Dirty::YES
    }

    /// What the chat input does with a paste no one else took: the text goes
    /// in. Images are a dead plane until the protocol grows them.
    pub(crate) fn builtin_paste(&mut self, text: &str) {
        if let InputAction::Changed = self.input_box.handle_paste(text) {
            self.input_fired_this_frame = true;
        }
    }

    fn handle_chat_key(&mut self, key_event: KeyEvent, sight: Sight, turn: &Turn) -> Vec<Action> {
        // A `/` opens the palette; the palette answers while it is open.
        let actions = self
            .command_palette
            .handle_key(key_event, &self.input_box.buffer);
        match actions {
            crate::components::command::CommandAction::Passthrough => {}
            action => return self.palette_actions(action, sight, turn),
        }
        // Readline EOF: Ctrl+D exits only on an empty line, the box
        // otherwise deletes the character at the cursor.
        if key::DELETE.matches(key_event) && self.input_box.is_empty() {
            return self.quit();
        }
        match self.input_box.handle_key(key_event, self.sync_input_mode()) {
            InputAction::Submit => return self.handle_submit(),
            InputAction::Changed => self.input_fired_this_frame = true,
            InputAction::Passthrough(key) => {
                if key.code == crossterm::event::KeyCode::Esc {
                    return self.handle_esc();
                }
            }
            InputAction::ContinueLine => {}
            InputAction::None => {}
        }
        vec![]
    }

    /// Tab on the palette: swap the command word for the completed one,
    /// keeping any args the user already typed.
    fn complete_command(&mut self, text: &str) {
        let value = self.input_box.buffer.value();
        let mut parts = value.splitn(2, char::is_whitespace);
        let _word = parts.next();
        let rest = parts.next().unwrap_or("");
        let completed = if rest.is_empty() {
            text.to_owned()
        } else {
            format!("{text} {rest}")
        };
        self.input_box.buffer.set_value(&completed);
        self.input_fired_this_frame = true;
    }

    fn palette_actions(
        &mut self,
        actions: crate::components::command::CommandAction,
        _sight: Sight,
        _turn: &Turn,
    ) -> Vec<Action> {
        use crate::components::command::CommandAction;
        match actions {
            CommandAction::Consumed | CommandAction::Passthrough => vec![],
            CommandAction::Complete(text) => {
                self.complete_command(&text);
                vec![]
            }
            CommandAction::Execute(cmd) => {
                self.input_box.discard();
                self.execute_command(cmd, 0)
            }
        }
    }

    /// Enter on the input box: a `send` frame, or a local shell line. The
    /// box's own submit pushes the history entry.
    pub(crate) fn handle_submit(&mut self) -> Vec<Action> {
        let text = self.input_box.buffer.value().to_owned();
        if text.is_empty() {
            return vec![];
        }
        if let Some(shell_line) = shell::parse_shell_prefix(&text) {
            self.input_box.submit();
            return vec![Action::ShellCommand {
                id: String::new(),
                command: shell_line.command,
                visible: shell_line.visible,
            }];
        }
        if self.submit_text(&text) {
            self.input_box.submit();
            vec![]
        } else {
            self.flash(OFFLINE_SEND_HINT.to_owned());
            vec![]
        }
    }

    fn submit_text(&mut self, text: &str) -> bool {
        if self.link != Link::Live {
            return false;
        }
        self.node.send(text);
        true
    }

    fn handle_esc(&mut self) -> Vec<Action> {
        if self.selection_state.take().is_some() {
            return vec![];
        }
        if self.command_palette.is_open() {
            self.command_palette.close();
            return vec![];
        }
        if !self.input_box.is_empty() {
            self.input_box.discard();
            return vec![];
        }
        if self.is_working() {
            // The protocol has no node-side stop; say so once per press.
            self.flash(NO_STOP_HINT.to_owned());
        }
        vec![]
    }

    fn quit(&mut self) -> Vec<Action> {
        self.node.shutdown();
        self.exit_request = ExitRequest::Success;
        vec![]
    }

    pub(crate) fn run_builtin(&mut self, action: maki_lua::BuiltinAction) -> Vec<Action> {
        use maki_lua::BuiltinAction;
        match action {
            BuiltinAction::FilePicker
            | BuiltinAction::PlanToggle
            | BuiltinAction::PlanEditor
            | BuiltinAction::PopQueue
            | BuiltinAction::PrevChat
            | BuiltinAction::NextChat
            | BuiltinAction::ModelPicker
            | BuiltinAction::Background => {
                // The planes these drove (mentions, plans, queue, tabs,
                // models, background moves) belong to the agent this client
                // no longer embeds.
                vec![]
            }
            BuiltinAction::Search => {
                let scroll = self.chat.scroll_pos();
                let auto = self.chat.auto_scroll();
                self.search_modal.open(scroll, auto);
                vec![]
            }
            BuiltinAction::Help => {
                self.help_modal.toggle();
                vec![]
            }
            BuiltinAction::EditInput => vec![Action::EditInputInEditor],
            BuiltinAction::InputRaw => {
                self.input_box.buffer.toggle_raw();
                vec![]
            }
            BuiltinAction::Undo => self.input_action(InputBox::undo),
            BuiltinAction::Redo => self.input_action(InputBox::redo),
            BuiltinAction::HistoryPrev => self.input_action(InputBox::history_up),
            BuiltinAction::HistoryNext => self.input_action(InputBox::history_down),
            BuiltinAction::Newline => self.input_action(|input| {
                input.newline();
                true
            }),
        }
    }

    /// The input actions skip the mode check the keys make. The mode decides
    /// what a typed key does, and an action is a plugin asking on purpose: a
    /// vim plugin's `u` runs `undo` from normal mode.
    fn input_action(&mut self, edit: impl FnOnce(&mut InputBox) -> bool) -> Vec<Action> {
        if edit(&mut self.input_box) {
            self.input_fired_this_frame = true;
        }
        vec![]
    }

    pub(crate) fn copy(&mut self, text: &str) {
        match self.clipboard.copy_text(text) {
            Ok(crate::clipboard::CopyResult::Copied) => {
                self.status_bar.flash("Copied selection".to_owned());
            }
            Ok(crate::clipboard::CopyResult::Noop) => {}
            Err(e) => self.status_bar.flash(format!("Copy failed: {e}")),
        }
    }

    pub(crate) fn any_overlay_open(&self) -> bool {
        self.overlays().any(Overlay::is_open)
    }

    fn overlays(&self) -> impl Iterator<Item = &dyn Overlay> {
        let items: [&dyn Overlay; 4] = [
            &self.alert_modal,
            &self.help_modal,
            &self.search_modal,
            &self.theme_picker,
        ];
        items.into_iter()
    }

    fn overlay(&self, owner: KeyboardOwner) -> Option<&dyn Overlay> {
        match owner {
            KeyboardOwner::Alert => Some(&self.alert_modal),
            KeyboardOwner::Help => Some(&self.help_modal),
            KeyboardOwner::Search => Some(&self.search_modal),
            KeyboardOwner::ThemePicker => Some(&self.theme_picker),
            KeyboardOwner::Chat => None,
        }
    }

    /// The overall animation cadence: the chat's plus the bar's spinner.
    pub(crate) fn cadence(&self) -> crate::repaint::Cadence {
        let chat = self.chat.cadence();
        let bar = self
            .status_bar
            .cadence(&crate::app::mouse::status_motion(self.is_working()));
        crate::repaint::Cadence::any([chat, bar])
    }

    /// Per-frame bookkeeping: flash expiry, highlight-worker drains, the
    /// splash while idle.
    pub(crate) fn tick(&mut self) -> crate::repaint::Dirty {
        let mut dirty = self.chat.tick();
        dirty |= self.status_bar.clear_expired_hint();
        dirty |= self.status_bar.poll_branch_update();
        dirty |= self.tick_error_expiry();
        dirty |= self.tick_input_changed();
        dirty |= self.refresh_lua_snapshots();
        dirty
    }

    fn refresh_lua_snapshots(&mut self) -> crate::repaint::Dirty {
        let hints = self.hint_reader.load_full();
        let hints_dirty = self.hints.poll(hints);
        let _ = hints_dirty;
        let rules = self.input_rules_reader.load_full();
        let rules_dirty = self.input_rules.poll(rules.clone());
        if rules_dirty == crate::repaint::Dirty::YES {
            self.input_box
                .buffer
                .set_rules(crate::text_buffer::RuleSet::with_plugins(&rules));
        }
        rules_dirty
    }

    pub(super) fn input_placeholder(&self) -> crate::components::input::Placeholder {
        if self.chat.has_user_message() {
            crate::components::input::Placeholder::Blank
        } else {
            crate::components::input::Placeholder::Suggestion
        }
    }

    pub(super) fn mode_label(&self) -> (std::borrow::Cow<'static, str>, ratatui::style::Style) {
        let label = if self.is_bash_input() {
            "[BASH]".into()
        } else {
            "[NODE]".into()
        };
        let style = ratatui::style::Style::new()
            .fg(self.effective_mode_color())
            .add_modifier(ratatui::style::Modifier::BOLD);
        (label, style)
    }

    fn is_bash_input(&self) -> bool {
        self.input_box.buffer.value().starts_with('!')
    }

    pub(super) fn effective_mode_color(&self) -> ratatui::style::Color {
        if self.is_bash_input() {
            theme::current().mode_bash
        } else {
            theme::current().mode_build
        }
    }

    /// Dim while a turn is live, so a streaming chat reads busy.
    pub(super) fn separator_style(&self) -> ratatui::style::Style {
        if self.is_working() {
            theme::current().input_border
        } else {
            ratatui::style::Style::new().fg(self.effective_mode_color())
        }
    }
}

/// The keyboard holders, first wins. Drawn-over owners come first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeyboardOwner {
    Alert,
    Help,
    Search,
    ThemePicker,
    /// Nothing above holds the keyboard: the chat input. It comes last
    /// because the command palette it answers is drawn under every modal.
    Chat,
}

impl KeyboardOwner {
    fn iter() -> impl Iterator<Item = KeyboardOwner> {
        [
            KeyboardOwner::Alert,
            KeyboardOwner::Help,
            KeyboardOwner::Search,
            KeyboardOwner::ThemePicker,
            KeyboardOwner::Chat,
        ]
        .into_iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::NodeHandle;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
    use maki_storage::id::MakiId;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    const TRANSCRIPT_LINES: usize = 200;

    fn bare_key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    pub(super) fn app() -> App {
        let scratch = tempfile::tempdir().expect("scratch dir");
        let storage = maki_storage::StateDir::from_path(scratch.path().to_path_buf());
        App::new(
            MakiId::generate(),
            NodeHandle::detached(),
            storage,
            maki_config::UiConfig::default(),
            maki_lua::KeymapReader::empty(),
            maki_lua::HintReader::empty(),
            maki_lua::InputRulesReader::empty(),
            maki_lua::EventHandle::disconnected_for_test(),
            crate::components::command::CommandPalette::new(maki_lua::LuaCommandReader::empty()),
        )
    }

    /// One tall committed row, and a frame drawn at a short height, so the
    /// transcript has somewhere to scroll.
    fn app_with_tall_transcript(area: ratatui::layout::Rect) -> App {
        let mut app = app();
        let text = (0..TRANSCRIPT_LINES)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        app.handle_node_event(NodeEvent::User { text, author: None });
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
        terminal
            .draw(|frame| {
                app.view(frame);
            })
            .unwrap();
        app
    }

    #[test]
    fn page_up_scrolls_the_transcript_and_page_down_returns() {
        let area = ratatui::layout::Rect::new(0, 0, 40, 10);
        let mut app = app_with_tall_transcript(area);
        assert!(app.chat.auto_scroll(), "pinned to the bottom at first");

        let actions = app.update(Msg::Key(bare_key(KeyCode::PageUp)));
        assert!(actions.is_empty());
        assert!(!app.chat.auto_scroll(), "PageUp unpins the bottom");
        let up = app.chat.scroll_pos();

        app.update(Msg::Key(bare_key(KeyCode::PageDown)));
        let back = app.chat.scroll_pos();
        assert!(back >= up, "PageDown made progress back down");
        assert_ne!(up, back, "the two keys went different ways");
    }

    #[test]
    fn page_keys_do_not_reach_the_input_box() {
        let area = ratatui::layout::Rect::new(0, 0, 40, 10);
        let mut app = app_with_tall_transcript(area);
        app.input_box.set_input("draft");
        app.update(Msg::Key(bare_key(KeyCode::PageUp)));
        assert_eq!(app.input_box.buffer.value(), "draft");
    }

    fn ctrl_d() -> Msg {
        Msg::Key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL))
    }

    #[test]
    fn ctrl_d_quits_on_an_empty_line() {
        let area = ratatui::layout::Rect::new(0, 0, 40, 10);
        let mut app = app_with_tall_transcript(area);
        app.update(ctrl_d());
        assert_eq!(app.exit_request, ExitRequest::Success);
    }

    #[test]
    fn ctrl_d_with_text_deletes_instead_of_quitting() {
        let area = ratatui::layout::Rect::new(0, 0, 40, 10);
        let mut app = app_with_tall_transcript(area);
        app.input_box.set_input("draft");
        app.update(ctrl_d());
        assert_eq!(app.exit_request, ExitRequest::None);
        assert_eq!(app.input_box.buffer.value(), "raft");
    }

    #[test]
    fn ctrl_alt_g_repins_the_bottom() {
        let area = ratatui::layout::Rect::new(0, 0, 40, 10);
        let mut app = app_with_tall_transcript(area);
        app.update(Msg::Key(bare_key(KeyCode::PageUp)));
        assert!(!app.chat.auto_scroll());

        let g = KeyEvent {
            code: KeyCode::Char('g'),
            modifiers: KeyModifiers::CONTROL.union(KeyModifiers::ALT),
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        app.update(Msg::Key(g));
        assert!(app.chat.auto_scroll());
    }
}
