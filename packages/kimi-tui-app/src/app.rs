//! The application's shared state and its link to a live session.
//!
//! The widgets are handed an `Arc<UiState>` and read from it on every frame;
//! the host that owns an engine handle feeds the same value. That is why the
//! state is behind `Mutex`es rather than living on the app: textual-rs renders
//! from `&self` on its own tick, so a value the engine updates between frames
//! has to be reachable without the app taking a mutable borrow of anything.
//!
//! No repaint plumbing is needed for the same reason. textual-rs re-renders on
//! its tick, so an op applied from another task shows up on the next frame.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use kimi_ui_core::{EntryOp, EntryStore, InputEvent, Key, KeyEvent};

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
pub trait UiTransport: Send {
    fn send(&mut self, command: UiCommand);
}

/// A transport that records instead of sending, for tests and for a
/// scrollback-only preview.
///
/// The log is shared, so the test keeps a handle and reads what the UI asked
/// for while the app owned the transport.
#[derive(Debug, Clone, Default)]
pub struct RecordingTransport {
    commands: Arc<Mutex<Vec<UiCommand>>>,
}

impl RecordingTransport {
    pub fn new() -> Self {
        Self::default()
    }

    /// A second handle on the same log.
    pub fn log(&self) -> Arc<Mutex<Vec<UiCommand>>> {
        Arc::clone(&self.commands)
    }
}

impl UiTransport for RecordingTransport {
    fn send(&mut self, command: UiCommand) {
        self.commands
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(command);
    }
}

/// Everything a frame reads, and the only place the UI mutates it.
pub struct UiState {
    store: Mutex<EntryStore>,
    prompt: Mutex<String>,
    transport: Mutex<Box<dyn UiTransport>>,
}

impl UiState {
    pub fn new(transport: impl UiTransport + 'static) -> Arc<Self> {
        Arc::new(Self {
            store: Mutex::new(EntryStore::new()),
            prompt: Mutex::new(String::new()),
            transport: Mutex::new(Box::new(transport)),
        })
    }

    /// Apply one op from the engine's projection.
    pub fn apply(&self, op: EntryOp) {
        lock(&self.store).apply(op);
    }

    /// Apply a batch in order.
    pub fn apply_all(&self, ops: impl IntoIterator<Item = EntryOp>) {
        let mut store = lock(&self.store);
        for op in ops {
            store.apply(op);
        }
    }

    /// Run `read` against the entry table. Kept as a closure so the store's
    /// lock is never handed out: a `MutexGuard` escaping into a widget would
    /// let one render hold it across another widget's.
    pub fn with_store<R>(&self, read: impl FnOnce(&EntryStore) -> R) -> R {
        read(&lock(&self.store))
    }

    /// The line being typed.
    pub fn prompt(&self) -> String {
        lock(&self.prompt).clone()
    }

    /// Handle one decoded input event. Returns whether it consumed the event.
    pub fn handle_input(&self, event: InputEvent) -> bool {
        match event {
            // A paste is text, not keystrokes: it never submits or cancels.
            InputEvent::Paste(text) => {
                lock(&self.prompt).push_str(&text);
                true
            }
            InputEvent::Key(key) => self.handle_key(key),
            // The slice has no scroll model yet.
            InputEvent::Resize { .. } | InputEvent::Mouse(_) => false,
        }
    }

    fn handle_key(&self, event: KeyEvent) -> bool {
        if event.is_cancel() || event.key == Key::Esc {
            self.send(UiCommand::Cancel);
            return true;
        }
        match event.key {
            Key::Enter => {
                let prompt = lock(&self.prompt).trim().to_string();
                self.prompt
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .clear();
                if !prompt.is_empty() {
                    self.send(UiCommand::Prompt(prompt));
                }
                true
            }
            Key::Backspace => {
                lock(&self.prompt).pop();
                true
            }
            // A chord is not text: Ctrl-D or Alt-b must not insert a character.
            Key::Char(character) if !event.modifiers.ctrl && !event.modifiers.alt => {
                lock(&self.prompt).push(character);
                true
            }
            _ => false,
        }
    }

    fn send(&self, command: UiCommand) {
        lock(&self.transport).send(command);
    }
}

/// A poisoned lock is not a reason to stop painting: the value behind it is
/// still readable, and a UI that panics on its own render is worse than one
/// that shows the last good frame.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Build the textual-rs app around `state` and take over the terminal.
///
/// Returns when the user quits. The host keeps its engine handle and its
/// transport; this function only owns the screen.
pub fn run_terminal(state: Arc<UiState>) -> anyhow::Result<()> {
    let screen_state = Arc::clone(&state);
    let mut app = textual_rs::App::new(move || {
        Box::new(crate::screen::AppScreen::new(Arc::clone(&screen_state)))
    })
    .with_css(crate::screen::AppScreen::CSS);
    app.run()
}
