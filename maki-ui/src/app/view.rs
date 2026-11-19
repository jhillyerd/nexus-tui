//! Draws one frame: transcript, input box, status bar, overlays — and
//! returns the caret cell for the IME.

use crate::components::input::BORDER_ROWS;
use crate::components::scrollbar::Thumbs;
use crate::selection::{self, SelectableZone, SelectionZone, ZoneRegistry};
use crate::theme;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::widgets::Block;

use super::mouse::status_motion;
use super::{App, KeyboardOwner};

struct ViewLayout {
    msg_area: Rect,
    input_area: Rect,
    status_area: Rect,
}

impl App {
    /// Returns the cell the terminal cursor belongs on, so an IME draws its
    /// preedit there: the input box's caret, or while an overlay owns the
    /// keyboard, the caret of the field its keys reach. None when that field
    /// wraps, is clipped, or the overlay has no field.
    pub fn view(&mut self, frame: &mut Frame) -> Option<Position> {
        let layout = self.compute_layout(frame.area());
        let owner = self.keyboard_owner();

        let ((caret, overlay_rect), thumbs) = Thumbs::record(|| {
            self.render_background(frame);
            self.render_messages(frame, &layout);
            let caret = self.render_input(frame, &layout);
            self.render_status_bar(frame, layout.status_area);
            let overlay_rect = self.render_overlays(frame, owner, layout.msg_area);
            (caret, overlay_rect)
        });
        self.register_zones(&layout, overlay_rect);
        self.apply_selection(frame, &thumbs);
        match owner {
            KeyboardOwner::Chat => caret,
            KeyboardOwner::Search => None,
            _ => None,
        }
    }

    /// Whether the terminal draws the caret. Only the chat input stopped
    /// painting its own; every other field still paints one.
    pub(crate) fn shows_terminal_cursor(&self) -> bool {
        self.keyboard_owner() == KeyboardOwner::Chat
    }

    /// The shape a plugin set with `maki.ui.set_cursor_shape`, None if no
    /// plugin ever did.
    pub(crate) fn cursor_style(&self) -> Option<maki_lua::CursorStyle> {
        self.keymap_reader.cursor_style()
    }

    fn compute_layout(&self, area: Rect) -> ViewLayout {
        let [content, status_area] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(area);
        let [msg_area, bottom_area] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(self.input_height(content.width)),
        ])
        .areas(content);
        ViewLayout {
            msg_area,
            input_area: bottom_area,
            status_area,
        }
    }

    fn input_height(&self, width: u16) -> u16 {
        self.input_box.height(width).max(BORDER_ROWS)
    }

    fn render_background(&self, frame: &mut Frame) {
        use ratatui::widgets::Widget as _;
        let bg =
            Block::default().style(ratatui::style::Style::new().bg(theme::current().background));
        bg.render(frame.area(), frame.buffer_mut());
    }

    fn render_messages(&mut self, frame: &mut Frame, layout: &ViewLayout) {
        let accent = self.effective_mode_color();
        self.chat.set_accent(accent);
        self.chat.view(
            frame,
            layout.msg_area,
            self.selection_state.is_some(),
            self.command_palette.lua_commands(),
        );
    }

    fn render_input(&mut self, frame: &mut Frame, layout: &ViewLayout) -> Option<Position> {
        if layout.input_area.height == 0 {
            return None;
        }
        let caret = self.input_box.view(
            frame,
            layout.input_area,
            self.input_placeholder(),
            self.separator_style(),
            self.lua_hint_line(),
        );
        self.command_palette
            .view(frame, layout.input_area, &self.input_box.buffer);
        caret
    }

    fn render_status_bar(&mut self, frame: &mut Frame, status_area: Rect) {
        let (mode_label, mode_style) = self.mode_label();
        let loop_status = self.loop_status.clone().map(Into::into);
        let attached = self.attached.clone();
        let ctx = crate::components::status_bar::StatusBarContext {
            status: &self.status,
            motion: status_motion(self.is_working()),
            mode_label,
            mode_style,
            model: &self.model,
            loop_status,
            attached: attached.as_deref(),
            link: self.link,
            auto_scroll: self.chat.auto_scroll(),
        };
        self.status_bar.view(frame, status_area, &ctx);
    }

    /// A later picker is drawn over an earlier one. {overlay_rect} is the
    /// topmost picker's.
    fn render_overlays(&mut self, frame: &mut Frame, owner: KeyboardOwner, msg_area: Rect) -> Rect {
        let mut overlay_rect = Rect::default();
        let full = frame.area();

        if self.search_modal.is_open() {
            let (r, _) = self.search_modal.view(frame, msg_area);
            if r.width > 0 {
                overlay_rect = r;
            }
        }
        if self.theme_picker.is_open() {
            let (r, _) = self.theme_picker.view(frame, full);
            if r.width > 0 {
                overlay_rect = r;
            }
        }
        if self.theme_picker.is_open() && owner == KeyboardOwner::ThemePicker {
            overlay_rect = full;
        }
        let r = self.help_modal.view(frame, full, &self.keymap_reader);
        if r.width > 0 {
            overlay_rect = r;
        }
        let r = self.alert_modal.view(frame, full);
        if r.width > 0 {
            overlay_rect = r;
        }
        overlay_rect
    }

    fn register_zones(&mut self, layout: &ViewLayout, overlay_rect: Rect) {
        // Push order = z-order. zone_at() walks in reverse, so later entries win.
        self.zones = ZoneRegistry::new();

        self.zones.push(SelectableZone {
            area: layout.msg_area,
            zone: SelectionZone::Messages,
        });

        if layout.input_area.height > 0 {
            let input_inner = Rect::new(
                layout.input_area.x,
                layout.input_area.y + 1,
                layout.input_area.width,
                layout.input_area.height.saturating_sub(2),
            );
            self.zones.push(SelectableZone {
                area: input_inner,
                zone: SelectionZone::Input,
            });
        }

        self.zones.push_overlay(layout.status_area);

        if overlay_rect.width > 0 {
            self.zones
                .push_overlay(selection::inset_border(overlay_rect));
        }

        // Overlay zone was removed (e.g. dialog closed), drop the dangling selection
        if let Some(ref state) = self.selection_state
            && state.sel().zone == SelectionZone::Overlay
            && self.zones.find_area(state.sel().area).is_none()
        {
            self.selection_state = None;
        }
    }

    fn apply_selection(&mut self, frame: &mut Frame, thumbs: &Thumbs) {
        let Some(ref state) = self.selection_state else {
            return;
        };

        let sel = state.sel();
        if let Some(screen_sel) = self.screen_selection(sel) {
            selection::apply_highlight(
                frame.buffer_mut(),
                sel.highlight_area(),
                &screen_sel,
                thumbs,
            );
        }
        if state.is_pending_copy() {
            let sel = *sel;
            self.copy_selection(frame.buffer_mut(), &sel, thumbs);
        }
    }

    fn lua_hint_line(&self) -> Option<ratatui::text::Line<'static>> {
        let spans: Vec<_> = self
            .hints
            .get()?
            .visible_in(self.chat_session_id())
            .flat_map(|entry| &entry.spans)
            .map(|(text, style_name)| {
                ratatui::text::Span::styled(text.clone(), theme::style_by_name(style_name))
            })
            .collect();
        (!spans.is_empty()).then(|| ratatui::text::Line::from(spans))
    }
}
