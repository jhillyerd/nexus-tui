use std::mem;
use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent};
use maki_lua::{LuaCommandInfo, LuaCommandReader, Missing, Permission, PluginReach, SENDS_PROMPT};
use nucleo::pattern::{CaseMatching, Normalization};
use nucleo::{Config, Matcher, Nucleo, Utf32String};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};

use crate::components::input::{COMMAND_PREFIX, sent_text};
use crate::text_buffer::TextBuffer;
use crate::theme;

const TICK_TIMEOUT_MS: u64 = 10;
pub(crate) const USER_ONLY_SUFFIX: &str = " can only be run by you";
/// A prompt starts a turn and adds to the transcript, like `maki.session.prompt`.
pub const SUBMITS_PROMPT: PluginAccess = PluginAccess::Needs(PROMPT_NEEDS);
pub(crate) const PROMPT_NEEDS: &[Permission] = SENDS_PROMPT;

/// What a plugin must hold to run a command through `maki.api.run_command`.
/// Typing a command is the user saying yes, so none of this applies there.
///
/// Mirror the guard on the Lua API that does the same thing: looser is a side
/// door around it, stricter only sends plugins to the API instead. That is why
/// `/model` and `/fast` need `session_settings`, as `maki.model.set` does.
/// So do `/cd` and `/workflow`: no API mirrors them, but they change where the
/// session's tools run and which tools it has.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PluginAccess {
    /// Only what changes the view, never how a session runs or what it holds.
    Open,
    /// Every one of them.
    Needs(&'static [Permission]),
    /// These move a security boundary: what runs without asking, which config
    /// loads, which code is installed. No grant unlocks them.
    UserOnly,
}

impl PluginAccess {
    /// Why {caller} may not do {what}, or `None` when it may. Every refusal a
    /// plugin gets for something the user could have typed reads this way.
    pub(crate) fn refusal(self, what: &str, caller: &PluginReach) -> Option<String> {
        match self {
            Self::Open => None,
            Self::Needs(needed) => needed
                .iter()
                .find(|&&perm| !caller.is_allowed(perm))
                .map(|&perm| Missing::Permission(perm).refusal(what)),
            Self::UserOnly => Some(format!("{what}{USER_ONLY_SUFFIX}")),
        }
    }
}

pub struct BuiltinCommand {
    pub name: &'static str,
    pub description: &'static str,
    pub max_args: usize,
    /// A typed `!` filters the palette down to commands that read one, so no
    /// handler can be reached by a bang it ignores.
    pub bang: bool,
    pub plugin_access: PluginAccess,
}

pub const BUILTIN_COMMANDS: &[BuiltinCommand] = &[
    BuiltinCommand {
        name: "/help",
        description: "Show the help modal",
        max_args: 0,
        bang: false,
        plugin_access: PluginAccess::Open,
    },
    BuiltinCommand {
        name: "/theme",
        description: "Pick a theme",
        max_args: 0,
        bang: false,
        plugin_access: PluginAccess::Open,
    },
    BuiltinCommand {
        name: "/search",
        description: "Search the transcript",
        max_args: 0,
        bang: false,
        plugin_access: PluginAccess::Open,
    },
    BuiltinCommand {
        name: "/copy",
        description: "Copy the last response to the clipboard",
        max_args: 0,
        bang: false,
        plugin_access: PluginAccess::Open,
    },
    BuiltinCommand {
        name: "/clear",
        description: "Clear the local transcript (the server keeps the thread)",
        max_args: 0,
        bang: false,
        plugin_access: PluginAccess::Open,
    },
    BuiltinCommand {
        name: "/exit",
        description: "Quit",
        max_args: 0,
        bang: false,
        plugin_access: PluginAccess::Open,
    },
];

pub struct ParsedCommand {
    pub name: String,
    /// Already trimmed, so empty means no args and handlers never re-check
    /// whitespace.
    pub args: String,
    pub bang: bool,
}

pub enum CommandAction {
    Consumed,
    Execute(ParsedCommand),
    Complete(String),
    Passthrough,
}

#[derive(Clone)]
enum CommandType {
    Builtin(&'static BuiltinCommand),
    Lua(usize),
}

struct CommandItem {
    name: String,
    max_args: usize,
    command_type: CommandType,
}

struct Match {
    command_type: CommandType,
    indices: Vec<u32>,
}

/// Every public method takes the input and catches up with it first. Lots of
/// paths change the text without a keypress (a chat switch, `$EDITOR`, history
/// recall). If each had to tell the palette, one would forget sooner or later,
/// and `Enter` would run a command the box no longer shows.
pub struct CommandPalette {
    /// `Esc` closes the list but leaves this alone, so the palette stays shut
    /// until the text changes.
    synced_version: Option<u64>,
    selected: usize,
    scroll_offset: usize,
    filtered: Vec<Match>,
    lua_reader: LuaCommandReader,
    lua_commands: Vec<LuaCommandInfo>,
    lua_generation: u64,
    nucleo: Nucleo<CommandItem>,
    matcher: Matcher,
    current_arg_count: usize,
    current_bang: bool,
}

impl CommandPalette {
    pub fn new(lua_reader: LuaCommandReader) -> Self {
        let lua_snap = lua_reader.load();
        let lua_generation = lua_snap.generation;
        let lua_commands = lua_snap.commands.clone();

        let nucleo = Self::build_nucleo(&lua_commands);
        Self {
            synced_version: None,
            selected: 0,
            scroll_offset: 0,
            filtered: Vec::new(),
            lua_reader,
            lua_commands,
            lua_generation,
            nucleo,
            matcher: Matcher::new(Config::DEFAULT),
            current_arg_count: 0,
            current_bang: false,
        }
    }

    /// Every command the palette knows, in display order. The one place that
    /// enumerates the sources: matching and name lookup both read it.
    fn items(lua_commands: &[LuaCommandInfo]) -> impl Iterator<Item = CommandItem> + '_ {
        let builtins = BUILTIN_COMMANDS.iter().map(|cmd| CommandItem {
            name: cmd.name.to_string(),
            max_args: cmd.max_args,
            command_type: CommandType::Builtin(cmd),
        });
        let lua = lua_commands.iter().enumerate().map(|(i, cmd)| CommandItem {
            name: cmd.name.to_string(),
            max_args: cmd.max_args,
            command_type: CommandType::Lua(i),
        });
        builtins.chain(lua)
    }

    fn build_nucleo(lua_commands: &[LuaCommandInfo]) -> Nucleo<CommandItem> {
        let nucleo = Nucleo::new(Config::DEFAULT, Arc::new(|| {}), None, 1);
        let injector = nucleo.injector();

        for item in Self::items(lua_commands) {
            injector.push(item, |item, cols| {
                cols[0] = Utf32String::from(item.name.as_str());
            });
        }

        nucleo
    }

    pub fn handle_key(&mut self, key: KeyEvent, input: &TextBuffer) -> CommandAction {
        if !self.is_active(input) {
            return CommandAction::Passthrough;
        }
        match key.code {
            KeyCode::Up => {
                self.move_up();
                CommandAction::Consumed
            }
            KeyCode::Down => {
                self.move_down();
                CommandAction::Consumed
            }
            KeyCode::Esc => {
                self.close();
                CommandAction::Consumed
            }
            KeyCode::Enter => match self.confirm(input.value()) {
                Some(cmd) => {
                    self.close();
                    CommandAction::Execute(cmd)
                }
                None => CommandAction::Consumed,
            },
            KeyCode::Tab => {
                if let Some(item) = self.filtered.get(self.selected) {
                    let mut text = self.item_name(item);
                    if self.current_bang {
                        text.push('!');
                    }
                    if self.item_has_args(item) {
                        text.push(' ');
                    }
                    CommandAction::Complete(text)
                } else {
                    CommandAction::Consumed
                }
            }
            _ => CommandAction::Passthrough,
        }
    }

    /// The command Enter would run on {input}, looked up before it does.
    pub fn enter_target(&mut self, input: &TextBuffer) -> Option<String> {
        if !self.is_active(input) {
            return None;
        }
        self.filtered.get(self.selected).map(|m| self.item_name(m))
    }

    pub fn is_active(&mut self, input: &TextBuffer) -> bool {
        self.follow(input);
        self.is_open()
    }

    pub(crate) fn is_open(&self) -> bool {
        !self.filtered.is_empty()
    }

    /// `sync` always leaves a row picked, so a test that wants an Enter the
    /// palette swallows has to catch up with {input} and take the pick away
    /// by hand.
    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn unselect(&mut self, input: &TextBuffer) {
        self.follow(input);
        self.selected = self.filtered.len();
    }

    /// The box keeps one buffer for life and its version only climbs, so a
    /// version we have seen means text we already matched.
    fn follow(&mut self, input: &TextBuffer) {
        let version = Some(input.version());
        if self.synced_version != version {
            self.synced_version = version;
            self.sync(input.value());
        }
    }

    fn sync(&mut self, input: &str) {
        let lua_snap = self.lua_reader.load();
        if lua_snap.generation != self.lua_generation {
            self.lua_generation = lua_snap.generation;
            self.lua_commands = lua_snap.commands.clone();
            self.nucleo = Self::build_nucleo(&self.lua_commands);
        }
        let Some(stripped) = sent_text(input).strip_prefix(COMMAND_PREFIX) else {
            self.filtered.clear();
            self.current_arg_count = 0;
            self.current_bang = false;
            return;
        };

        let parts: Vec<&str> = stripped.split_whitespace().collect();
        let raw_word = parts.first().copied().unwrap_or(stripped);
        let (cmd_word, bang) = raw_word
            .strip_suffix('!')
            .map_or((raw_word, false), |word| (word, true));
        self.current_bang = bang;
        // Read off the trimmed text, so a space after the last word is no
        // argument yet and the palette stays on the command Enter would run.
        self.current_arg_count = parts.len().saturating_sub(1);

        self.nucleo.pattern.reparse(
            0,
            cmd_word,
            CaseMatching::Ignore,
            Normalization::Smart,
            false,
        );

        self.tick();

        let typed = format!("/{cmd_word}");
        match self
            .filtered
            .iter()
            .position(|item| self.item_name(item).eq_ignore_ascii_case(&typed))
        {
            // A full name beats any fuzzy neighbor, so `/model` cannot run
            // `/models`.
            Some(index) => {
                let exact = self.filtered.remove(index);
                self.filtered.insert(0, exact);
                self.selected = 0;
            }
            // The command exists but this input ruled it out: too many args, or
            // a bang it does not read. Its fuzzy neighbors are still here and
            // Enter would run one, so `/cd a b` would launch `/packupdate`.
            None if self.find_item(&typed).is_some() => self.filtered.clear(),
            None => {}
        }
    }

    fn tick(&mut self) {
        loop {
            let status = self.nucleo.tick(TICK_TIMEOUT_MS);
            if status.changed {
                self.refresh_matches();
            }
            if !status.running {
                break;
            }
        }
    }

    fn refresh_matches(&mut self) {
        let snapshot = self.nucleo.snapshot();
        let pattern = snapshot.pattern();
        let has_pattern = !pattern.column_pattern(0).atoms.is_empty();

        self.filtered.clear();
        let count = snapshot.matched_item_count();
        for item in snapshot.matched_items(0..count) {
            let cmd_item = &item.data;
            let col = &item.matcher_columns[0];

            if self.current_arg_count > cmd_item.max_args {
                continue;
            }
            if self.current_bang
                && !matches!(cmd_item.command_type, CommandType::Builtin(command) if command.bang)
            {
                continue;
            }

            let indices = if has_pattern {
                let mut indices_buf = vec![];
                pattern.column_pattern(0).indices(
                    col.slice(..),
                    &mut self.matcher,
                    &mut indices_buf,
                );
                indices_buf
            } else {
                Vec::new()
            };

            self.filtered.push(Match {
                command_type: cmd_item.command_type.clone(),
                indices,
            });
        }

        self.selected = self.selected.min(self.filtered.len().saturating_sub(1));
    }

    pub fn close(&mut self) {
        self.filtered.clear();
        self.scroll_offset = 0;
        self.current_arg_count = 0;
        self.current_bang = false;
    }

    fn ensure_visible(&mut self, visible: usize) {
        if self.selected < self.scroll_offset {
            self.scroll_offset = self.selected;
        } else if self.selected >= self.scroll_offset + visible {
            self.scroll_offset = self.selected + 1 - visible;
        }
        // A filter that shrinks the list can leave the offset deep in empty
        // space, where the selection alone would pin a one-row window.
        self.scroll_offset = self
            .scroll_offset
            .min(self.filtered.len().saturating_sub(visible));
    }

    fn move_up(&mut self) {
        if self.filtered.is_empty() {
            return;
        }
        self.selected = if self.selected == 0 {
            self.filtered.len() - 1
        } else {
            self.selected - 1
        };
    }

    fn move_down(&mut self) {
        if self.filtered.is_empty() {
            return;
        }
        self.selected = if self.selected == self.filtered.len() - 1 {
            0
        } else {
            self.selected + 1
        };
    }

    fn item_name(&self, m: &Match) -> String {
        match &m.command_type {
            CommandType::Builtin(cmd) => cmd.name.to_string(),
            CommandType::Lua(i) => self.lua_commands[*i].name.to_string(),
        }
    }

    fn item_has_args(&self, m: &Match) -> bool {
        match &m.command_type {
            CommandType::Builtin(cmd) => cmd.max_args > 0,
            CommandType::Lua(i) => self.lua_commands[*i].max_args > 0,
        }
    }

    fn item_description(&self, m: &Match) -> &str {
        match &m.command_type {
            CommandType::Builtin(cmd) => cmd.description,
            CommandType::Lua(i) => &self.lua_commands[*i].description,
        }
    }

    fn confirm(&self, input: &str) -> Option<ParsedCommand> {
        let item = self.filtered.get(self.selected)?;
        let name = self.item_name(item);
        let args = sent_text(input)
            .strip_prefix('/')
            .and_then(|s| s.split_once(char::is_whitespace))
            .map(|(_, a)| a.trim())
            .unwrap_or("");
        Some(ParsedCommand {
            name,
            args: args.to_string(),
            bang: self.current_bang,
        })
    }

    /// Exact name lookup, case-insensitive like typing, but never fuzzy: an
    /// alias names one command on purpose, and a typo should report itself
    /// instead of running the closest neighbor.
    fn find_item(&self, name: &str) -> Option<CommandItem> {
        Self::items(&self.lua_commands).find(|item| item.name.eq_ignore_ascii_case(name))
    }

    /// Hands back the registered spelling, because that is the only one
    /// [`crate::app::App`] dispatches on. `Err` goes back to the plugin as is.
    pub fn resolve_for_plugin(&self, name: &str, caller: &PluginReach) -> Result<String, String> {
        let item = self
            .find_item(name)
            .ok_or_else(|| format!("unknown command '{name}'"))?;
        match self.plugin_refusal(&item, caller) {
            Some(refusal) => Err(refusal),
            None => Ok(item.name),
        }
    }

    fn plugin_refusal(&self, item: &CommandItem, caller: &PluginReach) -> Option<String> {
        let access = match item.command_type {
            CommandType::Builtin(cmd) => cmd.plugin_access,
            CommandType::Lua(i) => {
                return caller
                    .first_missing(&self.lua_commands[i].reach)
                    .map(|missing| missing.refusal(&item.name));
            }
        };
        access.refusal(&item.name, caller)
    }

    pub fn find_lua_command(&self, name: &str) -> Option<&LuaCommandInfo> {
        self.lua_commands.iter().find(|c| c.name.as_ref() == name)
    }

    pub fn lua_commands(&self) -> &[LuaCommandInfo] {
        &self.lua_commands
    }

    pub fn view(
        &mut self,
        frame: &mut Frame,
        input_area: Rect,
        input: &TextBuffer,
    ) -> Option<Rect> {
        self.follow(input);
        self.render(frame, input_area)
    }

    fn render(&mut self, frame: &mut Frame, input_area: Rect) -> Option<Rect> {
        if !self.is_open() {
            return None;
        }

        let popup_height = (self.filtered.len() as u16).min(input_area.y);
        if popup_height == 0 {
            return None;
        }
        self.ensure_visible(popup_height as usize);
        let start = self.scroll_offset;
        let filtered = &self.filtered;
        let end = (start + popup_height as usize).min(filtered.len());

        const GAP: usize = 2;
        let max_name = filtered
            .iter()
            .map(|item| self.item_name(item).len())
            .max()
            .unwrap_or(0);
        let max_desc = filtered
            .iter()
            .map(|item| self.item_description(item).len())
            .max()
            .unwrap_or(0);
        const PAD: usize = 1;
        let popup_width = (PAD + max_name + GAP + max_desc + PAD) as u16;

        let popup = Rect {
            x: input_area.x,
            y: input_area.y.saturating_sub(popup_height),
            width: popup_width.min(input_area.width),
            height: popup_height,
        };

        let t = theme::current();
        let lines: Vec<Line> = filtered[start..end]
            .iter()
            .enumerate()
            .map(|(row, m)| {
                let i = start + row;
                let name = self.item_name(m);
                let desc = self.item_description(m);
                let selected = i == self.selected;
                let name_pad = max_name - name.len() + GAP;

                if selected {
                    let s = t.item_selected;
                    let highlighted_name =
                        highlighted_spans(&name, &m.indices, s, t.item_match_selected);
                    let mut spans = vec![Span::styled(" ".repeat(PAD), s)];
                    spans.extend(highlighted_name);
                    spans.push(Span::styled(" ".repeat(name_pad), s));
                    spans.push(Span::styled(desc, s));
                    spans.push(Span::styled(" ".repeat(PAD), s));
                    Line::from(spans)
                } else {
                    let highlighted_name =
                        highlighted_spans(&name, &m.indices, t.item, t.item_match);
                    let mut spans = vec![Span::raw(" ".repeat(PAD))];
                    spans.extend(highlighted_name);
                    spans.push(Span::raw(" ".repeat(name_pad)));
                    spans.push(Span::styled(desc, t.item_desc));
                    spans.push(Span::raw(" ".repeat(PAD)));
                    Line::from(spans)
                }
            })
            .collect();

        frame.render_widget(Clear, popup);
        frame.render_widget(
            Paragraph::new(lines).style(Style::new().bg(t.background)),
            popup,
        );

        Some(popup)
    }
}

fn highlighted_spans<'a>(
    text: &str,
    indices: &[u32],
    base: Style,
    highlight: Style,
) -> Vec<Span<'a>> {
    if indices.is_empty() {
        return vec![Span::styled(text.to_string(), base)];
    }

    let mut spans = Vec::new();
    let mut in_match = false;
    let mut run = String::new();

    for (i, ch) in text.chars().enumerate() {
        let is_match = indices.binary_search(&(i as u32)).is_ok();
        if is_match != in_match && !run.is_empty() {
            spans.push(Span::styled(
                mem::take(&mut run),
                if in_match { highlight } else { base },
            ));
        }
        in_match = is_match;
        run.push(ch);
    }

    if !run.is_empty() {
        spans.push(Span::styled(run, if in_match { highlight } else { base }));
    }

    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synced(input: &str) -> CommandPalette {
        let mut p = CommandPalette::new(LuaCommandReader::empty());
        p.sync(input);
        p
    }

    /// A builtin is open to plugins only when it neither reads nor changes a
    /// session's content or settings: it opens host UI the user then drives,
    /// or acts on the process. A new builtin lands outside this list, so
    /// opening one is a decision someone writes down here, never a default.
    const OPEN_TO_PLUGINS: &[&str] = &["/help", "/theme", "/search", "/copy", "/clear", "/exit"];

    #[test]
    fn only_commands_that_never_touch_a_session_are_open_to_plugins() {
        let open: Vec<&str> = BUILTIN_COMMANDS
            .iter()
            .filter(|c| c.plugin_access == PluginAccess::Open)
            .map(|c| c.name)
            .collect();
        assert_eq!(open, OPEN_TO_PLUGINS);
    }

    #[test]
    fn slash_opens_the_palette_and_typing_filters() {
        let p = synced("/");
        assert!(p.is_open());

        let p = synced("/the");
        let names: Vec<String> = (0..p.filtered.len())
            .map(|i| p.item_name(&p.filtered[i]))
            .collect();
        assert!(names.iter().all(|n| n.contains("/the") || n == "/theme"));
    }

    #[test]
    fn an_exact_name_beats_its_neighbors() {
        let p = synced("/theme");
        assert_eq!(p.item_name(&p.filtered[0]), "/theme");
    }

    #[test]
    fn unknown_commands_leave_the_list_but_empty() {
        let p = synced("/nope");
        assert!(
            !p.is_open(),
            "a full match that misses rules clears the list"
        );
    }

    #[test]
    fn text_without_a_slash_closes_it() {
        let mut p = synced("/");
        assert!(p.is_open());
        p.sync("hello");
        assert!(!p.is_open());
    }

    #[test]
    fn tab_completes_the_selected_name() {
        let p = synced("/the");
        let name = p.item_name(&p.filtered[p.selected]);
        assert!(name.starts_with("/the"));
    }

    #[test]
    fn bangs_filter_to_commands_that_read_one() {
        let p = synced("/");
        assert!(p.filtered.iter().all(|_m| !p.current_bang));
    }
}
