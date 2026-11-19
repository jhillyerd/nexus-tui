use super::code_view;
use super::code_view::RenderLimits;
use super::code_view::SectionFlags;
use super::{DisplayMessage, ToolInput, ToolOutput, ToolStatus};

use crate::animation::spinner_str;
use crate::theme;

use std::borrow::Cow;
use std::fmt::Write;
use std::sync::Arc;
use std::time::Instant;

use unicode_width::UnicodeWidthStr;

use jiff::Timestamp;
use jiff::tz::TimeZone;

use crate::markdown::{should_truncate, text_to_lines, truncate_output, truncation_notice};
use maki_config::ToolOutputLines;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::render_worker::RenderWorker;

pub struct RenderCtx<'a> {
    pub started_at: Instant,
    pub width: u16,
    pub tool_output_lines: &'a ToolOutputLines,
}

pub const TOOL_INDICATOR: &str = "● ";
const RIGHT_INFO_GAP: &str = "  ";
const ASSISTANT_MARKER: &str = TOOL_INDICATOR;
const USER_MARKER: &str = "❯ ";
const BLANK_MARKER: &str = "  ";
const THINKING_LABEL: &str = "thinking> ";

const CODE_OUTPUT_DIVIDER: &str = "────────────";
const ANNOTATION_SEP: &str = " · ";
/// Stands in for a tool message whose role carries no name.
const UNNAMED_TOOL: &str = "?";

pub struct RoleStyle {
    pub marker: &'static str,
    pub marker_style: Style,
    pub label: &'static str,
    pub text_style: Style,
    pub use_markdown: bool,
    pub max_line_bytes: Option<usize>,
}

impl RoleStyle {
    pub fn label_style(&self) -> Style {
        self.marker_style.add_modifier(Modifier::BOLD)
    }

    pub fn gutter(&self) -> Span<'static> {
        Span::styled(self.marker, self.label_style())
    }
}

pub fn assistant_style() -> RoleStyle {
    RoleStyle {
        marker: ASSISTANT_MARKER,
        marker_style: theme::current().assistant_prefix,
        label: "",
        text_style: theme::current().assistant,
        use_markdown: true,
        max_line_bytes: None,
    }
}

pub fn user_style() -> RoleStyle {
    RoleStyle {
        marker: USER_MARKER,
        marker_style: theme::current().user,
        label: "",
        text_style: theme::current().user_text,
        use_markdown: false,
        max_line_bytes: None,
    }
}

pub fn thinking_style() -> RoleStyle {
    RoleStyle {
        marker: ASSISTANT_MARKER,
        marker_style: theme::current().thinking,
        label: THINKING_LABEL,
        text_style: theme::current().thinking,
        use_markdown: true,
        max_line_bytes: None,
    }
}

pub fn error_style() -> RoleStyle {
    RoleStyle {
        marker: BLANK_MARKER,
        marker_style: theme::current().tool_error,
        label: "",
        text_style: theme::current().error,
        use_markdown: false,
        max_line_bytes: None,
    }
}

pub fn notice_style() -> RoleStyle {
    RoleStyle {
        marker: BLANK_MARKER,
        marker_style: theme::current().tool_dim,
        label: "",
        text_style: theme::current().tool_dim,
        use_markdown: false,
        max_line_bytes: None,
    }
}

pub fn done_style() -> RoleStyle {
    RoleStyle {
        marker: BLANK_MARKER,
        marker_style: theme::current().tool_success,
        label: "",
        text_style: theme::current().tool_success.add_modifier(Modifier::BOLD),
        use_markdown: false,
        max_line_bytes: None,
    }
}

pub struct ToolLines {
    pub lines: Vec<Line<'static>>,
    pub highlight: Option<HighlightRequest>,
    pub gutter: Span<'static>,
    pub gutter_spins: bool,
    pub truncation: SectionFlags,
}

pub struct HighlightRequest {
    pub range: (usize, usize),
    pub input: Option<Arc<ToolInput>>,
    pub limits: RenderLimits,
}

impl HighlightRequest {
    fn new(
        range: (usize, usize),
        input: Option<Arc<ToolInput>>,
        limits: RenderLimits,
    ) -> Option<Self> {
        if range.0 == range.1 || input.is_none() {
            return None;
        }
        Some(Self {
            range,
            input,
            limits,
        })
    }
}

impl ToolLines {
    pub fn send_highlight(&self, worker: &RenderWorker) -> Option<u64> {
        let hl = self.highlight.as_ref()?;
        Some(worker.send(hl.input.clone(), hl.limits))
    }
}

pub fn format_timestamp_now(format: maki_config::ClockFormat) -> String {
    let zoned = Timestamp::now().to_zoned(TimeZone::system());
    zoned.strftime(crate::clock::hms(format)).to_string()
}

type RightPart<'a> = Option<(&'a str, Style)>;

fn right_parts_len(parts: &[RightPart]) -> Option<usize> {
    let (count, width) = parts
        .iter()
        .flatten()
        .fold((0, 0), |(count, width), (text, _)| {
            (count + 1, width + UnicodeWidthStr::width(*text))
        });
    (count > 0).then(|| width + RIGHT_INFO_GAP.len() * (count - 1) + 1)
}

fn spans_width(spans: &[Span]) -> usize {
    spans
        .iter()
        .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
        .sum()
}

/// Right-aligns the hint and timestamp on the header line. Whole pieces are
/// dropped from the left when there is no room for all of them, so the header
/// text never moves and nothing wraps.
pub fn append_right_info(
    line: &mut Line<'static>,
    hint: Option<&str>,
    timestamp: Option<&str>,
    width: u16,
) {
    let palette = theme::current();
    let parts: [RightPart; 2] = [
        hint.map(|h| (h, palette.tool_dim)),
        timestamp.map(|t| (t, palette.timestamp)),
    ];
    let header_width = spans_width(&line.spans);
    let w = width as usize;
    let Some((parts, suffix_len)) = (0..parts.len()).find_map(|dropped| {
        let kept = &parts[dropped..];
        let len = right_parts_len(kept)?;
        (header_width + 1 + len <= w).then_some((kept, len))
    }) else {
        return;
    };
    line.spans
        .push(Span::raw(" ".repeat(w - header_width - suffix_len)));
    for (i, (text, style)) in parts.iter().flatten().enumerate() {
        if i > 0 {
            line.spans.push(Span::raw(RIGHT_INFO_GAP));
        }
        line.spans.push(Span::styled((*text).to_owned(), *style));
    }
}

#[derive(Clone, Copy)]
#[allow(dead_code)] // Cancelled stays for the cancel path
enum Indicator {
    InProgress,
    Success,
    Error,
    Cancelled,
}

impl From<ToolStatus> for Indicator {
    fn from(s: ToolStatus) -> Self {
        match s {
            ToolStatus::InProgress => Self::InProgress,
            ToolStatus::Success => Self::Success,
            ToolStatus::Error => Self::Error,
        }
    }
}

struct ResolvedOutput<'a> {
    text: Option<Cow<'a, str>>,
    skipped: usize,
}

fn resolve_output<'a>(
    output: Option<&'a ToolOutput>,
    body: Option<&'a str>,
    pre_truncated: usize,
    limits: RenderLimits,
) -> ResolvedOutput<'a> {
    let full_text: Option<Cow<'a, str>> = output.map(|o| Cow::Borrowed(o.text()));

    let (raw_text, already_truncated): (Option<Cow<'a, str>>, usize) =
        if limits.is_output_expanded() {
            match &full_text {
                Some(t) => (Some(t.clone()), 0),
                None => match body {
                    Some(b) => (Some(Cow::Borrowed(b)), pre_truncated),
                    None => (None, 0),
                },
            }
        } else {
            match (body, &full_text) {
                (Some(b), _) => (Some(Cow::Borrowed(b)), pre_truncated),
                (None, Some(t)) => (Some(t.clone()), 0),
                (None, None) => (None, 0),
            }
        };

    let (text, skipped) = match raw_text {
        Some(t) if !t.is_empty() => {
            let tr = truncate_output(&t, limits.output);
            let s = if tr.skipped > 0 {
                tr.skipped
            } else {
                already_truncated
            };
            (Some(Cow::Owned(tr.kept.into_owned())), s)
        }
        _ => (None, already_truncated),
    };

    ResolvedOutput { text, skipped }
}

struct ToolLineBuilder {
    lines: Vec<Line<'static>>,
    content_range: (usize, usize),
    width: u16,
    truncation: SectionFlags,
    limits: RenderLimits,
    markdown: bool,
    indicator: Indicator,
    input: Option<Arc<ToolInput>>,
}

impl ToolLineBuilder {
    fn new(
        width: u16,
        expanded: SectionFlags,
        max_output_lines: usize,
        indicator: Indicator,
    ) -> Self {
        let limits = RenderLimits::new(expanded, max_output_lines);
        Self {
            lines: Vec::new(),
            content_range: (0, 0),
            width,
            truncation: SectionFlags::default(),
            limits,
            markdown: false,
            indicator,
            input: None,
        }
    }

    fn push_header(&mut self, tool_name: &str, header: &str, annotation: Option<&str>) {
        let mut spans = vec![Span::styled(
            header_prefix(tool_name, header),
            theme::current().tool_prefix,
        )];
        spans.push(Span::styled(header.to_owned(), theme::current().tool));
        if let Some(ann) = annotation {
            spans.push(Span::styled(
                format!(" ({ann})"),
                theme::current().tool_annotation,
            ));
        }
        self.lines.push(Line::from(spans));
    }

    fn push_code_content(&mut self, input: Option<&ToolInput>) {
        let content = code_view::render_tool_content(input, false, self.limits);
        self.truncation.script |= content.truncation.script;
        let start = self.lines.len();
        self.lines.extend(content.lines);
        self.content_range = (start, self.lines.len());
        self.input = input.map(|i| Arc::new(i.clone()));
    }

    fn push_resolved_output(&mut self, resolved: &ResolvedOutput<'_>) {
        if resolved.text.is_none() {
            return;
        }

        if self.content_range.1 > self.content_range.0 {
            self.lines.push(Line::from(Span::styled(
                CODE_OUTPUT_DIVIDER,
                theme::current().tool_dim,
            )));
        }

        if let Some(text) = &resolved.text {
            if self.markdown {
                self.push_markdown_body(text);
            } else {
                push_text_lines(&mut self.lines, text);
            }
            self.push_truncation_count(resolved.skipped);
        }
    }

    fn push_markdown_body(&mut self, text: &str) {
        let style = theme::current().assistant;
        let md_lines = text_to_lines(
            text,
            "",
            style,
            style,
            self.width,
            Some(maki_markdown::render::TOOL_OUTPUT_MAX_LINE_BYTES),
        );
        self.lines.extend(md_lines);
    }

    fn push_truncation_count(&mut self, skipped: usize) {
        if should_truncate(skipped) {
            self.truncation.output = true;
            let text = truncation_notice(skipped);
            self.lines
                .push(Line::from(Span::styled(text, theme::current().tool_dim)));
        }
    }

    fn finish(self, started_at: Instant) -> ToolLines {
        let highlight = HighlightRequest::new(self.content_range, self.input, self.limits);
        let gutter = match self.indicator {
            Indicator::InProgress => Span::styled(
                spinner_str(started_at.elapsed().as_millis()),
                theme::current().spinner,
            ),
            Indicator::Success => Span::styled(TOOL_INDICATOR, theme::current().tool_success),
            Indicator::Error => Span::styled(TOOL_INDICATOR, theme::current().tool_error),
            Indicator::Cancelled => Span::styled(TOOL_INDICATOR, theme::current().tool_dim),
        };
        ToolLines {
            lines: self.lines,
            highlight,
            gutter,
            gutter_spins: matches!(self.indicator, Indicator::InProgress),
            truncation: self.truncation,
        }
    }
}

fn push_text_lines(lines: &mut Vec<Line<'static>>, text: &str) {
    let style = theme::current().tool;
    lines.extend(
        text.lines()
            .map(|line| Line::from(Span::styled(line.to_owned(), style))),
    );
}

pub fn build_tool_lines(
    msg: &DisplayMessage,
    status: ToolStatus,
    rctx: &RenderCtx,
    expanded: SectionFlags,
) -> ToolLines {
    let tool_name = msg.role.tool_name().unwrap_or(UNNAMED_TOOL);
    let (header, body) = match msg.text.split_once('\n') {
        Some((h, b)) => (h, Some(b)),
        None => (msg.text.as_str(), None),
    };

    let mut b = ToolLineBuilder::new(
        rctx.width,
        expanded,
        rctx.tool_output_lines.get(tool_name),
        status.into(),
    );
    if msg
        .tool_output
        .as_deref()
        .is_some_and(ToolOutput::is_markdown)
    {
        b.markdown = true;
    }
    b.push_header(tool_name, header, msg.annotation.as_deref());
    b.push_code_content(msg.tool_input.as_deref());
    let resolved = resolve_output(
        msg.tool_output.as_deref(),
        body,
        msg.truncated_lines,
        b.limits,
    );
    b.push_resolved_output(&resolved);
    b.finish(rctx.started_at)
}

/// What `/` searches for one tool message, built on demand instead of kept
/// beside every rendered segment. It follows the same order
/// [`build_tool_lines`] draws in, except the output is never truncated: a
/// collapsed tool would otherwise hide its own body from search.
pub fn search_text_for(msg: &DisplayMessage) -> String {
    let (header, body) = match msg.text.split_once('\n') {
        Some((h, b)) => (h, Some(b)),
        None => (msg.text.as_str(), None),
    };
    let tool_name = msg.role.tool_name().unwrap_or(UNNAMED_TOOL);
    let mut out = header_search_text(tool_name, header, msg.annotation.as_deref());

    if let Some(ToolInput::Code { code, .. }) = msg.tool_input.as_deref() {
        push_section(&mut out, code.trim_end());
    }
    if let Some(text) = msg.tool_output.as_deref().map(ToolOutput::text).or(body) {
        push_section(&mut out, text);
    }
    out
}

/// A bash call without a description has an empty title, and a trailing
/// space here would sit next to the one the annotation starts with.
fn header_prefix(tool: &str, header: &str) -> String {
    if header.is_empty() {
        format!("{tool}>")
    } else {
        format!("{tool}> ")
    }
}

fn header_search_text(tool: &str, header: &str, annotation: Option<&str>) -> String {
    let prefix = header_prefix(tool, header);
    match annotation {
        Some(ann) => format!("{prefix}{header} ({ann})"),
        None => format!("{prefix}{header}"),
    }
}

fn push_section(out: &mut String, text: &str) {
    out.push('\n');
    out.push_str(text);
}

pub fn truncate_to_header(text: &mut String) {
    text.truncate(header_end(text));
}

#[allow(dead_code)] // restored-session shape; kept while the wire may carry it
pub fn replace_header(text: &mut String, header: &str) {
    text.replace_range(..header_end(text), header);
}

fn header_end(text: &str) -> usize {
    text.find('\n').unwrap_or(text.len())
}

/// Skips a note the row already shows.
#[allow(dead_code)]
pub(crate) fn append_annotation(ann: &mut Option<String>, note: &str) {
    match ann {
        Some(a) if a.split(ANNOTATION_SEP).any(|shown| shown == note) => {}
        Some(a) => write!(a, "{ANNOTATION_SEP}{note}").unwrap(),
        None => *ann = Some(note.to_owned()),
    }
}
