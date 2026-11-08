//! The one-line status row between the transcript and the prompt.
//!
//! The message itself is composed elsewhere — the host decides what happened and
//! localizes the sentence — so this widget only owns where it sits and how it is
//! drawn. It is dim and single-line on purpose: a stopped turn must not compete
//! with the transcript, and nothing here wraps or scrolls.
//!
//! The row is reserved whether or not there is a message, which is what a status
//! bar is: a fixed strip that is sometimes empty, rather than a line that appears
//! and shifts everything under it.

use std::sync::Arc;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style as RatStyle};
use textual_rs::Widget;
use textual_rs::widget::context::AppContext;

use crate::app::UiState;

pub struct StatusLine {
    state: Arc<UiState>,
}

impl StatusLine {
    pub fn new(state: Arc<UiState>) -> Self {
        Self { state }
    }
}

impl Widget for StatusLine {
    fn widget_type_name(&self) -> &'static str {
        "StatusLine"
    }

    fn render(&self, _ctx: &AppContext, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let text = if let Some(approval) = self.state.pending_approval() {
            format!(
                " Approval: {} ({}) | y: allow, n: deny, a: allow for session",
                approval.tool_name, approval.action
            )
        } else if let Some(question) = self.state.pending_question() {
            let first = question.questions.first().map(String::as_str).unwrap_or("");
            let opts: Vec<String> = question.options.first().cloned().unwrap_or_default();
            let mut s = format!(" Question: {}", first);
            for (i, o) in opts.iter().enumerate() {
                s.push_str(&format!(" [{}]{}", i + 1, o));
            }
            s.push_str(" | Enter confirm, Esc dismiss");
            s
        } else {
            match self.state.status() {
                Some(status) => format!(" {status}"),
                None => return,
            }
        };
        buf.set_stringn(
            area.x,
            area.y,
            text,
            area.width as usize,
            RatStyle::default().add_modifier(Modifier::DIM),
        );
    }
}
