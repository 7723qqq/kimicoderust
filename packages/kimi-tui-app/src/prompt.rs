//! The prompt line, one row below the transcript.
//!
//! The widget owns no text of its own: the line being typed lives in
//! [`UiState`], because the host reads it for the same reason it reads the
//! entry table — the frame is redrawn from shared state, not from widget
//! fields.

use std::any::Any;
use std::sync::Arc;

use crossterm::event::{Event, KeyEvent};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use textual_rs::Widget;
use textual_rs::widget::EventPropagation;
use textual_rs::widget::context::AppContext;

use crate::app::UiState;
use crate::input::to_input_event;

pub struct Prompt {
    state: Arc<UiState>,
}

impl Prompt {
    pub fn new(state: Arc<UiState>) -> Self {
        Self { state }
    }

    /// Route one terminal key into the shared state.
    ///
    /// Both this widget and the screen call it: textual-rs dispatches a key to
    /// the focused widget and falls back to the screen root when nothing is
    /// focused, and the slice must accept typing in either case.
    pub fn dispatch(state: &UiState, event: &dyn Any) -> EventPropagation {
        let Some(key) = event.downcast_ref::<KeyEvent>() else {
            return EventPropagation::Continue;
        };
        let Some(input) = to_input_event(Event::Key(*key)) else {
            return EventPropagation::Continue;
        };
        if state.handle_input(input) {
            EventPropagation::Stop
        } else {
            EventPropagation::Continue
        }
    }
}

impl Widget for Prompt {
    fn widget_type_name(&self) -> &'static str {
        "Prompt"
    }

    fn can_focus(&self) -> bool {
        true
    }

    fn on_event(&self, event: &dyn Any, _ctx: &AppContext) -> EventPropagation {
        Self::dispatch(&self.state, event)
    }

    fn render(&self, _ctx: &AppContext, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let line = format!("> {}", self.state.prompt());
        buf.set_stringn(area.x, area.y, &line, area.width as usize, Style::default());
    }
}
