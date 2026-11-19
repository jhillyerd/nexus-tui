use std::borrow::Cow;
use std::collections::VecDeque;
use std::env;
use std::path::Path;
use std::sync::mpsc::{Receiver, sync_channel};
use std::time::{Duration, Instant};

use super::Status;

use crate::animation::spinner_frame;
use crate::theme;

use notify::{Config, Error, Event, EventKindMask, RecommendedWatcher, RecursiveMode, Watcher};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::repaint::{Cadence, Dirty};

const TRUNCATE_PREFIX: &str = "..";
const CWD_MODEL_SEPARATOR: &str = "  ";
const OFFLINE_LABEL: &str = " [offline]";
const RECONNECTING_LABEL: &str = " [reconnecting]";

/// How the bar names the connection: attached and routable, or not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Link {
    /// The socket is up; the server accepted the join.
    Live,
    /// Never connected, or the first connect failed before the UI ran.
    Down,
    /// A live connection dropped; retrying with backoff.
    Retrying,
}

/// Everything on the bar that moves with the clock. The view draws from it and
/// the cadence asks for frames from it, so a spinner can't freeze and an idle
/// bar can't keep painting.
pub struct Motion {
    pub working: bool,
}

pub struct StatusBarContext<'a> {
    pub status: &'a Status,
    pub motion: Motion,
    pub mode_label: Cow<'static, str>,
    pub mode_style: Style,
    /// The thread's model, as `hello` announced it.
    pub model: &'a str,
    /// Agent-loop iteration/phase, as `loop_status` frames report.
    pub loop_status: Option<Cow<'static, str>>,
    /// The roster target this node answers to, once `attached` lands.
    pub attached: Option<&'a str>,
    pub link: Link,
    pub auto_scroll: bool,
}

pub struct StatusBar {
    flash: Option<(String, Instant)>,
    /// Messages waiting for the line, oldest first.
    queued: VecDeque<String>,
    started_at: Instant,
    cwd_branch: String,
    pub flash_duration: Duration,
    branch_update_rx: Option<Receiver<()>>,
    /// Nobody reads this. It is here so it dies with the bar, and dropping it
    /// stops notify's event thread and closes the inotify fd.
    _branch_watcher: Option<RecommendedWatcher>,
}

impl StatusBar {
    /// Startup warnings land behind whatever is showing, oldest first.
    #[allow(dead_code)] // boot warnings re-route here when the config loads again
    pub fn push_warnings(&mut self, warnings: &[String]) {
        for warning in warnings {
            self.queue_flash(warning.clone());
        }
    }

    pub fn new(flash_duration: Duration) -> Self {
        let (branch_update_rx, branch_watcher) = watch_branch().unzip();
        Self {
            flash: None,
            queued: VecDeque::new(),
            started_at: Instant::now(),
            cwd_branch: cwd_branch_label(),
            flash_duration,
            branch_update_rx,
            _branch_watcher: branch_watcher,
        }
    }

    /// Shows {msg} now, replacing whatever was showing. It answers something
    /// the user just did, and they want the latest state, not the one before.
    pub fn flash(&mut self, msg: String) {
        self.flash = Some((msg, Instant::now()));
    }

    /// Shows {msg} once the line is free, behind anything already waiting.
    /// Startup warnings come in a batch, and [`Self::flash`] would keep only
    /// the last of them.
    pub fn queue_flash(&mut self, msg: String) {
        if self.flash.is_some() {
            self.queued.push_back(msg);
            return;
        }
        self.flash = Some((msg, Instant::now()));
    }

    #[allow(dead_code)]
    pub fn flash_text(&self) -> Option<&str> {
        self.flash.as_ref().map(|(msg, _)| msg.as_str())
    }

    pub fn refresh_cwd(&mut self) {
        self.cwd_branch = cwd_branch_label();
    }

    pub fn poll_branch_update(&mut self) -> Dirty {
        let Some(rx) = &self.branch_update_rx else {
            return Dirty::NO;
        };
        let fresh = rx.try_recv().is_ok();
        if fresh {
            self.refresh_cwd();
        }
        Dirty::from(fresh)
    }

    #[allow(dead_code)]
    pub fn clear_flash(&mut self) {
        self.flash = None;
    }

    pub fn clear_expired_hint(&mut self) -> Dirty {
        let expired = self
            .flash
            .as_ref()
            .is_some_and(|(_, since)| since.elapsed() >= self.flash_duration);
        if expired {
            self.flash = self.queued.pop_front().map(|m| (m, Instant::now()));
        }
        Dirty::from(expired)
    }

    pub fn cadence(&self, motion: &Motion) -> Cadence {
        if motion.working {
            Cadence::SPINNER
        } else {
            Cadence::IDLE
        }
    }

    pub fn view(&self, frame: &mut Frame, area: Rect, ctx: &StatusBarContext) {
        let mut left_spans = Vec::new();

        if ctx.motion.working {
            let ch = spinner_frame(self.started_at.elapsed().as_millis());
            left_spans.push(Span::styled(format!(" {ch}"), theme::current().spinner));
        }

        left_spans.push(Span::styled(format!(" {}", ctx.mode_label), ctx.mode_style));

        if let Some(loop_status) = &ctx.loop_status {
            left_spans.push(Span::styled(
                format!(" [{loop_status}]"),
                theme::current().status_dim,
            ));
        }

        if !ctx.auto_scroll {
            left_spans.push(Span::styled(
                " auto-scroll paused",
                theme::current().status_dim,
            ));
        }

        if ctx.link != Link::Live {
            left_spans.push(Span::styled(
                match ctx.link {
                    Link::Down => OFFLINE_LABEL,
                    Link::Retrying => RECONNECTING_LABEL,
                    Link::Live => "",
                },
                theme::current().status_retry_error,
            ));
        }

        let mut right_spans = Vec::new();

        // A flash answers what the user just did, while the path and the
        // model sit there all day. So they shrink to make room for it.
        let flash_span = self
            .flash
            .as_ref()
            .map(|(msg, _)| Span::styled(format!(" {msg}"), theme::current().status_notice));

        match ctx.status {
            Status::Error { message: e, .. } => {
                left_spans.push(Span::styled(format!(" {e}"), theme::current().error));
            }
            _ => {
                let mut rest_spans = Vec::new();

                if let Some(target) = ctx.attached {
                    rest_spans.push(Span::styled(
                        format!(" @{target}"),
                        theme::current().status_dim,
                    ));
                }

                let reserved = left_spans
                    .iter()
                    .chain(rest_spans.iter())
                    .chain(flash_span.iter())
                    .map(Span::width)
                    .sum::<usize>()
                    + CWD_MODEL_SEPARATOR.width();
                let available = (area.width as usize).saturating_sub(reserved);
                let model = truncate_tail(ctx.model, available / 2);
                let cwd = truncate_tail(&self.cwd_branch, available.saturating_sub(model.width()));

                right_spans.push(Span::styled(cwd, theme::current().status_dim));
                right_spans.push(Span::raw(CWD_MODEL_SEPARATOR));
                right_spans.push(Span::styled(model, theme::current().status_dim));
                right_spans.append(&mut rest_spans);
            }
        }

        left_spans.extend(flash_span);

        let [left_area, right_area] = Layout::horizontal([
            Constraint::Min(0),
            Constraint::Length(right_spans.iter().map(|s| s.width() as u16).sum()),
        ])
        .areas(area);

        frame.render_widget(Paragraph::new(Line::from(left_spans)), left_area);
        frame.render_widget(
            Paragraph::new(Line::from(right_spans)).alignment(Alignment::Right),
            right_area,
        );
    }
}

fn truncate_tail(s: &str, max_width: usize) -> Cow<'_, str> {
    if s.width() <= max_width {
        return Cow::Borrowed(s);
    }
    let budget = max_width.saturating_sub(TRUNCATE_PREFIX.width());
    let mut used = 0;
    let mut start = s.len();
    for (i, c) in s.char_indices().rev() {
        let w = c.width().unwrap_or(0);
        if used + w > budget {
            break;
        }
        used += w;
        start = i;
    }
    Cow::Owned(format!("{TRUNCATE_PREFIX}{}", &s[start..]))
}

fn collapse_home(path: &str) -> String {
    let Some(home) = maki_storage::paths::home() else {
        return path.to_string();
    };
    collapse_home_with(path, &home.to_string_lossy())
}

fn collapse_home_with(path: &str, home: &str) -> String {
    path.strip_prefix(home)
        .map(|rest| format!("~{rest}"))
        .unwrap_or_else(|| path.to_string())
}

fn cwd_branch_label() -> String {
    let cwd = env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".into());
    let label = collapse_home(&cwd);
    match detect_branch(&cwd) {
        Some(branch) => format!("{label}:{branch}"),
        None => label,
    }
}

fn detect_branch(cwd: &str) -> Option<String> {
    let head = std::fs::read_to_string(find_git_dir(Path::new(cwd))?.join("HEAD")).ok()?;
    let head = head.trim();
    head.strip_prefix("ref: refs/heads/")
        .map(str::to_string)
        .or_else(|| Some(head.get(..7)?.to_string()))
}

fn find_git_dir(cwd: &Path) -> Option<std::path::PathBuf> {
    let mut dir = cwd;
    loop {
        let git = dir.join(".git");
        if git.is_dir() {
            return Some(git);
        }
        if let Ok(contents) = std::fs::read_to_string(&git) {
            let path = contents.trim().strip_prefix("gitdir: ")?.trim_start();
            let path = Path::new(path);
            return Some(if path.is_absolute() {
                path.to_path_buf()
            } else {
                dir.join(path)
            });
        }
        dir = dir.parent()?;
    }
}

fn watch_branch() -> Option<(Receiver<()>, RecommendedWatcher)> {
    let cwd = env::current_dir().ok()?;
    let head = find_git_dir(&cwd)?.join("HEAD");
    let (tx, rx) = sync_channel(1);
    let mut watcher = RecommendedWatcher::new(
        move |result: std::result::Result<Event, Error>| {
            if result.is_ok() {
                let _ = tx.send(());
            }
        },
        Config::default().with_event_kinds(EventKindMask::CORE),
    )
    .ok()?;
    watcher.watch(&head, RecursiveMode::NonRecursive).ok()?;
    Some((rx, watcher))
}
