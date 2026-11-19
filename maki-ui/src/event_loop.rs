//! Single-threaded ratatui event loop for one node connection. Terminal
//! input, node frames and the repaint clock feed one [`App`]; the loop
//! paints, ticks, and ends with the exit-code contract (0 clean, 2 connect
//! failure, 4 credential rejected).

use std::time::Duration;

use color_eyre::Result;
use crossterm::event::Event;
use ratatui::layout::Rect;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::app::{App, Msg};
use crate::appearance;
use crate::color_compat;
use crate::components::Action;
use crate::input::InputReader;
use crate::node::client::ClientEvent;
use crate::node::protocol;
use crate::present::present;
use crate::repaint::Dirty;
use crate::terminal::TerminalGuard;

/// How long the loop waits for events when everything is idle: the slowest
/// animation cadence plus a little slack.
const IDLE_POLL: Duration = Duration::from_millis(100);
/// Frames drained per wake beyond the first, so a burst never starves input.
const NODE_DRAIN_BUDGET: usize = 64;
/// What the ignored lua requests answer with.
const NO_AGENT_REPLY: &str = "no agent behind this client";

pub struct EventLoopParams {
    pub events: UnboundedReceiver<ClientEvent>,
    /// The lua host's UI requests (flashes, keybind completions, window
    /// views). `None` when the host never hands one out.
    pub ui_actions: Option<UnboundedReceiver<maki_lua::UiAction>>,
    pub app: App,
    pub max_log_bytes: u64,
    pub ui_config: maki_config::UiConfig,
}

/// Why the UI run ended: the exit code the process should exit with.
pub enum RunOutcome {
    Exit { code: i32 },
}

pub async fn run(params: EventLoopParams) -> Result<RunOutcome> {
    let (_guard, mut terminal) = TerminalGuard::init(params.max_log_bytes)?;
    color_compat::init();
    appearance::init(params.ui_config.theme.as_ref());
    let mut loop_ = Loop {
        last_area: Rect::default(),
        input: InputReader::new()?,
        node_rx: params.events,
        ui_rx: params.ui_actions,
        app: params.app,
        last_mode: maki_lua::InputMode::Insert,
    };
    loop_.run(&mut terminal).await
}

struct Loop {
    last_area: Rect,
    input: InputReader,
    node_rx: UnboundedReceiver<ClientEvent>,
    ui_rx: Option<UnboundedReceiver<maki_lua::UiAction>>,
    app: App,
    last_mode: maki_lua::InputMode,
}

impl Loop {
    async fn run(&mut self, terminal: &mut ratatui::DefaultTerminal) -> Result<RunOutcome> {
        let mut dirty = Dirty::YES;
        loop {
            if dirty == Dirty::YES {
                self.paint(terminal)?;
                dirty = Dirty::NO;
            }

            match self.wait().await {
                Some(event) => match event {
                    Wait::Input(event) => {
                        dirty |= self.handle_input(event);
                    }
                    Wait::Node(event) => {
                        dirty |= self.handle_node(event);
                    }
                    Wait::Ui(action) => {
                        let area = self.last_area;
                        self.handle_ui_action(action, area);
                        dirty = Dirty::YES;
                    }
                },
                None => {
                    // Idle tick: animations, flash expiry, worker drains.
                    dirty |= self.app.tick();
                }
            }

            dirty |= self.drain_node();
            dirty |= self.drain_input();

            let actions = std::mem::take(&mut self.app.pending_actions);
            for action in actions {
                if let Some(code) = self.run_action(action, terminal) {
                    return Ok(RunOutcome::Exit { code });
                }
            }

            if self.app.exit_request != crate::components::ExitRequest::None {
                return Ok(RunOutcome::Exit {
                    code: self.app.exit_request.code(),
                });
            }

            // The mode is announced to lua once per change.
            self.app.announce_mode(&mut self.last_mode);
            dirty |= self.app.tick_typeahead();
        }
    }

    async fn wait(&mut self) -> Option<Wait> {
        let wait = self.app.cadence().frame().unwrap_or(IDLE_POLL);
        // The input reader's queue is checked first and after every wait: it
        // is a plain channel, and keys must never queue behind node frames.
        if let Some(event) = self.input.try_next() {
            return Some(Wait::Input(event));
        }
        let node = &mut self.node_rx;
        let ui = self.ui_rx.as_mut();
        tokio::select! {
            biased;
            event = node.recv() => event.map(Wait::Node),
            action = async {
                match ui {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => action.map(Wait::Ui),
            () = tokio::time::sleep(wait) => None,
        }
    }

    fn handle_input(&mut self, event: Event) -> Dirty {
        match event {
            // Presses only: the enhancement flags make held keys stream
            // Release/Repeat, and a key that answers twice is wrong however
            // it lands.
            Event::Key(key) if key.kind == crossterm::event::KeyEventKind::Press => {
                let actions = self.app.update(Msg::Key(key));
                self.pending(actions);
                Dirty::YES
            }
            Event::Key(_) => Dirty::NO,
            Event::Paste(text) => {
                let actions = self.app.update(Msg::Paste(text));
                self.pending(actions);
                Dirty::YES
            }
            Event::Mouse(mouse) => {
                let _ = self.app.update(Msg::Mouse(mouse));
                Dirty::YES
            }
            Event::Resize(..) => Dirty::YES,
            Event::FocusGained | Event::FocusLost => Dirty::NO,
        }
    }

    fn drain_input(&mut self) -> Dirty {
        let mut dirty = Dirty::NO;
        while let Some(event) = self.input.try_next() {
            dirty |= self.handle_input(event);
        }
        dirty |= self.app.poll_mouse_paste();
        dirty
    }

    fn handle_node(&mut self, event: ClientEvent) -> Dirty {
        match event {
            ClientEvent::Connected => {
                self.app
                    .handle_node_event(crate::node::NodeEvent::Connected);
                Dirty::YES
            }
            ClientEvent::Reconnecting { attempt, after } => {
                self.app
                    .handle_node_event(crate::node::NodeEvent::Reconnecting { attempt, after });
                Dirty::YES
            }
            ClientEvent::Frame(frame) => self.handle_frame(frame),
            ClientEvent::Terminated(termination) => {
                self.app
                    .handle_node_event(crate::node::NodeEvent::Terminated(termination));
                Dirty::YES
            }
        }
    }

    /// The lua host's requests. Ones the client cannot serve answer closed
    /// or refused, so a plugin never waits forever.
    fn handle_ui_action(&mut self, action: maki_lua::UiAction, frame_area: Rect) {
        use maki_lua::UiAction;
        match action {
            UiAction::Flash(msg) => self.app.flash(msg),
            UiAction::OpenEditor { path, reply_tx } => {
                let _ = reply_tx.send(0);
                let _ = open::that_detached(&path);
            }
            UiAction::Builtin {
                action, reply_tx, ..
            } => {
                self.app.run_builtin(action);
                let _ = reply_tx.send(true);
            }
            UiAction::KeybindDone(ticket) => {
                let actions = self.app.keybind_done(ticket);
                self.app.pending_actions.extend(actions);
            }
            UiAction::KeybindFallback(ticket) => {
                let actions = self.app.keybind_fallback(ticket);
                self.app.pending_actions.extend(actions);
            }
            UiAction::PluginUnloaded(plugin) => {
                self.app.clear_plugin_marks(&plugin);
            }
            UiAction::RunCommand {
                cmdline,
                caller,
                reply_tx,
                ..
            } => {
                let result = self.app.run_cmdline(&cmdline, &caller).map(|_| ());
                let _ = reply_tx.send(result);
            }
            UiAction::WinSaveView { reply_tx } => {
                let _ = reply_tx.send(self.app.chat.win_view());
            }
            UiAction::WinRestView { scroll_top } => {
                self.app.chat.scroll_to_row(scroll_top);
            }
            // Session, model, plan, task and input requests belong to the
            // agent plane; the client has no agent to ask.
            UiAction::OpenWin { .. } => {}
            UiAction::SetWindowTitle(_) => {}
            UiAction::Input { req, reply_tx } => {
                let reply = match req {
                    maki_lua::InputRequest::Read => Ok(serde_json::json!({
                        "text": self.app.input_box.buffer.value(),
                        "cursor": self.app.input_box.buffer.cursor_byte(),
                        "anchor": self.app.input_box.buffer.anchor(),
                        "version": self.app.input_box.buffer.version(),
                    })),
                    maki_lua::InputRequest::Edit(edit) => self
                        .app
                        .apply_input_edit(edit, frame_area)
                        .map(|v| serde_json::json!(v)),
                    maki_lua::InputRequest::Decorate(decorate) => self
                        .app
                        .decorate_input(decorate)
                        .map(|v| serde_json::json!(v)),
                    maki_lua::InputRequest::Submit(submit) => {
                        // A plugin's submit sends what is in the box, while
                        // it still holds the version the plugin saw.
                        if submit.version != self.app.input_box.buffer.version() {
                            Err("the input moved on".to_owned())
                        } else {
                            let text = self.app.input_box.buffer.value().to_owned();
                            let sent = self.app.submit_plugin_text(&text);
                            if sent {
                                self.app.input_box.discard();
                            }
                            Ok(serde_json::json!({ "sent": sent }))
                        }
                    }
                };
                let _ = reply_tx.send(reply);
            }
            UiAction::Session { reply_tx, .. }
            | UiAction::Model { reply_tx, .. }
            | UiAction::Plan { reply_tx, .. }
            | UiAction::Task { reply_tx, .. } => {
                let _ = reply_tx.send(Err(NO_AGENT_REPLY.to_owned()));
            }
            UiAction::ExportSession { reply_tx, .. } => {
                let _ = reply_tx.send(Err(NO_AGENT_REPLY.to_owned()));
            }
            UiAction::PluginTaskEvent { .. }
            | UiAction::PluginTaskOpened { .. }
            | UiAction::PluginTaskEnded { .. } => {
                // Subagent task tabs belong to the agent plane; nothing
                // opens one here.
            }
        }
    }

    fn handle_frame(&mut self, frame: protocol::ServerFrame) -> Dirty {
        let Some(event) = crate::node::NodeEvent::project(frame) else {
            return Dirty::NO;
        };
        self.app.handle_node_event(event);
        Dirty::YES
    }

    fn drain_node(&mut self) -> Dirty {
        let mut dirty = Dirty::NO;
        for _ in 0..NODE_DRAIN_BUDGET {
            match self.node_rx.try_recv() {
                Ok(event) => dirty |= self.handle_node(event),
                Err(_) => break,
            }
        }
        dirty
    }

    fn pending(&mut self, actions: Vec<Action>) {
        self.app.pending_actions.extend(actions);
    }

    fn run_action(
        &mut self,
        action: Action,
        terminal: &mut ratatui::DefaultTerminal,
    ) -> Option<i32> {
        match action {
            Action::ManualExit => Some(self.app.exit_request.code()),
            Action::ShellCommand {
                id: _,
                command,
                visible: _,
            } => {
                // A `!` line runs in a normal shell; output goes to the
                // terminal it came from.
                let _ = std::process::Command::new("sh")
                    .arg("-c")
                    .arg(&command)
                    .status();
                None
            }
            Action::OpenEditor(path) => {
                let _ = open::that_detached(&path);
                None
            }
            Action::OpenUrl(url) => {
                let _ = open::that(&url);
                None
            }
            Action::EditInputInEditor => {
                self.edit_input_in_editor(terminal);
                None
            }
            Action::Suspend => {
                // Suspension parks the terminal; the next paint catches up.
                None
            }
        }
    }

    /// Edits the draft in `$VISUAL`/`$EDITOR`, paused terminal and all.
    fn edit_input_in_editor(&mut self, terminal: &mut ratatui::DefaultTerminal) {
        let app = &mut self.app;
        let draft = tempfile::NamedTempFile::new().and_then(|mut f| {
            std::io::Write::write_all(&mut f, app.input_box.buffer.value().as_bytes())?;
            Ok(f.into_temp_path())
        });
        let Ok(path) = draft else {
            app.flash("no temp file for the editor".into());
            return;
        };
        let _pause = self.input.pause();
        match crate::terminal::open_in_editor(&path, terminal) {
            Ok(code) if code != 0 => app.flash(format!("editor exited with {code}")),
            Ok(_) => {
                if let Ok(text) = std::fs::read_to_string(&path) {
                    app.input_box.buffer.set_value(&text);
                    app.note_input_changed(true);
                }
            }
            Err(e) => app.flash(e),
        }
    }

    fn paint(&mut self, terminal: &mut ratatui::DefaultTerminal) -> Result<()> {
        self.last_area = terminal.size().unwrap_or_default().into();
        let app = &mut self.app;
        present(terminal, |frame| {
            let position = app.view(frame);
            color_compat::downgrade_if_needed(frame.buffer_mut());
            position.map(|position| crate::present::Caret {
                position,
                shown: app.shows_terminal_cursor(),
                style: app.cursor_style().map(crate::terminal::cursor_style),
            })
        })?;
        Ok(())
    }
}

enum Wait {
    Input(Event),
    Node(ClientEvent),
    Ui(maki_lua::UiAction),
}
