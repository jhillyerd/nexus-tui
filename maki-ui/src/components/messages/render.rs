use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use super::segment::Segment;

pub(super) struct RenderCursor {
    skip: u16,
    y: u16,
    bottom: u16,
    viewport: Rect,
}

impl RenderCursor {
    /// `skip` is the number of rows to drop from the first segment drawn, not
    /// a document offset.
    pub fn new(skip: u16, viewport: Rect) -> Self {
        Self {
            skip,
            y: viewport.y,
            bottom: viewport.y + viewport.height,
            viewport,
        }
    }

    pub fn past_bottom(&self) -> bool {
        self.y >= self.bottom
    }

    fn render(
        &mut self,
        lines: &[Line<'static>],
        h: u16,
        base: Style,
        gutter_cols: u16,
        marker: Option<&Span<'static>>,
        frame: &mut Frame,
    ) {
        if self.skip >= h {
            self.skip -= h;
            return;
        }
        if self.y >= self.bottom {
            return;
        }
        let visible_h = h
            .saturating_sub(self.skip)
            .min(self.bottom.saturating_sub(self.y));
        let gutter_area = Rect::new(self.viewport.x, self.y, gutter_cols, visible_h);
        let seg_area = Rect::new(
            self.viewport.x + gutter_cols,
            self.y,
            self.viewport.width - gutter_cols,
            visible_h,
        );
        if gutter_cols > 0 {
            let marker = Line::from(marker.cloned().unwrap_or_default());
            frame.render_widget(Paragraph::new(marker).style(base), gutter_area);
        }
        let mut p = Paragraph::new(lines.to_vec())
            .wrap(Wrap { trim: false })
            .style(base);
        if self.skip > 0 {
            p = p.scroll((self.skip, 0));
            self.skip = 0;
        }
        frame.render_widget(p, seg_area);
        self.y += visible_h;
    }

    /// A scrolled `Paragraph` wraps every row above the viewport before it
    /// draws anything, so a tall segment used to cost its full height on every
    /// frame. Walking to the first visible row and drawing only what fits
    /// keeps the cost at one screen.
    pub fn render_segment(
        &mut self,
        seg: &Segment,
        style: Option<Style>,
        highlight: bool,
        frame: &mut Frame,
    ) {
        let h = seg.text_height(self.viewport.width);
        if self.skip >= h {
            self.skip -= h;
            return;
        }
        let mut base = style.unwrap_or_default();
        if highlight {
            base = base.add_modifier(Modifier::REVERSED);
        }
        let cols = seg.gutter_cols(self.viewport.width);
        let mut walk = seg.rows_from(self.skip, self.viewport.width);
        while self.y < self.bottom
            && let Some((lines, rows)) = walk.next_chunk(self.bottom - self.y)
        {
            // The first chunk starts at the top of the line `skip` lands in, so
            // only the rows above `skip` inside that line are left to drop.
            // `render` zeroes `skip`, so later chunks drop nothing.
            self.skip = self.skip.saturating_sub(rows.start);
            // The gutter paints from the top of what is visible, so the marker
            // only goes in while the segment's first row is on screen.
            let marker = seg
                .gutter
                .as_ref()
                .filter(|_| rows.start == 0 && self.skip == 0);
            self.render(lines, rows.end - rows.start, base, cols, marker, frame);
        }
    }
}
