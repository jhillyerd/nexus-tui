//! The app's half of the node connection: lifecycle, status-bar data, and
//! the exit-code contract.

use super::App;
use crate::components::status_bar::Link;
use crate::components::{Action, ExitRequest, Status};
use crate::node::NodeEvent;
use nexus::client::Termination;

impl App {
    /// One projected frame landed. The transcript takes what it renders;
    /// the bar and the connection state take the rest.
    pub(crate) fn handle_node_event(&mut self, event: NodeEvent) {
        match event {
            NodeEvent::Connected => {
                self.link = Link::Live;
                self.status = Status::Idle;
                self.chat
                    .handle_node_event(event, self.ui_config.clock_format);
            }
            NodeEvent::Reconnecting { attempt, .. } => {
                self.link = Link::Retrying;
                self.status = Status::Idle;
                self.chat.stream_reset();
                self.flash(format!("connection lost \u{2014} retrying (#{attempt})"));
            }
            NodeEvent::Hello { model, .. } => {
                self.announced_model = Some(model.clone());
                self.model = model;
            }
            NodeEvent::TurnDone { stats } => {
                self.status = Status::Idle;
                self.chat
                    .handle_node_event(NodeEvent::TurnDone { stats }, self.ui_config.clock_format);
            }
            NodeEvent::TurnStopped { chars } => {
                self.status = Status::Idle;
                self.flash(format!("turn stopped after {chars} chars"));
                self.chat.handle_node_event(
                    NodeEvent::TurnStopped { chars },
                    self.ui_config.clock_format,
                );
            }
            NodeEvent::TurnFailed { reason } => {
                self.status = Status::error(reason.clone());
                self.chat.handle_node_event(
                    NodeEvent::TurnFailed { reason },
                    self.ui_config.clock_format,
                );
            }
            NodeEvent::LoopStatus {
                iteration,
                max,
                phase,
            } => {
                self.loop_status = Some(format!("loop {iteration}/{max} {phase}"));
            }
            NodeEvent::Meta { title, .. } => {
                if let Some(title) = title {
                    self.chat.set_name(title);
                }
            }
            NodeEvent::Attached { target } => {
                self.attached = Some(target);
            }
            NodeEvent::Terminated(termination) => self.terminate(termination),
            // Streaming and committed rows: the transcript's data. A delta
            // means a turn is live; committed rows say nothing on their own
            // (a replay arrives while idle).
            event => {
                if matches!(
                    event,
                    NodeEvent::Token { .. }
                        | NodeEvent::ReasoningDelta { .. }
                        | NodeEvent::ToolCall { .. }
                        | NodeEvent::ToolCallDelta { .. }
                ) {
                    self.status = Status::Streaming;
                }
                self.chat
                    .handle_node_event(event, self.ui_config.clock_format);
            }
        }
    }

    /// The connection loop ended; the process exits with its code.
    fn terminate(&mut self, termination: Termination) {
        self.link = Link::Down;
        match termination {
            Termination::Shutdown => {
                self.exit_request = ExitRequest::Success;
            }
            Termination::CredentialRejected => {
                self.status = Status::error("credential rejected — pair again".to_owned());
                self.alert_modal
                    .open("credential rejected — pair again".to_owned());
                self.exit_request = ExitRequest::Error;
                self.pending_actions.push(Action::ManualExit);
            }
            Termination::ConnectFailed(detail) => {
                self.status = Status::error(detail.clone());
                self.exit_request = ExitRequest::Error;
                self.pending_actions.push(Action::ManualExit);
            }
        }
    }
}
