//! Tool chip body rendering: the call's input as a code block, and its
//! result as plain text with a line cap.

use crate::highlight::{fallback_span, highlight_line};
use crate::markdown::{should_truncate, truncation_notice};
use crate::theme;
use ratatui::text::{Line, Span};

use super::ToolInput;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SectionFlags {
    pub script: bool,
    pub output: bool,
}

impl SectionFlags {
    pub fn any(self) -> bool {
        self.script || self.output
    }
}

#[derive(Clone, Copy)]
pub struct RenderLimits {
    pub script: usize,
    pub output: usize,
}

impl RenderLimits {
    pub fn new(expanded: SectionFlags, output_limit: usize) -> Self {
        Self {
            script: if expanded.script {
                usize::MAX
            } else {
                output_limit
            },
            output: if expanded.output {
                usize::MAX
            } else {
                output_limit
            },
        }
    }

    pub fn is_output_expanded(self) -> bool {
        self.output == usize::MAX
    }
}

pub struct ToolContent {
    pub lines: Vec<Line<'static>>,
    pub truncation: SectionFlags,
}

pub fn render_tool_content(
    input: Option<&ToolInput>,
    highlight: bool,
    limits: RenderLimits,
) -> ToolContent {
    let mut lines = Vec::new();
    let mut truncation = SectionFlags::default();
    if let Some(ToolInput::Code { language, code }) = input {
        let code_lines: Vec<String> = code
            .trim_end_matches('\n')
            .lines()
            .map(String::from)
            .collect();
        let total = code_lines.len();
        let hl = highlight.then(|| maki_highlight::Highlighter::for_token(language));
        let (code_result, trunc) = render_code(hl, 1, &code_lines, total, limits.script);
        truncation.script = trunc;
        lines.extend(code_result);
    }
    ToolContent { lines, truncation }
}

fn nr_width(max_nr: usize) -> usize {
    max_nr.max(1).ilog10() as usize + 1
}

fn gutter(nr_str: &str) -> Span<'static> {
    Span::styled(format!("{nr_str} "), theme::current().diff_line_nr)
}

fn truncation_line(truncated: usize) -> Line<'static> {
    Line::from(Span::styled(
        truncation_notice(truncated),
        theme::current().tool_dim,
    ))
}

fn highlight_spans(hl: &mut maki_highlight::Highlighter, text: &str) -> Vec<Span<'static>> {
    let with_nl = format!("{text}\n");
    highlight_line(hl, &with_nl)
        .into_iter()
        .filter(|span| !span.content.is_empty())
        .collect()
}

fn render_code(
    mut hl: Option<maki_highlight::Highlighter>,
    start_line: usize,
    code_lines: &[String],
    total_count: usize,
    max_lines: usize,
) -> (Vec<Line<'static>>, bool) {
    let capped = code_lines.len().min(max_lines);
    let hidden = total_count.saturating_sub(capped);
    let has_truncation = should_truncate(hidden);
    let display_count = if has_truncation {
        capped
    } else {
        code_lines.len()
    };
    let max_nr = start_line + display_count.saturating_sub(1);
    let w = nr_width(max_nr);

    let mut lines: Vec<Line<'static>> = code_lines
        .iter()
        .take(display_count)
        .enumerate()
        .map(|(i, text)| {
            let nr = start_line + i;
            let mut spans = vec![gutter(&format!("{nr:>w$}"))];
            match &mut hl {
                Some(h) => spans.extend(highlight_spans(h, text)),
                None => spans.push(fallback_span(text)),
            }
            Line::from(spans)
        })
        .collect();

    if has_truncation {
        lines.push(truncation_line(hidden));
    }
    (lines, has_truncation)
}
