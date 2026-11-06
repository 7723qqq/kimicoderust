//! Render backend contract (design `reports/design/RUST-UI-DECOUPLING.md`
//! §1.3): the `RenderBackend` trait, the shared node arena behind it, and the
//! two product backends.
//!
//! The component tree is pure data; a backend owns it and drives rendering.
//! The two backends share the same tree and differ only in their
//! [`CapabilitySet`], mirroring the TS side's `TuiMainScreen` / `TuiAltScreen`
//! split. What each does with a render pass is the whole difference: the
//! scrollback backend emits only the lines that were not on screen yet, the
//! alt-screen backend repaints the viewport in place.
//!
//! Both are driven through one [`NodeArena`], so an invariant cannot hold in
//! one backend and not the other: parent links exist (a node knows its
//! children), unmounting a node detaches its subtree rather than orphaning it,
//! and focus cannot name a node that is not mounted.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io;

use crate::layout::{Row, Style, wrap};

/// Width a paint uses before any resize has been announced.
///
/// 80 is the classic terminal default and the value the TS side falls back to;
/// choosing it here rather than erroring keeps a headless or test-driven paint
/// meaningful without inventing a resize event first.
pub const DEFAULT_PAINT_WIDTH: usize = 80;

/// Opaque handle for a mounted node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct NodeId(pub u64);

/// Component-tree node.
///
/// Carries what a backend needs to place it: the source text (re-wrapped on
/// every layout, never pre-wrapped, so a resize does not truncate), the style
/// it draws in, and whether it participates in layout at all. Per-node input
/// handlers and richer widgets land with the component crate.
pub struct ComponentNode {
    pub text: String,
    pub style: Style,
    /// Laid out and painted. A node with `visible: false` still occupies the
    /// tree (its children keep their identities) but contributes no rows —
    /// that is how a collapsed block is represented without unmounting it.
    pub visible: bool,
    /// Text wrapped for this width on the last layout pass. `None` means the
    /// node has never been laid out; `Some(w)` with a different width means it
    /// must be re-wrapped from `text` rather than reused.
    laid_out_width: Option<usize>,
    laid_out: Vec<Row>,
    /// Whether the node wants keyboard input. Only a focusable node can take
    /// focus, so a stray `set_focus` cannot hand the editor's keys to a label.
    focusable: bool,
    /// Input handler, called for events delivered to this node. `None` means
    /// the node takes focus but ignores keys (a scroll container, say).
    ///
    /// A boxed closure rather than a trait object: the handler is per-node
    /// state, and a trait would force every component to define a type for
    /// what is one function. It never crosses a thread boundary — the tree is
    /// owned by whichever thread drives the backend.
    on_input: Option<Box<InputHandler>>,
}

/// A node's input handler: one function from an event to what the node did
/// with it. Aliased so the boxed-closure field does not repeat the signature.
pub type InputHandler = dyn FnMut(&InputEvent) -> InputOutcome;

/// What a node did with an input event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputOutcome {
    /// Consumed; nobody else sees it.
    Handled,
    /// Not for this node. The event keeps falling through to the next
    /// candidate (an enclosing focusable ancestor, then the global queue).
    Ignored,
}

impl std::fmt::Debug for ComponentNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ComponentNode")
            .field("text", &self.text)
            .field("style", &self.style)
            .field("visible", &self.visible)
            .field("focusable", &self.focusable)
            .field("has_handler", &self.on_input.is_some())
            .finish_non_exhaustive()
    }
}

/// Two nodes are equal when what they *paint* is equal. The handler is not
/// part of that: comparing closures is not a thing, and a node that differs
/// only in its handler looks identical on screen.
impl PartialEq for ComponentNode {
    fn eq(&self, other: &Self) -> bool {
        self.text == other.text
            && self.style == other.style
            && self.visible == other.visible
            && self.focusable == other.focusable
            && self.laid_out == other.laid_out
    }
}

impl Eq for ComponentNode {}

impl Default for ComponentNode {
    fn default() -> Self {
        Self::text("")
    }
}

impl Clone for ComponentNode {
    /// Cloning drops the handler: a cloned node is a new node, and a clone
    /// that shared a closure would make two nodes answer to one piece of
    /// captured state. Callers that need one re-attach it.
    fn clone(&self) -> Self {
        Self {
            text: self.text.clone(),
            style: self.style,
            visible: self.visible,
            laid_out_width: self.laid_out_width,
            laid_out: self.laid_out.clone(),
            focusable: self.focusable,
            on_input: None,
        }
    }
}

impl ComponentNode {
    /// A plain-text node: default style, visible, not focusable.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: Style::DEFAULT,
            visible: true,
            laid_out_width: None,
            laid_out: Vec::new(),
            focusable: false,
            on_input: None,
        }
    }

    /// Let this node take focus and receive input.
    pub fn focusable(mut self) -> Self {
        self.focusable = true;
        self
    }

    /// Attach an input handler. Implies [`Self::focusable`]: a handler that can
    /// never be reached is a bug waiting for a confusing report.
    pub fn on_input(mut self, handler: impl FnMut(&InputEvent) -> InputOutcome + 'static) -> Self {
        self.focusable = true;
        self.on_input = Some(Box::new(handler));
        self
    }

    pub fn is_focusable(&self) -> bool {
        self.focusable
    }

    pub fn styled(text: impl Into<String>, style: Style) -> Self {
        Self {
            style,
            ..Self::text(text)
        }
    }

    /// Lay this node out at `width`, re-wrapping only when the width changed.
    ///
    /// Caching on width rather than on call count is what makes a resize
    /// correct: the rows were computed for the old width, so reusing them
    /// would draw lines the terminal can no longer hold.
    pub fn layout(&mut self, width: usize) -> &[Row] {
        if self.laid_out_width != Some(width.max(1)) {
            self.laid_out = if self.visible {
                wrap(&self.text, width, self.style)
            } else {
                Vec::new()
            };
            self.laid_out_width = Some(width.max(1));
        }
        &self.laid_out
    }

    /// The rows of the last layout pass (empty before the first one).
    pub fn laid_out(&self) -> &[Row] {
        &self.laid_out
    }
}

/// Keyboard identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Key {
    Char(char),
    Esc,
    Enter,
    Tab,
    Backspace,
    Delete,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
}

impl Key {
    /// Whether this key moves the cursor rather than editing text. A backend
    /// uses this to decide what "input progress" means without knowing the
    /// editor's semantics.
    pub fn is_navigation(self) -> bool {
        matches!(
            self,
            Key::Left
                | Key::Right
                | Key::Up
                | Key::Down
                | Key::Home
                | Key::End
                | Key::PageUp
                | Key::PageDown
        )
    }
}

/// Modifier state accompanying a [`KeyEvent`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Modifiers {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

impl Modifiers {
    pub fn ctrl() -> Self {
        Self {
            ctrl: true,
            ..Self::default()
        }
    }

    pub fn with_shift(self, shift: bool) -> Self {
        Self { shift, ..self }
    }
}

/// One keyboard event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct KeyEvent {
    pub key: Key,
    pub modifiers: Modifiers,
}

impl KeyEvent {
    pub fn new(key: Key) -> Self {
        Self {
            key,
            modifiers: Modifiers::default(),
        }
    }

    /// v2 / readline `Ctrl+C`. Kept as one predicate so every backend agrees
    /// on which chord cancels: a backend that spelled it inline once already
    /// differed from the other.
    pub fn is_cancel(self) -> bool {
        self.modifiers.ctrl && matches!(self.key, Key::Char('c' | 'C'))
    }
}

/// Mouse gesture kinds the backends model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseKind {
    ScrollUp,
    ScrollDown,
    Press,
    Release,
}

/// One mouse event in viewport coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MouseEvent {
    pub kind: MouseKind,
    pub column: u16,
    pub row: u16,
}

/// Single input entry point.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum InputEvent {
    Key(KeyEvent),
    Paste(String),
    Mouse(MouseEvent),
    /// Terminal resized; backends re-measure and schedule a full repaint.
    Resize {
        columns: u16,
        rows: u16,
    },
}

/// Terminal state to restore on [`RenderBackend::stop`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StopOptions {
    /// Leave the last frame on screen (scrollback keeps the transcript; an
    /// alt-screen backend otherwise clears the viewport it took over).
    pub keep_frame: bool,
}

/// What a backend can do, so a component degrades instead of guessing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CapabilitySet {
    /// Renders by appending to the terminal's own scrollback.
    pub scrollback_passthrough: bool,
    /// Captures the mouse for in-viewport scrolling.
    pub mouse_capture: bool,
    /// Draws text selection itself (the terminal's is unavailable).
    pub text_selection: bool,
    /// In-buffer search over the rendered frame.
    pub search: bool,
    pub osc8_links: bool,
    /// Inline images (kitty / sixel / iTerm2).
    pub inline_images: bool,
}

/// A run of rendered lines, in display order.
///
/// The unit every backend schedules a repaint in: the scrollback backend hands
/// back only the tail it has not printed, the alt-screen backend paints the
/// whole frame.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Frame {
    lines: Vec<String>,
}

impl Frame {
    pub fn new(lines: impl IntoIterator<Item = String>) -> Self {
        Self {
            lines: lines.into_iter().collect(),
        }
    }

    pub fn empty() -> Self {
        Self::default()
    }

    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// The lines of `self` that are not a prefix of `previous` — what a
    /// passthrough backend still has to print.
    ///
    /// A common prefix is assumed rather than a common substring: the terminal
    /// cannot retract a line it already printed, so content that changed
    /// *before* the divergence point cannot be corrected by this backend and
    /// is reported as a full repaint instead of a wrong partial one.
    pub fn tail_after(&self, previous: &Frame) -> Frame {
        let common = self
            .lines
            .iter()
            .zip(previous.lines.iter())
            .take_while(|(a, b)| a == b)
            .count();
        if common == self.lines.len() {
            return Frame::empty();
        }
        if common < previous.lines.len() {
            // Something above the divergence point changed. A passthrough
            // terminal cannot fix it, so the caller has to decide what to do
            // with a full redraw; signalling that here beats printing a
            // silently stale screen.
            return self.clone();
        }
        Frame::new(self.lines[common..].to_vec())
    }
}

/// The contract a UI host drives. Object-safe: the backend is chosen at launch
/// and stored behind `dyn`.
pub trait RenderBackend {
    /// Attach `node` under `parent` (`None` = root), returning its handle.
    ///
    /// Returns `None` when `parent` is not a mounted node: a caller that names
    /// a stale parent gets no node rather than a silently detached one.
    fn mount(&mut self, node: ComponentNode, parent: Option<NodeId>) -> Option<NodeId>;
    /// Detach a node and its whole subtree. Unknown ids are a no-op.
    fn unmount(&mut self, id: NodeId);
    /// Ask for a render pass; `force` bypasses diffing.
    fn request_render(&mut self, force: bool);
    /// The frame that the next render pass would paint.
    fn frame(&self) -> Frame;
    /// Run the pending render pass and return the lines this backend actually
    /// emitted (empty when nothing changed or nothing was requested).
    fn render(&mut self) -> Frame;
    /// Enter the render loop's terminal state (raw mode, alt-screen, ...).
    fn start(&mut self) -> io::Result<()>;
    /// Leave it again, restoring the terminal per `opts`.
    fn stop(&mut self, opts: StopOptions) -> io::Result<()>;
    /// Single input entry point (keyboard / paste / mouse / resize).
    fn dispatch_input(&mut self, ev: InputEvent);
    /// Move keyboard focus; `None` releases it. Focusing an unmounted node is
    /// ignored (focus cannot name something that is not there).
    fn set_focus(&mut self, id: Option<NodeId>);
    /// The currently focused node, if any.
    fn focused(&self) -> Option<NodeId>;
    /// The capability set components degrade against.
    fn capabilities(&self) -> CapabilitySet;
}

/// The node table both built-in backends share.
///
/// Tracks the parent link and child order, which is what makes cascade unmount
/// and depth-first painting possible. Keyed by a monotonic id: ids are never
/// reused, so a stale handle can never resolve to a different node.
#[derive(Debug, Default)]
pub struct NodeArena {
    nodes: HashMap<NodeId, ComponentNode>,
    parents: HashMap<NodeId, Option<NodeId>>,
    children: HashMap<NodeId, Vec<NodeId>>,
    next: u64,
}

impl NodeArena {
    /// Mount `node` under `parent`. `None` for an unknown parent is the
    /// "refused" answer, not an error: a caller holding a stale id is a bug in
    /// the caller, and silently re-rooting the node would hide it.
    pub(crate) fn mount(&mut self, node: ComponentNode, parent: Option<NodeId>) -> Option<NodeId> {
        if let Some(p) = parent
            && !self.nodes.contains_key(&p)
        {
            return None;
        }
        let id = NodeId(self.next);
        self.next += 1;
        self.nodes.insert(id, node);
        self.parents.insert(id, parent);
        self.children.insert(id, Vec::new());
        if let Some(p) = parent
            && let Some(siblings) = self.children.get_mut(&p)
        {
            siblings.push(id);
        }
        Some(id)
    }

    /// Detach `id` and everything beneath it. Returns the number of nodes
    /// removed (0 for an unknown id), so a caller can tell "nothing to do" from
    /// "removed a subtree".
    pub(crate) fn unmount(&mut self, id: NodeId) -> usize {
        if !self.nodes.contains_key(&id) {
            return 0;
        }
        let mut removed = 0;
        let mut stack = vec![id];
        while let Some(current) = stack.pop() {
            if let Some(kids) = self.children.remove(&current) {
                stack.extend(kids);
            }
            self.parents.remove(&current);
            if self.nodes.remove(&current).is_some() {
                removed += 1;
            }
        }
        // Detach the subtree root from its own parent's child list.
        for kids in self.children.values_mut() {
            kids.retain(|child| *child != id);
        }
        removed
    }

    pub fn contains(&self, id: NodeId) -> bool {
        self.nodes.contains_key(&id)
    }

    pub fn get(&self, id: NodeId) -> Option<&ComponentNode> {
        self.nodes.get(&id)
    }

    pub(crate) fn get_mut(&mut self, id: NodeId) -> Option<&mut ComponentNode> {
        self.nodes.get_mut(&id)
    }

    /// Whether `id` is mounted *and* declared focusable.
    pub(crate) fn is_focusable(&self, id: NodeId) -> bool {
        self.nodes.get(&id).is_some_and(|node| node.focusable)
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Depth-first, child-order traversal of every mounted node.
    pub fn walk(&self) -> Vec<NodeId> {
        let mut out = Vec::with_capacity(self.nodes.len());
        let mut roots: Vec<NodeId> = self
            .parents
            .iter()
            .filter(|(_, parent)| parent.is_none())
            .map(|(id, _)| *id)
            .collect();
        roots.sort();
        let mut stack: Vec<NodeId> = roots.into_iter().rev().collect();
        while let Some(id) = stack.pop() {
            out.push(id);
            if let Some(kids) = self.children.get(&id) {
                stack.extend(kids.iter().rev().copied());
            }
        }
        out
    }

    /// Every node's text as one line per node, in paint order.
    pub fn lines(&self) -> Vec<String> {
        self.walk()
            .into_iter()
            .filter_map(|id| self.nodes.get(&id))
            .map(|node| node.text.clone())
            .collect()
    }

    /// Lay every visible node out at `width` and collect the resulting rows,
    /// in paint order.
    ///
    /// This is the step that makes a node a component rather than a string:
    /// the width decides how many rows it occupies, and a resize re-wraps from
    /// the source text instead of reusing rows that no longer fit.
    pub(crate) fn layout(&mut self, width: usize) -> Vec<Row> {
        let ids = self.walk();
        let mut rows = Vec::new();
        for id in ids {
            if let Some(node) = self.nodes.get_mut(&id) {
                rows.extend(node.layout(width).iter().cloned());
            }
        }
        rows
    }

    /// The style of a node run, coalesced: consecutive rows that share a style
    /// collapse into one [`StyledRow`], which is what a backend emits as one
    /// SGR span.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn styled_rows(&mut self, width: usize) -> Vec<StyledRow> {
        let mut out: Vec<StyledRow> = Vec::new();
        for row in self.layout(width) {
            match out.last_mut() {
                Some(last) if last.style == row.style => {
                    last.lines.push(row.text);
                }
                _ => out.push(StyledRow {
                    style: row.style,
                    lines: vec![row.text],
                }),
            }
        }
        out
    }
}

/// A run of consecutive painted lines that share one style.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StyledRow {
    pub style: Style,
    pub lines: Vec<String>,
}

/// Input a backend has received but not yet consumed by a component.
///
/// The component crate will drain this; keeping the queue here is what lets the
/// backends be complete on their own side of the seam instead of dropping
/// events while no handler is wired.
#[derive(Debug, Default)]
pub struct InputQueue {
    events: std::collections::VecDeque<InputEvent>,
}

impl InputQueue {
    pub fn push(&mut self, ev: InputEvent) {
        // A resize supersedes any queued resize: only the latest geometry can
        // be rendered, and letting them accumulate makes a resize storm look
        // like an input storm.
        if matches!(ev, InputEvent::Resize { .. }) {
            self.events
                .retain(|queued| !matches!(queued, InputEvent::Resize { .. }));
        }
        self.events.push_back(ev);
    }

    pub fn pop(&mut self) -> Option<InputEvent> {
        self.events.pop_front()
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Drop everything; used when a backend stops and late input can no longer
    /// reach a component.
    pub fn clear(&mut self) {
        self.events.clear();
    }
}

/// State every built-in backend keeps: the tree, focus, the viewport size, the
/// pending-render flag, and the input queue.
#[derive(Debug, Default)]
pub(crate) struct BackendState {
    pub(crate) arena: NodeArena,
    pub(crate) focus: Option<NodeId>,
    pub(crate) pending: bool,
    pub(crate) force: bool,
    pub(crate) started: bool,
    pub(crate) size: (u16, u16),
    pub(crate) input: InputQueue,
    pub(crate) last_frame: Frame,
}

impl BackendState {
    pub(crate) fn mount(&mut self, node: ComponentNode, parent: Option<NodeId>) -> Option<NodeId> {
        self.arena.mount(node, parent)
    }

    pub(crate) fn unmount(&mut self, id: NodeId) {
        self.arena.unmount(id);
        if self.focus == Some(id) || self.focus.is_some_and(|f| !self.arena.contains(f)) {
            self.focus = None;
        }
    }

    pub(crate) fn request_render(&mut self, force: bool) {
        self.pending = true;
        self.force |= force;
    }

    /// Lay out at `width` and return the resulting frame. Mutable because a
    /// node caches its wrapped rows; the read-only [`RenderBackend::frame`]
    /// reports the cached result instead.
    pub(crate) fn layout_frame(&mut self, width: usize) -> Frame {
        Frame::new(
            self.arena
                .layout(width)
                .into_iter()
                .map(|row| row.text)
                .collect::<Vec<_>>(),
        )
    }

    /// The viewport width a paint should use: the last resize, else a
    /// generous default so a backend used before any resize still lays out.
    pub(crate) fn paint_width(&self) -> usize {
        match self.size.0 {
            0 => DEFAULT_PAINT_WIDTH,
            columns => columns as usize,
        }
    }

    pub(crate) fn set_focus(&mut self, id: Option<NodeId>) {
        if let Some(target) = id
            && !self.arena.is_focusable(target)
        {
            return;
        }
        self.focus = id;
    }

    /// Deliver one event: resize updates the geometry, then the event goes to
    /// the focused node's handler. Anything the focused handler ignores (or
    /// everything, when nothing is focused) is queued for the host, so a
    /// dispatch is never a silent drop.
    pub(crate) fn dispatch_input(&mut self, ev: InputEvent) {
        if let InputEvent::Resize { columns, rows } = ev {
            self.size = (columns, rows);
            // A resize is geometry, not something a component consumes: it
            // always reaches the host so a layout pass is scheduled.
            self.input.push(ev);
            return;
        }
        if let Some(id) = self.focus
            && let Some(node) = self.arena.get_mut(id)
            && let Some(handler) = node.on_input.as_mut()
            && handler(&ev) == InputOutcome::Handled
        {
            return;
        }
        self.input.push(ev);
    }
}

/// Passthrough scrollback backend: renders by appending the lines the terminal
/// has not printed yet. No mouse, no search.
#[derive(Default)]
pub struct ScrollbackBackend {
    state: BackendState,
}

impl ScrollbackBackend {
    pub fn new() -> Self {
        Self::default()
    }

    /// Lines already handed to the terminal. Exposed for tests and for a host
    /// that resumes into an existing scrollback.
    pub fn emitted_line_count(&self) -> usize {
        self.state.last_frame.len()
    }

    /// Events that reached the host queue instead of a focused component.
    pub fn pending_input_len(&self) -> usize {
        self.state.input.len()
    }

    /// Drain the host queue.
    pub fn take_input(&mut self) -> Vec<InputEvent> {
        let mut out = Vec::with_capacity(self.state.input.len());
        while let Some(ev) = self.state.input.pop() {
            out.push(ev);
        }
        out
    }
}

impl RenderBackend for ScrollbackBackend {
    fn mount(&mut self, node: ComponentNode, parent: Option<NodeId>) -> Option<NodeId> {
        self.state.mount(node, parent)
    }

    fn unmount(&mut self, id: NodeId) {
        self.state.unmount(id);
    }

    fn request_render(&mut self, force: bool) {
        self.state.request_render(force);
    }

    fn frame(&self) -> Frame {
        self.state.last_frame.clone()
    }

    /// Emits only the new tail. A `force` pass re-emits the whole frame, which
    /// is what a caller asks for when the terminal was cleared underneath us.
    fn render(&mut self) -> Frame {
        if !self.state.pending {
            return Frame::empty();
        }
        let width = self.state.paint_width();
        let frame = self.state.layout_frame(width);
        let emitted = if self.state.force {
            frame.clone()
        } else {
            frame.tail_after(&self.state.last_frame)
        };
        if !emitted.is_empty() || self.state.force {
            self.state.last_frame = frame;
            self.state.pending = false;
            self.state.force = false;
            return emitted;
        }
        // Nothing new: the pending flag still clears, otherwise every caller
        // that polls render() would re-run the same comparison forever.
        self.state.pending = false;
        Frame::empty()
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

    fn capabilities(&self) -> CapabilitySet {
        CapabilitySet {
            scrollback_passthrough: true,
            ..CapabilitySet::default()
        }
    }
}

/// Fullscreen alt-screen backend: owns the viewport, so every render pass
/// repaints the whole frame and `stop` clears it unless asked to keep the last
/// one.
#[derive(Default)]
pub struct AltScreenBackend {
    state: BackendState,
    /// Whether the screen currently holds a frame this backend painted, which
    /// is what `stop` needs to know before clearing.
    painted: bool,
}

impl AltScreenBackend {
    pub fn new() -> Self {
        Self::default()
    }

    /// The viewport size the last resize announced (`(0, 0)` before one).
    pub fn viewport(&self) -> (u16, u16) {
        self.state.size
    }

    /// Events that reached the host queue instead of a focused component.
    pub fn pending_input_len(&self) -> usize {
        self.state.input.len()
    }

    /// Drain the host queue.
    pub fn take_input(&mut self) -> Vec<InputEvent> {
        let mut out = Vec::with_capacity(self.state.input.len());
        while let Some(ev) = self.state.input.pop() {
            out.push(ev);
        }
        out
    }
}

impl RenderBackend for AltScreenBackend {
    fn mount(&mut self, node: ComponentNode, parent: Option<NodeId>) -> Option<NodeId> {
        self.state.mount(node, parent)
    }

    fn unmount(&mut self, id: NodeId) {
        self.state.unmount(id);
    }

    fn request_render(&mut self, force: bool) {
        self.state.request_render(force);
    }

    fn frame(&self) -> Frame {
        self.state.last_frame.clone()
    }

    /// Repaints the whole viewport: that is the point of taking the screen
    /// over, and it makes a partial-diff bug impossible here.
    fn render(&mut self) -> Frame {
        if !self.state.pending {
            return Frame::empty();
        }
        let width = self.state.paint_width();
        let frame = self.state.layout_frame(width);
        self.state.last_frame = frame.clone();
        self.state.pending = false;
        self.state.force = false;
        self.painted = true;
        frame
    }

    fn start(&mut self) -> io::Result<()> {
        self.state.started = true;
        Ok(())
    }

    fn stop(&mut self, opts: StopOptions) -> io::Result<()> {
        self.state.started = false;
        self.state.input.clear();
        if !opts.keep_frame {
            self.state.last_frame = Frame::empty();
            self.painted = false;
        }
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

    fn capabilities(&self) -> CapabilitySet {
        CapabilitySet {
            mouse_capture: true,
            text_selection: true,
            search: true,
            ..CapabilitySet::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(text: &str) -> ComponentNode {
        ComponentNode::text(text)
    }

    /// The trait must be object-safe: hosts select the backend at launch time.
    fn assert_object_safe(_backend: &dyn RenderBackend) {}

    #[test]
    fn trait_is_object_safe() {
        let backends: Vec<Box<dyn RenderBackend>> = vec![
            Box::new(ScrollbackBackend::new()),
            Box::new(AltScreenBackend::new()),
        ];
        for backend in &backends {
            assert_object_safe(backend.as_ref());
        }
    }

    #[test]
    fn scrollback_reports_passthrough_capabilities() {
        let caps = ScrollbackBackend::new().capabilities();
        assert!(caps.scrollback_passthrough);
        assert!(!caps.mouse_capture);
        assert!(!caps.text_selection);
        assert!(!caps.search);
        assert!(!caps.osc8_links);
        assert!(!caps.inline_images);
    }

    #[test]
    fn alt_screen_reports_fullscreen_capabilities() {
        let caps = AltScreenBackend::new().capabilities();
        assert!(!caps.scrollback_passthrough);
        assert!(caps.mouse_capture);
        assert!(caps.text_selection);
        assert!(caps.search);
    }

    #[test]
    fn lifecycle_and_mounting_work_through_the_trait_object() {
        let mut backend: Box<dyn RenderBackend> = Box::new(ScrollbackBackend::new());
        backend.start().unwrap();
        let root = backend.mount(node("root"), None).unwrap();
        let child = backend.mount(node("child"), Some(root)).unwrap();
        assert_ne!(root, child);
        backend.set_focus(Some(child));
        backend.dispatch_input(InputEvent::Key(KeyEvent {
            key: Key::Esc,
            modifiers: Modifiers::default(),
        }));
        backend.dispatch_input(InputEvent::Paste(" pasted ".into()));
        backend.dispatch_input(InputEvent::Mouse(MouseEvent {
            kind: MouseKind::ScrollUp,
            column: 0,
            row: 3,
        }));
        backend.request_render(true);
        backend.unmount(child);
        backend.set_focus(None);
        backend.stop(StopOptions { keep_frame: true }).unwrap();
    }

    #[test]
    fn node_ids_are_monotonic_per_backend() {
        let mut backend = AltScreenBackend::new();
        let a = backend.mount(node("a"), None).unwrap();
        let b = backend.mount(node("b"), None).unwrap();
        assert!(a < b);
        backend.unmount(a);
        let c = backend.mount(node("c"), None).unwrap();
        assert!(b < c, "ids are never reused");
    }

    /// Mounting under an id that is not mounted is refused rather than
    /// silently re-rooted: a caller holding a stale handle has a bug, and
    /// hiding it produces a tree nobody describes.
    #[test]
    fn mounting_under_an_unknown_parent_is_refused() {
        let mut backend = ScrollbackBackend::new();
        assert_eq!(backend.mount(node("orphan"), Some(NodeId(999))), None);
        assert!(backend.frame().is_empty());
    }

    /// Unmounting a node takes its subtree with it — the bug the skeleton had,
    /// where children stayed in the arena with a dangling parent.
    #[test]
    fn unmount_detaches_the_whole_subtree() {
        let mut backend = ScrollbackBackend::new();
        let root = backend.mount(node("root"), None).unwrap();
        let mid = backend.mount(node("mid"), Some(root)).unwrap();
        let leaf = backend.mount(node("leaf"), Some(mid)).unwrap();
        let sibling = backend.mount(node("sibling"), Some(root)).unwrap();
        backend.request_render(true);
        assert_eq!(backend.render().len(), 4);

        backend.unmount(mid);
        backend.request_render(true);
        let lines = backend.render().lines().to_vec();
        assert_eq!(lines, vec!["root", "sibling"]);
        assert!(!lines.iter().any(|l| l == "leaf"));
        let _ = (leaf, sibling);
    }

    /// A node is laid out to the viewport width, not printed as one long line.
    /// This is the difference between a component and a string.
    #[test]
    fn nodes_are_wrapped_to_the_viewport_width() {
        let mut backend = ScrollbackBackend::new();
        backend.dispatch_input(InputEvent::Resize {
            columns: 10,
            rows: 24,
        });
        backend.mount(node("alpha beta gamma delta"), None).unwrap();
        backend.request_render(true);
        let rows = backend.render().lines().to_vec();
        assert!(
            rows.len() > 1,
            "one logical line became several rows: {rows:?}"
        );
        for row in &rows {
            assert!(crate::layout::width(row) <= 10, "row overflows: {row:?}");
        }
    }

    /// A resize re-wraps from the source text. Reusing rows computed for the
    /// previous width is the bug this guards: they would draw lines the
    /// terminal can no longer hold.
    #[test]
    fn resize_relayouts_instead_of_reusing_old_rows() {
        let mut backend = ScrollbackBackend::new();
        backend.dispatch_input(InputEvent::Resize {
            columns: 40,
            rows: 24,
        });
        backend.mount(node("alpha beta gamma delta"), None).unwrap();
        backend.request_render(true);
        assert_eq!(backend.render().len(), 1, "fits on one row at width 40");

        backend.dispatch_input(InputEvent::Resize {
            columns: 10,
            rows: 24,
        });
        backend.request_render(true);
        let rows = backend.render().lines().to_vec();
        assert!(rows.len() > 1, "narrowing must re-wrap: {rows:?}");
        for row in &rows {
            assert!(crate::layout::width(row) <= 10, "row overflows: {row:?}");
        }
    }

    /// An invisible node contributes no rows but keeps its identity, so a
    /// collapsed block can be expanded again without re-mounting.
    #[test]
    fn an_invisible_node_paints_nothing_but_stays_mounted() {
        let mut backend = ScrollbackBackend::new();
        let hidden = ComponentNode {
            visible: false,
            ..ComponentNode::text("secret")
        };
        let id = backend.mount(hidden, None).unwrap();
        backend.request_render(true);
        assert!(backend.render().is_empty(), "invisible paints nothing");

        backend.unmount(id);
        assert!(backend.mount(node("after"), None).is_some());
    }

    /// Consecutive rows sharing a style coalesce into one span: a backend
    /// emits one SGR run for a whole block instead of one per line.
    #[test]
    fn styled_rows_coalesce_runs_of_the_same_style() {
        let mut arena = NodeArena::default();
        let bold = crate::layout::Style::DEFAULT.bold();
        arena
            .mount(ComponentNode::styled("one", bold), None)
            .unwrap();
        arena
            .mount(ComponentNode::styled("two", bold), None)
            .unwrap();
        arena
            .mount(
                ComponentNode::styled("three", crate::layout::Style::DEFAULT),
                None,
            )
            .unwrap();

        let runs = arena.styled_rows(40);
        assert_eq!(runs.len(), 2, "two styles, two runs: {runs:?}");
        assert_eq!(runs[0].style, bold);
        assert_eq!(runs[0].lines, vec!["one".to_string(), "two".to_string()]);
        assert_eq!(runs[1].lines, vec!["three".to_string()]);
    }

    /// Without any resize the paint still lays out, at the documented default.
    #[test]
    fn paint_before_any_resize_uses_the_default_width() {
        let mut backend = ScrollbackBackend::new();
        let long = "x".repeat(DEFAULT_PAINT_WIDTH + 20);
        backend.mount(node(&long), None).unwrap();
        backend.request_render(true);
        let rows = backend.render().lines().to_vec();
        assert_eq!(rows.len(), 2, "wrapped at the default width: {rows:?}");
        for row in &rows {
            assert!(crate::layout::width(row) <= DEFAULT_PAINT_WIDTH);
        }
    }

    #[test]
    fn walk_is_depth_first_in_child_order() {
        let mut arena = NodeArena::default();
        let root = arena.mount(node("root"), None).unwrap();
        let a = arena.mount(node("a"), Some(root)).unwrap();
        let _a1 = arena.mount(node("a1"), Some(a)).unwrap();
        let _b = arena.mount(node("b"), Some(root)).unwrap();
        assert_eq!(arena.lines(), vec!["root", "a", "a1", "b"]);
    }

    /// A passthrough backend prints only what the terminal has not seen.
    #[test]
    fn scrollback_emits_only_the_new_tail() {
        let mut backend = ScrollbackBackend::new();
        let root = backend.mount(node("one"), None).unwrap();
        backend.request_render(false);
        assert_eq!(backend.render().lines(), &["one".to_string()]);
        assert_eq!(backend.emitted_line_count(), 1);

        // Appending a child leaves the first line alone.
        backend.mount(node("two"), Some(root)).unwrap();
        backend.request_render(false);
        assert_eq!(backend.render().lines(), &["two".to_string()]);

        // Nothing changed: no emission, and the pending flag still cleared.
        backend.request_render(false);
        assert!(backend.render().is_empty());
    }

    /// An alt-screen backend repaints its whole frame every pass — it owns the
    /// viewport, so a partial diff there would be the bug.
    #[test]
    fn alt_screen_repaints_the_whole_frame() {
        let mut backend = AltScreenBackend::new();
        let root = backend.mount(node("one"), None).unwrap();
        backend.request_render(false);
        assert_eq!(backend.render().lines(), &["one".to_string()]);

        backend.mount(node("two"), Some(root)).unwrap();
        backend.request_render(false);
        assert_eq!(
            backend.render().lines(),
            &["one".to_string(), "two".to_string()]
        );
    }

    /// A `force` pass on the scrollback backend re-emits everything: that is
    /// what a host asks for when the terminal was cleared underneath it.
    #[test]
    fn forced_render_re_emits_the_whole_frame() {
        let mut backend = ScrollbackBackend::new();
        backend.mount(node("one"), None).unwrap();
        backend.request_render(false);
        backend.render();
        backend.request_render(true);
        assert_eq!(backend.render().lines(), &["one".to_string()]);
    }

    /// Content changed above the divergence point cannot be fixed by
    /// appending, so the frame is reported whole rather than partially wrong.
    #[test]
    fn tail_after_reports_a_full_frame_when_an_earlier_line_changed() {
        let previous = Frame::new(vec!["a".into(), "b".into(), "c".into()]);
        let changed = Frame::new(vec!["a".into(), "CHANGED".into(), "c".into()]);
        assert_eq!(changed.tail_after(&previous), changed);
        let extended = Frame::new(vec!["a".into(), "b".into(), "c".into(), "d".into()]);
        assert_eq!(extended.tail_after(&previous).lines(), &["d".to_string()]);
        assert!(previous.tail_after(&previous).is_empty());
    }

    /// Focus cannot name a node that is not mounted or not focusable, and
    /// unmounting the focused node releases focus.
    #[test]
    fn focus_is_cleared_when_its_node_goes_away() {
        let mut backend = AltScreenBackend::new();
        assert_eq!(backend.mount(node("a"), None), Some(NodeId(0)));
        backend.set_focus(Some(NodeId(999)));
        assert_eq!(backend.focused(), None, "an unknown id does not take focus");

        // A plain text node is not focusable: a label must not swallow the
        // editor's keys.
        let label = backend.mount(node("label"), None).unwrap();
        backend.set_focus(Some(label));
        assert_eq!(backend.focused(), None, "a non-focusable node is refused");

        let id = backend
            .mount(ComponentNode::text("b").focusable(), None)
            .unwrap();
        backend.set_focus(Some(id));
        assert_eq!(backend.focused(), Some(id));
        backend.unmount(id);
        assert_eq!(backend.focused(), None);
    }

    /// A focused node's handler consumes the event; anything it ignores falls
    /// through to the host queue rather than vanishing.
    #[test]
    fn focused_handler_consumes_and_ignored_events_fall_through() {
        use std::cell::RefCell;
        use std::rc::Rc;

        let seen = Rc::new(RefCell::new(Vec::new()));
        let sink = Rc::clone(&seen);
        let mut backend = AltScreenBackend::new();
        let id = backend
            .mount(
                ComponentNode::text("editor").on_input(move |ev| {
                    if matches!(
                        ev,
                        InputEvent::Key(KeyEvent {
                            key: Key::Char('c'),
                            ..
                        })
                    ) {
                        sink.borrow_mut().push("c");
                        InputOutcome::Handled
                    } else {
                        InputOutcome::Ignored
                    }
                }),
                None,
            )
            .unwrap();
        backend.set_focus(Some(id));

        backend.dispatch_input(InputEvent::Key(KeyEvent::new(Key::Char('c'))));
        assert_eq!(*seen.borrow(), vec!["c"]);
        assert_eq!(
            backend.pending_input_len(),
            0,
            "a handled event is not queued for the host"
        );

        backend.dispatch_input(InputEvent::Key(KeyEvent::new(Key::Esc)));
        assert_eq!(
            *seen.borrow(),
            vec!["c"],
            "ignored events do not reach the handler twice"
        );
        assert_eq!(backend.pending_input_len(), 1, "ignored falls through");
    }

    /// With nothing focused every event goes to the host: a backend is not a
    /// filter, and dropping input before a component exists loses keystrokes.
    #[test]
    fn unfocused_backend_queues_every_event() {
        let mut backend = ScrollbackBackend::new();
        backend.dispatch_input(InputEvent::Key(KeyEvent::new(Key::Char('x'))));
        backend.dispatch_input(InputEvent::Paste("p".into()));
        assert_eq!(backend.pending_input_len(), 2);
    }

    /// A resize always reaches the host: it is geometry, not something a
    /// component consumes, and the host schedules the relayout.
    #[test]
    fn resize_reaches_the_host_even_with_a_focused_handler() {
        let mut backend = AltScreenBackend::new();
        let id = backend
            .mount(
                ComponentNode::text("e").on_input(|_| InputOutcome::Handled),
                None,
            )
            .unwrap();
        backend.set_focus(Some(id));
        backend.dispatch_input(InputEvent::Resize {
            columns: 100,
            rows: 40,
        });
        assert_eq!(backend.viewport(), (100, 40));
        assert_eq!(backend.pending_input_len(), 1);
    }

    /// Input is queued rather than dropped: the component crate drains it, and
    /// a backend that discarded events would look correct while losing the
    /// user's keystrokes.
    #[test]
    fn input_is_queued_and_a_resize_supersedes_an_earlier_one() {
        let mut backend = ScrollbackBackend::new();
        backend.dispatch_input(InputEvent::Key(KeyEvent::new(Key::Char('x'))));
        backend.dispatch_input(InputEvent::Resize {
            columns: 80,
            rows: 24,
        });
        backend.dispatch_input(InputEvent::Resize {
            columns: 100,
            rows: 40,
        });
        assert_eq!(backend.state.input.len(), 2, "only the latest resize stays");
        assert!(matches!(
            backend.state.input.pop(),
            Some(InputEvent::Key(KeyEvent { .. }))
        ));
        assert!(matches!(
            backend.state.input.pop(),
            Some(InputEvent::Resize { columns: 100, .. })
        ));
        assert!(backend.state.input.is_empty());
    }

    /// Stopping clears queued input: late events can no longer reach a
    /// component, so keeping them only grows without bound.
    #[test]
    fn stop_clears_queued_input() {
        let mut backend = ScrollbackBackend::new();
        backend.dispatch_input(InputEvent::Paste("still here?".into()));
        backend.stop(StopOptions::default()).unwrap();
        assert!(backend.state.input.is_empty());
    }

    #[test]
    fn cancel_chord_is_defined_once() {
        assert!(
            KeyEvent {
                key: Key::Char('c'),
                modifiers: Modifiers::ctrl(),
            }
            .is_cancel()
        );
        assert!(
            KeyEvent {
                key: Key::Char('C'),
                modifiers: Modifiers::ctrl(),
            }
            .is_cancel()
        );
        assert!(!KeyEvent::new(Key::Char('c')).is_cancel());
        assert!(
            !KeyEvent {
                key: Key::Esc,
                modifiers: Modifiers::ctrl(),
            }
            .is_cancel()
        );
    }
}
