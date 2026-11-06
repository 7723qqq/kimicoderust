//! Headless render backend: the entry table rendered to a plain-text line
//! buffer. Drives projection tests and recorded-stream replay without a
//! terminal.
//!
//! Deliberately out of scope here — render-layer concerns, noted so the seam
//! is explicit and stays one place:
//!
//! - **Parse cache**: `snapshot()` re-walks content every call. A real backend
//!   caches parsed lines per `(EntryId, content generation)` so a large tool
//!   result is parsed once, not per frame.
//! - **Viewport virtualization**: the whole table is materialized. A real
//!   backend renders only the visible window; large payloads stay referenced
//!   by [`EntryId`] and are never copied — the store already holds exactly one
//!   allocation per entry, moved in at projection time.

use crate::entry_id::EntryKind;
use crate::entry_op::{EntryOp, EntryStore};
use crate::layout::{Style, width, wrap};
use crate::render_backend::{
    BackendState, CapabilitySet, ComponentNode, Frame, InputEvent, NodeId, RenderBackend,
    StopOptions,
};
use std::io;

/// A backend with no terminal: applies [`EntryOp`]s to an [`EntryStore`] and
/// renders deterministic plain text.
pub struct HeadlessBackend {
    store: EntryStore,
    /// The same backend state the two product backends keep, so mounting,
    /// cascade unmount, focus and input queuing behave identically here. Only
    /// the rendering differs: this one answers with the entry table.
    state: BackendState,
}

impl HeadlessBackend {
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply one projection op.
    pub fn apply(&mut self, op: EntryOp) {
        self.store.apply(op);
    }

    /// Apply a batch of ops in order.
    pub fn apply_all(&mut self, ops: impl IntoIterator<Item = EntryOp>) {
        for op in ops {
            self.store.apply(op);
        }
    }

    /// The underlying entry table (violations, seal state, ...).
    pub fn store(&self) -> &EntryStore {
        &self.store
    }

    /// Deterministic plain-text rendering: one `== <entry-id>` header per
    /// entry (sorted id order) followed by its content verbatim.
    pub fn snapshot(&self) -> String {
        let mut out = String::new();
        for (id, content) in self.store.entries() {
            out.push_str("== ");
            out.push_str(&id.to_string());
            out.push('\n');
            out.push_str(&content.text);
            if !content.text.ends_with('\n') {
                out.push('\n');
            }
        }
        out
    }

    /// [`Self::snapshot`] as lines, for per-line assertions.
    pub fn render_lines(&self) -> Vec<String> {
        self.snapshot().lines().map(str::to_owned).collect()
    }

    /// Render the entry table the way a terminal backend would: each entry
    /// laid out at `width`, in the store's display order, with a gutter that
    /// names the entry's kind.
    ///
    /// This is the end-to-end path a real backend takes — entry → layout →
    /// painted rows — minus the terminal. `snapshot()` above dumps the table
    /// verbatim and is what the projection tests assert on; this is what a
    /// slice test asserts on when the question is "what would the user see".
    pub fn paint(&self, total_width: usize) -> Vec<String> {
        let mut lines = Vec::new();
        for (_id, content) in self.store.entries() {
            let gutter = format!("{}| ", kind_gutter(content.kind));
            // The gutter occupies columns too; the text wraps in what is left.
            let gutter_width = width(&gutter);
            let body_width = total_width.saturating_sub(gutter_width).max(1);
            let style = style_for_kind(content.kind);
            for (index, row) in wrap(&content.text, body_width, style)
                .into_iter()
                .enumerate()
            {
                if index == 0 {
                    lines.push(format!("{gutter}{}", row.text));
                } else {
                    lines.push(format!("{}{}", " ".repeat(gutter_width), row.text));
                }
            }
        }
        lines
    }
}

/// Short left-margin label for an entry kind: what the terminal user sees
/// instead of a raw entry id. One character where it can be, so the gutter does
/// not eat the line.
fn kind_gutter(kind: EntryKind) -> &'static str {
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

/// The style a terminal backend paints an entry in, so the headless renderer
/// and a real one agree on which blocks are de-emphasised.
fn style_for_kind(kind: EntryKind) -> Style {
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

impl Default for HeadlessBackend {
    fn default() -> Self {
        Self {
            store: EntryStore::new(),
            state: BackendState::default(),
        }
    }
}

impl RenderBackend for HeadlessBackend {
    fn mount(&mut self, node: ComponentNode, parent: Option<NodeId>) -> Option<NodeId> {
        self.state.mount(node, parent)
    }

    fn unmount(&mut self, id: NodeId) {
        self.state.unmount(id);
    }

    fn request_render(&mut self, force: bool) {
        self.state.request_render(force);
    }

    /// The frame here is the entry table, not the node arena: this backend
    /// exists to render projections, and its nodes are a formality the shared
    /// arena keeps consistent with the product backends.
    fn frame(&self) -> Frame {
        Frame::new(self.render_lines())
    }

    fn render(&mut self) -> Frame {
        if !self.state.pending {
            return Frame::empty();
        }
        self.state.pending = false;
        self.state.force = false;
        self.frame()
    }

    fn start(&mut self) -> io::Result<()> {
        self.state.started = true;
        Ok(())
    }

    fn stop(&mut self, _opts: StopOptions) -> io::Result<()> {
        self.state.started = false;
        self.state.input.clear();
        Ok(())
    }

    fn dispatch_input(&mut self, ev: InputEvent) {
        self.state.dispatch_input(ev);
    }

    fn set_focus(&mut self, id: Option<NodeId>) {
        self.state.set_focus(id);
    }

    fn focused(&self) -> Option<NodeId> {
        self.state.focus
    }

    /// No terminal, no capabilities: everything degrades to plain text.
    fn capabilities(&self) -> CapabilitySet {
        CapabilitySet::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry_id::{EntryId, EntryKind};
    use crate::entry_op::{AppendDelta, EntryContent, SeqSpan};

    fn upsert(id: EntryId, text: &str) -> EntryOp {
        let kind = id.kind;
        EntryOp::Upsert {
            id,
            content: EntryContent::new(kind, text),
        }
    }

    #[test]
    fn snapshot_is_header_plus_content_in_id_order() {
        let mut backend = HeadlessBackend::new();
        let user = EntryId::engine("t1", 0, EntryKind::UserMsg);
        let assistant = EntryId::engine("t1", 0, EntryKind::AssistantMsg);
        backend.apply(upsert(assistant, "body\nlines"));
        backend.apply(upsert(user, "question"));
        // Same (ns, turn, seq): the kind tiebreak orders user_msg first.
        assert_eq!(
            backend.snapshot(),
            "== engine:t1:0:user_msg\nquestion\n== engine:t1:0:assistant_msg\nbody\nlines\n"
        );
    }

    #[test]
    fn render_lines_matches_snapshot() {
        let mut backend = HeadlessBackend::new();
        let a = EntryId::engine("t1", 0, EntryKind::AssistantMsg);
        backend.apply(upsert(a.clone(), "alpha"));
        backend.apply(EntryOp::Append {
            id: a,
            delta: AppendDelta {
                text: " beta".into(),
                span: SeqSpan::new(0, 1),
                is_final: true,
            },
        });
        assert_eq!(
            backend.render_lines(),
            vec!["== engine:t1:0:assistant_msg", "alpha beta"]
        );
    }

    #[test]
    fn trait_object_path_still_works() {
        let mut backend: Box<dyn RenderBackend> = Box::new(HeadlessBackend::new());
        backend.start().unwrap();
        let id = backend
            .mount(ComponentNode::text("n"), None)
            .expect("root mounts");
        backend.set_focus(Some(id));
        backend.request_render(false);
        backend.unmount(id);
        backend.stop(StopOptions::default()).unwrap();
        assert_eq!(backend.capabilities(), CapabilitySet::default());
    }

    /// The headless backend keeps the same tree invariants as the product
    /// backends: cascade unmount, refused unknown parents, focus release.
    #[test]
    fn headless_shares_the_backend_tree_invariants() {
        let mut backend = HeadlessBackend::new();
        let root = backend.mount(ComponentNode::text("r"), None).unwrap();
        let child = backend.mount(ComponentNode::text("c"), Some(root)).unwrap();
        assert_eq!(
            backend.mount(ComponentNode::text("x"), Some(NodeId(999))),
            None
        );
        backend.set_focus(Some(child));
        backend.unmount(root);
        assert_eq!(
            backend.focused(),
            None,
            "unmounting the root releases focus"
        );
    }
}
