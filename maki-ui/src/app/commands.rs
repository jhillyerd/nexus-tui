//! The slash commands a node client answers: the client-side builtins and
//! the lua commands. Everything else lives on the server now.

use std::sync::Arc;

use maki_lua::BuiltinAction;

use super::App;
use crate::components::Action;
use crate::components::command::ParsedCommand;

const EXIT_COMMAND: &str = "/exit";

impl App {
    /// Entry point for `maki.api.run_command`: splits a command line into the
    /// name and args the input bar would hand over, leading slash optional.
    /// `Err` means nothing ran at all, so the Lua caller can say why.
    pub(crate) fn run_cmdline(
        &mut self,
        cmdline: &str,
        caller: &maki_lua::PluginReach,
    ) -> Result<Vec<Action>, String> {
        let trimmed = cmdline.trim();
        let (name, args) = trimmed
            .split_once(char::is_whitespace)
            .unwrap_or((trimmed, ""));
        let resolved = self
            .command_palette
            .resolve_for_plugin(&format!("/{}", name.trim_start_matches('/')), caller)?;
        Ok(self.execute_command(
            ParsedCommand {
                name: resolved,
                args: args.trim().to_owned(),
                bang: false,
            },
            0,
        ))
    }

    pub(super) fn execute_command(&mut self, cmd: ParsedCommand, _depth: u8) -> Vec<Action> {
        match cmd.name.as_str() {
            "/help" => self.run_builtin(BuiltinAction::Help),
            "/theme" => {
                self.theme_picker.open();
                vec![]
            }
            "/search" => self.run_builtin(BuiltinAction::Search),
            "/copy" => {
                let text = self.chat.last_message_text().to_owned();
                if text.is_empty() {
                    self.flash("nothing to copy".into());
                    vec![]
                } else {
                    self.copy(&text);
                    vec![]
                }
            }
            "/clear" => {
                self.chat.messages_panel().load_messages(Vec::new());
                vec![]
            }
            EXIT_COMMAND => self.quit(),
            name if self.command_palette.find_lua_command(name).is_some() => {
                self.run_lua_command(name, cmd.args);
                vec![]
            }
            _ => vec![],
        }
    }

    fn run_lua_command(&self, name: &str, args: String) {
        let Some(lua_cmd) = self.command_palette.find_lua_command(name) else {
            return;
        };
        self.lua_event_handle.run_command(
            Arc::clone(&lua_cmd.plugin),
            Arc::clone(&lua_cmd.name),
            args,
            0,
        );
    }
}
