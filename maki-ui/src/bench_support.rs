//! A bench links this crate from outside, so the few private items it needs
//! to build an `App` and drive its input get a public door here.

use std::path::PathBuf;
use std::time::Duration;

use maki_config::UiConfig;
use maki_lua::{
    EventHandle, HintReader, InputRule, InputRulesReader, KeymapReader, LuaCommandReader, TextEdit,
};
use maki_storage::StateDir;
use ratatui::layout::Rect;

use crate::app::App;
use crate::components::command::CommandPalette;
use crate::node::NodeHandle;
use crate::text_buffer::RuleSet;

const CWD: &str = "/tmp";
const SETTLE_TIMEOUT: Duration = Duration::from_secs(120);

/// No Lua host and no node, so the numbers are the UI's own work. {storage}
/// should be a scratch dir, because input history lands in it.
pub fn app(storage: StateDir) -> App {
    App::new(
        maki_storage::id::MakiId::generate(),
        NodeHandle::detached(),
        storage,
        UiConfig::default(),
        KeymapReader::empty(),
        HintReader::empty(),
        InputRulesReader::empty(),
        EventHandle::disconnected_for_test(),
        CommandPalette::new(LuaCommandReader::empty()),
    )
}

pub fn input_value(app: &App) -> &str {
    app.input_box.buffer.value()
}

/// Lands the text the way history recall does, with the cursor parked at
/// the start.
pub fn set_input(app: &mut App, text: &str) {
    app.input_box.set_input(text);
}

/// Lands the edit the way `maki.ui.input_edit` does. {area} matters because
/// a terminal that leaves the box no text row refuses plugin writes.
pub fn plugin_edit(
    app: &mut App,
    _area: Rect,
    start: usize,
    stop: usize,
    text: &str,
) -> Result<(), String> {
    let version = app.input_box.buffer.version();
    let edit = TextEdit {
        start,
        stop,
        text: text.to_owned(),
    };
    app.input_box
        .apply_edits(version, vec![edit], Some(stop), None, None)
        .map(|_| ())
}

pub type Rules = Vec<InputRule>;

/// No lua host in a bench, so the rule set is empty: the numbers below are
/// the layout engine's floor, not its ceiling.
pub fn rules() -> Rules {
    Vec::new()
}

pub fn set_rules(app: &mut App, rules: Rules) {
    app.input_box
        .buffer
        .set_rules(RuleSet::with_plugins(&rules));
}

/// Waits for the syntax/decor worker to catch up, so the bench measures
/// rendered input, not queue depth.
pub fn settle_parse(app: &mut App) {
    let deadline = std::time::Instant::now() + SETTLE_TIMEOUT;
    while app.input_box.poll_decor() {
        if std::time::Instant::now() > deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let _ = PathBuf::from(CWD);
}

pub fn set_cursor(app: &mut App, line: usize, x: usize) {
    let _ = (line, x, app);
}

/// The app's per-frame bookkeeping, for benches that measure it.
pub fn tick(app: &mut App) -> bool {
    app.tick_bench()
}

pub fn cwd() -> &'static str {
    CWD
}
