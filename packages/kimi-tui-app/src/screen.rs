//! The root screen: transcript above, prompt below.
//!
//! textual-rs mounts this as the root widget and lays its children out with the
//! stylesheet below. The screen also carries the key fallback — when nothing is
//! focused, the framework dispatches a key to the screen root instead of to a
//! child, and typing has to work in that state too.

use std::any::Any;
use std::sync::Arc;

use textual_rs::Widget;
use textual_rs::widget::EventPropagation;
use textual_rs::widget::context::AppContext;

use crate::app::UiState;
use crate::prompt::Prompt;
use crate::status::StatusLine;
use crate::transcript::Transcript;

pub struct AppScreen {
    state: Arc<UiState>,
}

impl AppScreen {
    /// The transcript takes every row the prompt and the status line leave. One
    /// row each for the prompt and the status keeps the slice's shape; a
    /// multi-line editor and a multi-line status grow these later.
    pub const CSS: &'static str = "
        AppScreen { layout-direction: vertical; }
        Transcript { flex-grow: 1; }
        StatusLine { height: 1; }
        Prompt { height: 1; }
    ";

    pub fn new(state: Arc<UiState>) -> Self {
        Self { state }
    }
}

impl Widget for AppScreen {
    fn widget_type_name(&self) -> &'static str {
        "AppScreen"
    }

    /// A container paints nothing itself: the transcript and the prompt below
    /// cover the whole area the layout gives them.
    fn render(
        &self,
        _ctx: &AppContext,
        _area: ratatui::layout::Rect,
        _buf: &mut ratatui::buffer::Buffer,
    ) {
    }

    fn compose(&self) -> Vec<Box<dyn Widget>> {
        vec![
            Box::new(Transcript::new(Arc::clone(&self.state))),
            Box::new(StatusLine::new(Arc::clone(&self.state))),
            Box::new(Prompt::new(Arc::clone(&self.state))),
        ]
    }

    fn on_event(&self, event: &dyn Any, _ctx: &AppContext) -> EventPropagation {
        Prompt::dispatch(&self.state, event)
    }
}
