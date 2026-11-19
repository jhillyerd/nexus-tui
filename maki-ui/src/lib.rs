//! The nexus TUI: a ratatui front-end for one nexus thread. The node
//! connection (see [`node`]) feeds [`NodeEvent`]s into the app; the app
//! renders the transcript and sends user turns back.

pub mod animation;
pub mod app;
mod appearance;
#[doc(hidden)]
pub mod bench_support;
pub mod chat;
mod clipboard;
mod clock;
mod color_compat;
mod components;
mod decor_worker;
pub use components::command::{BUILTIN_COMMANDS, BuiltinCommand, PluginAccess, SUBMITS_PROMPT};
pub use components::keybindings;
mod highlight;
pub use highlight::highlight_ansi;
mod markdown;
pub use markdown::text_to_lines;
mod node;
mod render_worker;
pub mod repaint;
mod selection;
pub mod splash;
mod text_buffer;
mod theme;
mod tty_probe;
pub use theme::BUNDLED_THEMES;
pub mod update;
pub mod wrap;

mod event_loop;
mod input;
pub use node::{NodeEvent, NodeHandle};
mod present;
mod terminal;

use std::time::Instant;

use color_eyre::Result;
use maki_config::UiConfig;
use maki_storage::StateDir;
use maki_storage::id::MakiId;

use crate::components::command::CommandPalette;

/// Width of the controlling terminal, if any. Answers even when stdout is
/// redirected, so callers that care gate on [`std::io::IsTerminal`].
pub fn terminal_width() -> Option<u16> {
    crossterm::terminal::size().ok().map(|(w, _)| w)
}

pub use event_loop::EventLoopParams;

/// How a UI run ended.
pub use event_loop::RunOutcome;

/// Build the app the loop drives: one chat, one input, the lua-fed decor
/// (keymaps, hints, input rules), and the node handle.
#[allow(clippy::too_many_arguments)]
pub fn build_app(
    session_id: MakiId,
    node: NodeHandle,
    storage: StateDir,
    ui_config: UiConfig,
    lua: &maki_lua::PluginHost,
) -> Result<crate::app::App> {
    let keymap_reader = lua.keymap_reader();
    let hint_reader = lua.hint_reader();
    let input_rules_reader = lua.input_rules_reader();
    let lua_command_reader = lua.command_reader();
    let event_handle = lua.event_handle();
    let palette = CommandPalette::new(lua_command_reader);
    Ok(app::App::new(
        session_id,
        node,
        storage,
        ui_config,
        keymap_reader,
        hint_reader,
        input_rules_reader,
        event_handle,
        palette,
    ))
}

/// Loads the lua init files and hands back the UI config (theme, input
/// behavior) plus any startup warnings.
pub fn load_config(lua: &mut maki_lua::PluginHost) -> Result<(UiConfig, Vec<String>)> {
    let mut warnings = Vec::new();
    // Global init.lua only: project config was trust-gated agent plumbing,
    // and a thin client has no local trust surface — the server owns it.
    let init_files = maki_lua::InitFiles::Global;
    let raw = lua
        .load_init_files(init_files, &mut warnings)
        .map_err(|e| color_eyre::eyre::eyre!("{e}"))?;
    let config = raw
        .unwrap_or_default()
        .into_config(&[])
        .map_err(|e| color_eyre::eyre::eyre!("{e}"))?;
    Ok((config.ui, warnings))
}

pub async fn run(params: EventLoopParams) -> Result<RunOutcome> {
    let started = Instant::now();
    let outcome = event_loop::run(params).await?;
    tracing::info!(
        elapsed_ms = started.elapsed().as_millis() as u64,
        "ui run ended"
    );
    Ok(outcome)
}
