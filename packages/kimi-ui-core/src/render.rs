//! Entry → painted lines: the one place that decides what an entry looks like.
//!
//! Both presentation paths read from here. The headless backend turns these
//! lines into text (projection tests and recorded-stream replay), and a
//! terminal backend mounts them as nodes so `kimi-ui-terminal` can style and
//! diff them. Two renderers would drift on exactly the details a user notices —
//! which kind is dim, how wide the gutter is, where a wrapped line continues.

use crate::entry_id::EntryKind;
use crate::entry_op::EntryStore;
use crate::layout::{Style, width, wrap};

/// Short left-margin label for an entry kind: what the user sees instead of a
/// raw entry id. One character where it can be, so the gutter does not eat the
/// line.
pub fn kind_gutter(kind: EntryKind) -> &'static str {
    match kind {
        EntryKind::UserMsg => ">",
        EntryKind::AssistantMsg => " ",
        EntryKind::Thinking => "~",
        EntryKind::ToolCall => "$",
        EntryKind::Status => "*",
        EntryKind::Banner => "=",
        EntryKind::CompactionBoundary => "-",
    }
}

/// The style an entry is painted in, so the headless renderer and a terminal
/// one agree on which blocks are de-emphasised.
pub fn style_for_kind(kind: EntryKind) -> Style {
    match kind {
        // Reasoning is present but secondary: dim, so a reader can skip it.
        EntryKind::Thinking => Style::DEFAULT.dim(),
        // Status and compaction fences are chrome, not content.
        EntryKind::Status | EntryKind::CompactionBoundary | EntryKind::Banner => {
            Style::DEFAULT.dim()
        }
        _ => Style::DEFAULT,
    }
}

/// The gutter's printable width at any terminal width — constant, because the
/// gutter is one character plus `"| "`.
pub const GUTTER_WIDTH: usize = 2;

/// Every entry laid out at `total_width`, in the store's display order.
///
/// The gutter occupies columns too, so each entry's text wraps in what is left;
/// continuation rows are indented by the gutter width so the margin stays a
/// column of its own.
pub fn entry_lines(store: &EntryStore, total_width: usize) -> Vec<(String, Style)> {
    let mut lines = Vec::new();
    for (_id, content) in store.entries() {
        let gutter = format!("{}| ", kind_gutter(content.kind));
        let gutter_width = width(&gutter);
        let body_width = total_width.saturating_sub(gutter_width).max(1);
        let style = style_for_kind(content.kind);
        for (index, row) in wrap(&content.text, body_width, style)
            .into_iter()
            .enumerate()
        {
            if index == 0 {
                lines.push((format!("{gutter}{}", row.text), style));
            } else {
                lines.push((format!("{}{}", " ".repeat(gutter_width), row.text), style));
            }
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry_id::EntryId;
    use crate::entry_op::{EntryContent, EntryOp};

    fn store_with(entries: &[(EntryKind, &str)]) -> EntryStore {
        let mut store = EntryStore::new();
        for (kind, text) in entries {
            let id = EntryId::engine("t1", 0, *kind);
            store.apply(EntryOp::Upsert {
                id,
                content: EntryContent::new(*kind, *text),
            });
        }
        store
    }

    #[test]
    fn every_entry_gets_a_gutter_that_names_its_kind() {
        let store = store_with(&[
            (EntryKind::UserMsg, "q"),
            (EntryKind::AssistantMsg, "a"),
            (EntryKind::ToolCall, "t"),
        ]);
        let lines: Vec<String> = entry_lines(&store, 40)
            .into_iter()
            .map(|(line, _)| line)
            .collect();
        assert_eq!(lines, vec![">| q", " | a", "$| t"]);
    }

    #[test]
    fn reasoning_and_status_are_dim() {
        let store = store_with(&[(EntryKind::Thinking, "hmm"), (EntryKind::Status, "ok")]);
        for (_, style) in entry_lines(&store, 40) {
            assert_eq!(style, Style::DEFAULT.dim());
        }
    }

    #[test]
    fn a_wrapped_entry_indents_its_continuation_rows_past_the_gutter() {
        let store = store_with(&[(EntryKind::AssistantMsg, "alpha beta gamma delta")]);
        let lines = entry_lines(&store, 12);
        assert!(lines.len() > 1, "the text wraps at twelve columns");
        assert!(lines[0].0.starts_with(" | "));
        for (line, _) in lines.iter().skip(1) {
            assert!(
                line.starts_with("   "),
                "continuation rows align under the body: {line:?}"
            );
            assert!(width(line) <= 12);
        }
    }

    #[test]
    fn the_body_width_accounts_for_the_gutter() {
        let store = store_with(&[(EntryKind::UserMsg, "abcdefghij")]);
        let lines = entry_lines(&store, GUTTER_WIDTH + 4);
        for (line, _) in &lines {
            assert!(width(line) <= GUTTER_WIDTH + 4, "{line:?} overflows");
        }
        assert!(lines.len() >= 3, "ten characters at four per row");
    }
}
