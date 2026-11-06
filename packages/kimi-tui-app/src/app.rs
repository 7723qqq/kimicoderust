//! The application layer: an [`EntryOp`] stream in, a terminal out, user
//! commands back.
//!
//! It owns three things and nothing else:
//!
//! - the entry table the engine's projection produces ([`EntryStore`]),
//! - the terminal backend that paints it ([`TerminalBackend`]),
//! - the line being typed, and the two commands a user can issue while a turn
//!   runs: submit a prompt, or cancel (Esc / Ctrl-C).
//!
//! The link to a live session is a trait ([`UiTransport`]), not a dependency:
//! whichever host owns the engine forwards [`UiCommand`]s and feeds
//! [`App::apply`] with the ops it receives. That keeps this crate out of the
//! engine's layer — the design's rule is that the app talks to the engine over
//! the contract, not by linking it (`reports/design/RUST-UI-DECOUPLING.md` §3).

use std::io::Write;

use kimi_ui_core::render_backend::DEFAULT_PAINT_WIDTH;
use kimi_ui_core::{
    ComponentNode, EntryOp, EntryStore, InputEvent, Key, KeyEvent, NodeId, RenderBackend, render,
};
use kimi_ui_terminal::{Mode, TerminalBackend};

/// What the UI tells the session to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiCommand {
    /// Run this prompt as the next turn.
    Prompt(String),
    /// Stop the turn in flight.
    Cancel,
}

/// The host's side of the contract: it forwards commands to the engine.
///
/// Object-safe on purpose — a host that owns an engine handle stores it behind
/// `dyn`, and the app never learns which transport it is talking to.
pub trait UiTransport {
    fn send(&mut self, command: UiCommand);
}

/// A transport that records instead of sending, for tests and for a
/// scrollback-only preview.
#[derive(Debug, Default)]
pub struct RecordingTransport {
    commands: Vec<UiCommand>,
}

impl RecordingTransport {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn commands(&self) -> &[UiCommand] {
        &self.commands
    }

    /// Take everything recorded since the last call.
    pub fn take(&mut self) -> Vec<UiCommand> {
        std::mem::take(&mut self.commands)
    }
}

impl UiTransport for RecordingTransport {
    fn send(&mut self, command: UiCommand) {
        self.commands.push(command);
    }
}

/// The vertical slice's UI half: startup, paint, stream, prompt, cancel.
///
/// Re-deriving the whole frame on every [`App::draw`] is deliberate for the
/// slice: the entry table is small, and a per-entry cache would be the thing to
/// get wrong first. The diff that keeps the terminal from flickering lives in
/// the backend, which only prints what is new.
pub struct App<W: Write, T: UiTransport> {
    backend: TerminalBackend<W>,
    transport: T,
    store: EntryStore,
    /// The line being typed below the transcript.
    buffer: String,
    /// Columns the transcript is laid out for.
    width: usize,
    /// Nodes mounted for the current frame, replaced on every draw.
    nodes: Vec<NodeId>,
}

impl<W: Write, T: UiTransport> App<W, T> {
    /// A scrollback-mode app: the transcript appends to the terminal's own
    /// history, which is what the slice promises (Esc stops the turn, and the
    /// output that already arrived stays on screen).
    pub fn new(out: W, transport: T) -> Self {
        Self::with_mode(Mode::Scrollback, out, transport)
    }

    pub fn with_mode(mode: Mode, out: W, transport: T) -> Self {
        Self {
            backend: TerminalBackend::new(mode, out),
            transport,
            store: EntryStore::new(),
            buffer: String::new(),
            width: DEFAULT_PAINT_WIDTH,
            nodes: Vec::new(),
        }
    }

    /// Feed one op from the engine's projection.
    pub fn apply(&mut self, op: EntryOp) {
        self.store.apply(op);
    }

    /// Apply a batch in order.
    pub fn apply_all(&mut self, ops: impl IntoIterator<Item = EntryOp>) {
        for op in ops {
            self.store.apply(op);
        }
    }

    pub fn store(&self) -> &EntryStore {
        &self.store
    }

    /// The line being typed.
    pub fn prompt_buffer(&self) -> &str {
        &self.buffer
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn backend(&self) -> &TerminalBackend<W> {
        &self.backend
    }

    pub fn transport(&self) -> &T {
        &self.transport
    }

    pub fn into_parts(self) -> (TerminalBackend<W>, T) {
        (self.backend, self.transport)
    }

    /// Lay the transcript out at the current width and paint what changed.
    pub fn draw(&mut self) {
        self.rebuild_nodes();
        self.backend.request_render(false);
        self.backend.render();
    }

    /// Repaint everything, for a terminal that was cleared underneath us or a
    /// resize that invalidated every line.
    pub fn redraw(&mut self) {
        self.rebuild_nodes();
        self.backend.request_render(true);
        self.backend.render();
    }

    /// Handle one decoded input event.
    pub fn handle(&mut self, event: InputEvent) {
        match event {
            InputEvent::Resize { columns, .. } => {
                self.resize(columns as usize);
            }
            InputEvent::Paste(text) => self.buffer.push_str(&text),
            InputEvent::Key(key) => self.handle_key(key),
            // The slice has no scroll model yet; the event stays queued in the
            // backend for whichever component grows one.
            InputEvent::Mouse(_) => {}
        }
    }

    fn handle_key(&mut self, event: KeyEvent) {
        if event.is_cancel() || event.key == Key::Esc {
            self.transport.send(UiCommand::Cancel);
            return;
        }
        match event.key {
            Key::Enter => {
                let prompt = self.buffer.trim().to_string();
                self.buffer.clear();
                if !prompt.is_empty() {
                    self.transport.send(UiCommand::Prompt(prompt));
                }
            }
            Key::Backspace => {
                self.buffer.pop();
            }
            // A chord is not text: Ctrl-D or Alt-b must not insert a character.
            Key::Char(character) if !event.modifiers.ctrl && !event.modifiers.alt => {
                self.buffer.push(character);
            }
            _ => {}
        }
    }

    fn resize(&mut self, columns: usize) {
        let width = columns.max(1);
        if width == self.width {
            return;
        }
        self.width = width;
        self.backend.dispatch_input(InputEvent::Resize {
            columns: width.min(u16::MAX as usize) as u16,
            rows: 0,
        });
        // Every line was wrapped for the old width, so nothing above the
        // divergence point can be corrected in scrollback: repaint in full.
        self.redraw();
    }

    /// Replace the frame's nodes with the current entry table.
    ///
    /// Remounting rather than diffing nodes is the slice's simplification; the
    /// ids change, the painted lines do not, and the backend's own diff is what
    /// decides how much reaches the terminal.
    fn rebuild_nodes(&mut self) {
        for id in self.nodes.drain(..) {
            self.backend.unmount(id);
        }
        for (line, style) in render::entry_lines(&self.store, self.width) {
            if let Some(id) = self.backend.mount(ComponentNode::styled(line, style), None) {
                self.nodes.push(id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kimi_ui_core::{AppendDelta, EntryContent, EntryId, EntryKind, SeqSpan};

    fn painted(app: &App<Vec<u8>, RecordingTransport>) -> String {
        String::from_utf8_lossy(app.backend().writer()).into_owned()
    }

    fn app() -> App<Vec<u8>, RecordingTransport> {
        App::new(Vec::new(), RecordingTransport::new())
    }

    fn upsert(id: &EntryId, text: &str) -> EntryOp {
        EntryOp::Upsert {
            id: id.clone(),
            content: EntryContent::new(id.kind, text),
        }
    }

    #[test]
    fn typing_a_line_and_pressing_enter_sends_a_prompt() {
        let mut app = app();
        for character in ['h', 'i'] {
            app.handle(InputEvent::Key(KeyEvent::new(Key::Char(character))));
        }
        assert_eq!(app.prompt_buffer(), "hi");
        app.handle(InputEvent::Key(KeyEvent::new(Key::Enter)));
        assert_eq!(app.transport().commands(), [UiCommand::Prompt("hi".into())]);
        assert_eq!(app.prompt_buffer(), "", "the line is consumed");
    }

    #[test]
    fn an_empty_line_is_not_sent() {
        let mut app = app();
        app.handle(InputEvent::Key(KeyEvent::new(Key::Enter)));
        assert!(app.transport().commands().is_empty());
    }

    #[test]
    fn a_whitespace_only_line_is_not_sent() {
        let mut app = app();
        for character in [' ', '\t'] {
            app.handle(InputEvent::Key(KeyEvent::new(Key::Char(character))));
        }
        app.handle(InputEvent::Key(KeyEvent::new(Key::Enter)));
        assert!(app.transport().commands().is_empty());
    }

    #[test]
    fn escape_and_ctrl_c_both_cancel() {
        let mut app = app();
        app.handle(InputEvent::Key(KeyEvent::new(Key::Esc)));
        app.handle(InputEvent::Key(KeyEvent {
            key: Key::Char('c'),
            modifiers: kimi_ui_core::Modifiers::ctrl(),
        }));
        assert_eq!(
            app.transport().commands(),
            [UiCommand::Cancel, UiCommand::Cancel]
        );
    }

    #[test]
    fn a_control_chord_does_not_insert_text() {
        let mut app = app();
        app.handle(InputEvent::Key(KeyEvent {
            key: Key::Char('d'),
            modifiers: kimi_ui_core::Modifiers::ctrl(),
        }));
        assert_eq!(app.prompt_buffer(), "");
    }

    #[test]
    fn backspace_removes_the_last_character_only() {
        let mut app = app();
        for character in ['a', 'b'] {
            app.handle(InputEvent::Key(KeyEvent::new(Key::Char(character))));
        }
        app.handle(InputEvent::Key(KeyEvent::new(Key::Backspace)));
        assert_eq!(app.prompt_buffer(), "a");
        app.handle(InputEvent::Key(KeyEvent::new(Key::Backspace)));
        app.handle(InputEvent::Key(KeyEvent::new(Key::Backspace)));
        assert_eq!(app.prompt_buffer(), "", "an empty buffer is not an error");
    }

    #[test]
    fn a_paste_lands_in_the_buffer() {
        let mut app = app();
        app.handle(InputEvent::Paste("multi\nline".into()));
        assert_eq!(app.prompt_buffer(), "multi\nline");
    }

    #[test]
    fn the_transcript_is_painted_with_its_kind_gutter() {
        let mut app = app();
        let user = EntryId::engine("t1", 0, EntryKind::UserMsg);
        let assistant = EntryId::engine("t1", 0, EntryKind::AssistantMsg);
        app.apply(upsert(&user, "question"));
        app.apply(upsert(&assistant, "answer"));
        app.draw();
        let out = painted(&app);
        assert!(out.contains(">| question"), "{out:?}");
        assert!(out.contains(" | answer"), "{out:?}");
    }

    #[test]
    fn streaming_appends_only_the_new_tail_on_scrollback() {
        let mut app = app();
        let assistant = EntryId::engine("t1", 0, EntryKind::AssistantMsg);
        let user = EntryId::engine("t1", 0, EntryKind::UserMsg);
        app.apply(upsert(&user, "q"));
        app.apply(upsert(&assistant, ""));
        app.apply(EntryOp::Append {
            id: assistant.clone(),
            delta: AppendDelta {
                text: "one".into(),
                span: SeqSpan::new(0, 0),
                is_final: false,
            },
        });
        app.draw();
        assert!(painted(&app).contains(" | one"));

        app.apply(EntryOp::Append {
            id: assistant,
            delta: AppendDelta {
                text: " two".into(),
                span: SeqSpan::new(1, 1),
                is_final: true,
            },
        });
        app.draw();
        let out = painted(&app);
        assert!(out.contains(" | one two"), "{out:?}");
        assert_eq!(
            out.matches(">| q").count(),
            1,
            "the user row above was not reprinted"
        );
    }

    #[test]
    fn a_draw_with_no_change_prints_nothing_new() {
        let mut app = app();
        app.apply(upsert(&EntryId::engine("t1", 0, EntryKind::UserMsg), "q"));
        app.draw();
        let before = painted(&app).len();
        app.draw();
        assert_eq!(painted(&app).len(), before);
    }

    #[test]
    fn a_resize_rewraps_instead_of_truncating() {
        let mut app = app();
        app.apply(upsert(
            &EntryId::engine("t1", 0, EntryKind::AssistantMsg),
            "alpha beta gamma delta epsilon",
        ));
        app.handle(InputEvent::Resize {
            columns: 14,
            rows: 20,
        });
        app.draw();
        assert_eq!(app.width(), 14);
        let out = painted(&app);
        assert!(out.contains("alpha"), "{out:?}");
        assert!(out.contains("epsilon"), "no text is lost: {out:?}");
        // The wrapped rows align under the body rather than the gutter.
        assert!(out.contains("\r\n   "), "{out:?}");
    }

    #[test]
    fn esc_stops_the_turn_without_losing_what_arrived() {
        let mut app = app();
        app.apply(upsert(
            &EntryId::engine("t1", 0, EntryKind::AssistantMsg),
            "partial answer",
        ));
        app.draw();
        app.handle(InputEvent::Key(KeyEvent::new(Key::Esc)));
        assert_eq!(app.transport().commands(), [UiCommand::Cancel]);
        assert!(painted(&app).contains("partial answer"));
    }

    #[test]
    fn a_turn_end_is_painted_as_a_status_entry() {
        let mut app = app();
        app.apply(upsert(
            &EntryId::engine("t1", 0, EntryKind::Status),
            "turn ended",
        ));
        app.draw();
        assert!(painted(&app).contains("*| turn ended"));
    }

    #[test]
    fn an_alt_screen_app_repaints_and_leaves_cleanly() {
        let mut app = App::with_mode(Mode::AltScreen, Vec::new(), RecordingTransport::new());
        app.apply(upsert(&EntryId::engine("t1", 0, EntryKind::UserMsg), "q"));
        app.draw();
        let out = painted(&app);
        assert!(out.contains("\x1b[H"), "the viewport is repainted in place");
        assert!(out.contains(">| q"));
    }
}
