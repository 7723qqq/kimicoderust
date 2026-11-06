//! The transcript widget: the entry table painted as terminal rows.
//!
//! What an entry looks like is not decided here. `kimi_ui_core::render` lays the
//! store out into `(text, style)` rows — gutter, wrap width, per-kind dimming —
//! and this module only turns those rows into buffer cells. Keeping the policy
//! there is what let the previous backend and its tests survive a change of
//! renderer; the alternative is two renderers drifting on the details a reader
//! notices first.

use std::sync::Arc;

use kimi_ui_core::layout::{Color, Style};
use kimi_ui_core::render::entry_lines;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color as RatColor, Modifier, Style as RatStyle};
use textual_rs::Widget;
use textual_rs::widget::context::AppContext;

use crate::app::UiState;

/// The scrolled-to-bottom view of the entry table.
pub struct Transcript {
    state: Arc<UiState>,
}

impl Transcript {
    pub fn new(state: Arc<UiState>) -> Self {
        Self { state }
    }
}

impl Widget for Transcript {
    fn widget_type_name(&self) -> &'static str {
        "Transcript"
    }

    fn render(&self, _ctx: &AppContext, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let width = area.width as usize;
        let lines = self.state.with_store(|store| entry_lines(store, width));
        // The tail, not the head: a turn streams downward and the newest row is
        // the one the user is watching.
        let skip = lines.len().saturating_sub(area.height as usize);
        for (row, (text, style)) in lines[skip..].iter().enumerate() {
            let y = area.y + row as u16;
            buf.set_stringn(area.x, y, text, width, to_ratatui(*style));
        }
    }
}

/// Our style vocabulary → ratatui's. The two carry the same information; this
/// is the only place that says so.
fn to_ratatui(style: Style) -> RatStyle {
    let mut out = RatStyle::default();
    out = match style.fg {
        Color::Default => out,
        Color::Ansi(index) => out.fg(RatColor::Indexed(index)),
        Color::Rgb { r, g, b } => out.fg(RatColor::Rgb(r, g, b)),
    };
    for (set, modifier) in [
        (style.attributes.bold, Modifier::BOLD),
        (style.attributes.dim, Modifier::DIM),
        (style.attributes.italic, Modifier::ITALIC),
        (style.attributes.underline, Modifier::UNDERLINED),
        (style.attributes.reversed, Modifier::REVERSED),
    ] {
        if set {
            out = out.add_modifier(modifier);
        }
    }
    out
}
