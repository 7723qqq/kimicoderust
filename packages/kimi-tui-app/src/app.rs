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

use kimi_ui_core::{
    EntryId, EntryOp, EntryStore, InputEvent, Key, KeyEvent, SteerDelivery, SubmissionState,
};

/// What the UI tells the session to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiCommand {
    /// Run this prompt as the next turn.
    Prompt(String),
    /// Hand this message to the turn in flight (v2 #4627).
    Steer(String),
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

pub struct ApprovalResponse {
    pub allowed: bool,
    pub reason: Option<String>,
    pub remember: bool,
}

pub struct PendingApproval {
    pub tool_name: String,
    pub action: String,
    pub reason: Option<String>,
    pub arguments: serde_json::Value,
    pub session_approval_rule: Option<String>,
    #[allow(clippy::type_complexity)]
    responder: std::sync::Mutex<Option<Box<dyn FnOnce(ApprovalResponse) + Send>>>,
}

impl PendingApproval {
    pub fn new<F>(
        tool_name: String,
        action: String,
        reason: Option<String>,
        arguments: serde_json::Value,
        session_approval_rule: Option<String>,
        responder: F,
    ) -> Self
    where
        F: FnOnce(ApprovalResponse) + Send + 'static,
    {
        Self {
            tool_name,
            action,
            reason,
            arguments,
            session_approval_rule,
            responder: std::sync::Mutex::new(Some(Box::new(responder))),
        }
    }

    pub fn answer(&self, response: ApprovalResponse) {
        if let Some(callback) = self
            .responder
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            callback(response);
        }
    }
}

pub struct QuestionResponse {
    pub answers: std::collections::HashMap<String, String>,
    pub method: Option<String>,
    pub cancelled: bool,
}

pub struct PendingQuestion {
    pub questions: Vec<String>,
    pub options: Vec<Vec<String>>,
    pub selected: std::sync::Mutex<Vec<Option<usize>>>,
    #[allow(clippy::type_complexity)]
    responder: std::sync::Mutex<Option<Box<dyn FnOnce(QuestionResponse) + Send>>>,
}

impl PendingQuestion {
    pub fn new<F>(questions: Vec<String>, options: Vec<Vec<String>>, responder: F) -> Self
    where
        F: FnOnce(QuestionResponse) + Send + 'static,
    {
        Self {
            questions: questions.clone(),
            options,
            selected: std::sync::Mutex::new(vec![None; questions.len()]),
            responder: std::sync::Mutex::new(Some(Box::new(responder))),
        }
    }

    pub fn select(&self, q: usize, option: usize) {
        let mut selected = self
            .selected
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(slot) = selected.get_mut(q) {
            *slot = Some(option);
        }
    }

    pub fn selection(&self) -> Vec<Option<usize>> {
        self.selected
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn answer(&self) {
        if let Some(cb) = self
            .responder
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let mut answers = std::collections::HashMap::new();
            for (idx, q) in self.questions.iter().enumerate() {
                if let Some(Some(opt_idx)) = self.selection().get(idx)
                    && let Some(opt) = self.options.get(idx).and_then(|o| o.get(*opt_idx))
                {
                    answers.insert(q.clone(), opt.clone());
                }
            }
            cb(QuestionResponse {
                answers,
                method: Some("cli".into()),
                cancelled: false,
            });
        }
    }

    pub fn dismiss(&self) {
        if let Some(cb) = self
            .responder
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            cb(QuestionResponse {
                answers: std::collections::HashMap::new(),
                method: None,
                cancelled: true,
            });
        }
    }
}

/// Everything a frame reads, and the only place the UI mutates it.
pub struct UiState {
    store: Mutex<EntryStore>,
    prompt: Mutex<String>,
    pending_approval: Mutex<Option<std::sync::Arc<PendingApproval>>>,
    pending_question: Mutex<Option<std::sync::Arc<PendingQuestion>>>,
    /// Char-offset of the caret inside [`Self::prompt`].
    cursor: Mutex<usize>,
    /// One line under the transcript: a turn that stopped, a retry countdown.
    /// Composed and localized by the host; the UI only decides where it sits.
    status: Mutex<Option<String>>,
    /// Whether a turn is in flight. Enter steers while it is set, and starts a
    /// new turn while it is not.
    turn_running: Mutex<bool>,
    /// Steered rows waiting to be read (v2 #4627).
    steer: Mutex<SteerDelivery>,
    /// Every applied op, in order. Feature-gated: an integration test needs to
    /// see the projection seam (which entry was pended and which was cleared),
    /// and inferring that from the final store would prove nothing.
    #[cfg(feature = "ops-trace")]
    ops: Mutex<Vec<EntryOp>>,
    /// The prompt id of the last steer the engine accepted. Feature-gated for
    /// the same reason: a test compares it with `turn.steer.promptIds`.
    #[cfg(feature = "ops-trace")]
    last_steer_prompt: Mutex<Option<String>>,
    transport: Mutex<Box<dyn UiTransport>>,
}

impl UiState {
    pub fn new(transport: impl UiTransport + 'static) -> Arc<Self> {
        Arc::new(Self {
            store: Mutex::new(EntryStore::new()),
            prompt: Mutex::new(String::new()),
            pending_approval: Mutex::new(None),
            pending_question: Mutex::new(None),
            cursor: Mutex::new(0),
            status: Mutex::new(None),
            turn_running: Mutex::new(false),
            steer: Mutex::new(SteerDelivery::new()),
            #[cfg(feature = "ops-trace")]
            ops: Mutex::new(Vec::new()),
            #[cfg(feature = "ops-trace")]
            last_steer_prompt: Mutex::new(None),
            transport: Mutex::new(Box::new(transport)),
        })
    }

    /// Apply one op from the engine's projection.
    pub fn apply(&self, op: EntryOp) {
        #[cfg(feature = "ops-trace")]
        lock(&self.ops).push(op.clone());
        lock(&self.store).apply(op);
    }

    /// Apply a batch in order.
    pub fn apply_all(&self, ops: impl IntoIterator<Item = EntryOp>) {
        let mut store = lock(&self.store);
        for op in ops {
            #[cfg(feature = "ops-trace")]
            lock(&self.ops).push(op.clone());
            store.apply(op);
        }
    }

    /// Every applied op, in arrival order. `ops-trace` only.
    #[cfg(feature = "ops-trace")]
    pub fn ops_snapshot(&self) -> Vec<EntryOp> {
        lock(&self.ops).clone()
    }

    /// The prompt id of the last steer the engine accepted. `ops-trace` only.
    #[cfg(feature = "ops-trace")]
    pub fn last_steer_prompt_id(&self) -> Option<String> {
        lock(&self.last_steer_prompt).clone()
    }

    /// Run `read` against the entry table. Kept as a closure so the store's
    /// lock is never handed out: a `MutexGuard` escaping into a widget would
    /// let one render hold it across another widget's.
    pub fn with_store<R>(&self, read: impl FnOnce(&EntryStore) -> R) -> R {
        read(&lock(&self.store))
    }

    pub fn set_pending_approval(&self, approval: std::sync::Arc<PendingApproval>) {
        *lock(&self.pending_approval) = Some(approval);
    }

    pub fn clear_pending_approval(&self) {
        let _ = lock(&self.pending_approval).take();
    }

    pub fn pending_approval(&self) -> Option<std::sync::Arc<PendingApproval>> {
        lock(&self.pending_approval).clone()
    }

    pub fn set_pending_question(&self, question: std::sync::Arc<PendingQuestion>) {
        *lock(&self.pending_question) = Some(question);
    }

    pub fn clear_pending_question(&self) {
        let _ = lock(&self.pending_question).take();
    }

    pub fn pending_question(&self) -> Option<std::sync::Arc<PendingQuestion>> {
        lock(&self.pending_question).clone()
    }

    /// The line being typed.
    pub fn prompt(&self) -> String {
        lock(&self.prompt).clone()
    }

    /// The caret's char-offset inside the line being typed.
    pub fn prompt_cursor(&self) -> usize {
        lock(&self.cursor).min(self.prompt().chars().count())
    }

    /// Show (or clear, with `None`) the status line.
    pub fn set_status(&self, status: Option<String>) {
        *lock(&self.status) = status;
    }

    /// The status line's current text.
    pub fn status(&self) -> Option<String> {
        lock(&self.status).clone()
    }

    /// Whether a turn is in flight. The host sets it from the turn lifecycle;
    /// the input layer reads it to decide prompt-vs-steer.
    pub fn set_turn_running(&self, running: bool) {
        *lock(&self.turn_running) = running;
    }

    pub fn turn_running(&self) -> bool {
        *lock(&self.turn_running)
    }

    /// Register a submitted steer: its rows are dimmed until the engine reads
    /// them. The host calls this after creating the entry.
    pub fn steer_submitted(&self, entries: Vec<EntryId>) {
        let ops = lock(&self.steer).note(entries);
        self.apply_all(ops);
    }

    /// Apply the engine's receipt for the submitted batch.
    pub fn steer_receipt(
        &self,
        entries: &[EntryId],
        prompt_id: &str,
        state: SubmissionState,
        steered: bool,
    ) {
        #[cfg(feature = "ops-trace")]
        if steered {
            *lock(&self.last_steer_prompt) = Some(prompt_id.to_string());
        }
        let ops = lock(&self.steer).apply_submission(entries, prompt_id, state, steered);
        self.apply_all(ops);
    }

    /// The engine consumed these steers into the running turn.
    pub fn steer_acknowledge(&self, prompt_ids: &[String]) {
        let ops = lock(&self.steer).acknowledge(prompt_ids);
        self.apply_all(ops);
    }

    /// A queued steer's own turn started.
    pub fn steer_acknowledge_started(&self, prompt_id: Option<&str>) {
        let ops = lock(&self.steer).acknowledge_started(prompt_id);
        self.apply_all(ops);
    }

    /// A pending steer stopped without being read.
    pub fn steer_release(&self, prompt_id: &str) {
        let ops = lock(&self.steer).release_prompt(prompt_id);
        self.apply_all(ops);
    }

    /// A local dispatch failure: stop showing the batch as pending.
    pub fn steer_release_entries(&self, entries: &[EntryId]) {
        let ops = lock(&self.steer).release_entries(entries);
        self.apply_all(ops);
    }

    /// Drop every pending steer (session reset / replay).
    pub fn reset_steer(&self) {
        let ops = lock(&self.steer).reset();
        self.apply_all(ops);
    }

    /// Handle one decoded input event. Returns whether it consumed the event.
    pub fn handle_input(&self, event: InputEvent) -> bool {
        match event {
            // A paste is text, not keystrokes: it never submits or cancels.
            InputEvent::Paste(text) => {
                self.insert_text(&text);
                true
            }
            InputEvent::Key(key) => self.handle_key(key),
            // The slice has no scroll model yet.
            InputEvent::Resize { .. } | InputEvent::Mouse(_) => false,
        }
    }

    fn handle_key(&self, event: KeyEvent) -> bool {
        if let Some(approval) = self.pending_approval() {
            match event.key {
                Key::Char('y') | Key::Char('Y') => {
                    approval.answer(ApprovalResponse {
                        allowed: true,
                        reason: None,
                        remember: false,
                    });
                    self.clear_pending_approval();
                    return true;
                }
                Key::Char('a') | Key::Char('A') => {
                    approval.answer(ApprovalResponse {
                        allowed: true,
                        reason: None,
                        remember: approval.session_approval_rule.is_some(),
                    });
                    self.clear_pending_approval();
                    return true;
                }
                Key::Char('n') | Key::Char('N') | Key::Esc => {
                    approval.answer(ApprovalResponse {
                        allowed: false,
                        reason: Some("User denied tool execution".into()),
                        remember: false,
                    });
                    self.clear_pending_approval();
                    return true;
                }
                _ => return true,
            }
        }
        if let Some(question) = self.pending_question() {
            match event.key {
                Key::Char(c) if c.is_ascii_digit() => {
                    if let Some(n) = c
                        .to_digit(10)
                        .map(|d| d as usize)
                        .filter(|&d| d >= 1 && d <= question.options.len())
                    {
                        question.select(0, n - 1);
                    }
                    return true;
                }
                Key::Enter => {
                    question.answer();
                    self.clear_pending_question();
                    return true;
                }
                Key::Esc => {
                    question.dismiss();
                    self.clear_pending_question();
                    return true;
                }
                _ => return true,
            }
        }
        if event.is_cancel() || event.key == Key::Esc {
            self.send(UiCommand::Cancel);
            return true;
        }
        match event.key {
            Key::Enter => {
                let prompt = lock(&self.prompt).trim().to_string();
                lock(&self.prompt).clear();
                *lock(&self.cursor) = 0;
                if !prompt.is_empty() {
                    // A turn in flight gets the message as a steer; otherwise
                    // it starts one. Same key, decided by the lifecycle
                    // (v2 #4627).
                    if self.turn_running() {
                        self.send(UiCommand::Steer(prompt));
                    } else {
                        self.send(UiCommand::Prompt(prompt));
                    }
                }
                true
            }
            Key::Backspace => self.delete_before(),
            Key::Delete => self.delete_after(),
            Key::Left => {
                let chars = self.prompt().chars().count();
                let mut cursor = lock(&self.cursor);
                if *cursor > 0 {
                    *cursor -= 1;
                }
                *cursor = (*cursor).min(chars);
                drop(cursor);
                true
            }
            Key::Right => {
                let chars = self.prompt().chars().count();
                let mut cursor = lock(&self.cursor);
                if *cursor < chars {
                    *cursor += 1;
                }
                drop(cursor);
                true
            }
            Key::Home => {
                *lock(&self.cursor) = 0;
                true
            }
            Key::End => {
                *lock(&self.cursor) = self.prompt().chars().count();
                true
            }
            // Readline-style chords and word movement, so a user accustomed
            // to a terminal editor keeps working. Each conjugate is handled
            // explicitly instead of falling through to "not text", because the
            // contract already decoded these keys.
            Key::Char('a') if event.modifiers.ctrl && !event.modifiers.alt => {
                *lock(&self.cursor) = 0;
                true
            }
            Key::Char('e') if event.modifiers.ctrl && !event.modifiers.alt => {
                *lock(&self.cursor) = self.prompt().chars().count();
                true
            }
            Key::Char('b') if event.modifiers.ctrl && !event.modifiers.alt => {
                let chars = self.prompt().chars().count();
                let mut cursor = lock(&self.cursor);
                if *cursor > 0 {
                    *cursor -= 1;
                }
                *cursor = (*cursor).min(chars);
                drop(cursor);
                true
            }
            Key::Char('f') if event.modifiers.ctrl && !event.modifiers.alt => {
                let chars = self.prompt().chars().count();
                let mut cursor = lock(&self.cursor);
                if *cursor < chars {
                    *cursor += 1;
                }
                drop(cursor);
                true
            }
            Key::Char('h') if event.modifiers.ctrl && !event.modifiers.alt => {
                self.delete_before();
                true
            }
            Key::Char('d') if event.modifiers.ctrl && !event.modifiers.alt => {
                self.delete_after();
                true
            }
            Key::Char('k') if event.modifiers.ctrl && !event.modifiers.alt => {
                self.kill_to_end();
                true
            }
            Key::Char('u') if event.modifiers.ctrl && !event.modifiers.alt => {
                self.kill_to_start();
                true
            }
            Key::Char('w') if event.modifiers.ctrl && !event.modifiers.alt => {
                self.kill_word_backward();
                true
            }
            Key::Char('b') if event.modifiers.alt && !event.modifiers.ctrl => {
                *lock(&self.cursor) = self.prev_word();
                true
            }
            Key::Char('f') if event.modifiers.alt && !event.modifiers.ctrl => {
                *lock(&self.cursor) = self.next_word();
                true
            }
            Key::Char('d') if event.modifiers.alt && !event.modifiers.ctrl => {
                self.kill_word_forward();
                true
            }
            // A plain chord (Ctrl-D on an empty line) still must not insert.
            // A chord is not text: Ctrl-D or Alt-b must not insert a character.
            Key::Char(character) if !event.modifiers.ctrl && !event.modifiers.alt => {
                self.insert_char(character);
                true
            }
            _ => false,
        }
    }

    fn insert_text(&self, text: &str) {
        // Inserting through the whole string keeps the multi-byte offset arithmetic
        // in one place: `cursor` is a char count, and the string is rebuilt on
        // each edit. Prompts are short enough for this slice; a real editor would
        // splice, but correctness beats speed here.
        let cursor = lock(&self.cursor).min(self.prompt().chars().count());
        let chars: Vec<char> = self.prompt().chars().collect();
        let mut prompt = lock(&self.prompt);
        let mut result: String = chars[..cursor].iter().collect();
        result.push_str(text);
        result.extend(chars[cursor..].iter());
        *prompt = result;
        *lock(&self.cursor) = cursor + text.chars().count();
    }

    fn insert_char(&self, character: char) {
        let cursor = lock(&self.cursor).min(self.prompt().chars().count());
        let chars: Vec<char> = self.prompt().chars().collect();
        let mut prompt = lock(&self.prompt);
        let mut result: String = chars[..cursor].iter().collect();
        result.push(character);
        result.extend(chars[cursor..].iter());
        *prompt = result;
        *lock(&self.cursor) = cursor + 1;
    }

    fn kill_to_end(&self) {
        let cursor = lock(&self.cursor).min(self.prompt().chars().count());
        let chars: Vec<char> = self.prompt().chars().collect();
        let mut prompt = lock(&self.prompt);
        *prompt = chars[..cursor].iter().collect();
        // cursor stays where the text ended.
    }

    fn kill_to_start(&self) {
        let cursor = lock(&self.cursor).min(self.prompt().chars().count());
        let chars: Vec<char> = self.prompt().chars().collect();
        let mut prompt = lock(&self.prompt);
        *prompt = chars[cursor..].iter().collect();
        *lock(&self.cursor) = 0;
    }

    fn kill_word_backward(&self) {
        let cursor = lock(&self.cursor).min(self.prompt().chars().count());
        let target = self.prev_word();
        let chars: Vec<char> = self.prompt().chars().collect();
        let mut prompt = lock(&self.prompt);
        let mut result: String = chars[..target].iter().collect();
        result.extend(chars[cursor..].iter());
        *prompt = result;
        *lock(&self.cursor) = target;
    }

    fn kill_word_forward(&self) {
        let cursor = lock(&self.cursor).min(self.prompt().chars().count());
        let target = self.next_word();
        let chars: Vec<char> = self.prompt().chars().collect();
        let mut prompt = lock(&self.prompt);
        let mut result: String = chars[..cursor].iter().collect();
        result.extend(chars[target..].iter());
        *prompt = result;
    }

    fn prev_word(&self) -> usize {
        let chars: Vec<char> = self.prompt().chars().collect();
        let mut i = lock(&self.cursor).min(chars.len());
        while i > 0 && !is_word_char(chars[i - 1]) {
            i -= 1;
        }
        while i > 0 && is_word_char(chars[i - 1]) {
            i -= 1;
        }
        i
    }

    fn next_word(&self) -> usize {
        let chars: Vec<char> = self.prompt().chars().collect();
        let len = chars.len();
        let mut i = lock(&self.cursor).min(len);
        while i < len && !is_word_char(chars[i]) {
            i += 1;
        }
        while i < len && is_word_char(chars[i]) {
            i += 1;
        }
        i
    }

    fn delete_before(&self) -> bool {
        let cursor = lock(&self.cursor).min(self.prompt().chars().count());
        if cursor == 0 {
            return true;
        }
        let chars: Vec<char> = self.prompt().chars().collect();
        let mut prompt = lock(&self.prompt);
        let mut result: String = chars[..cursor - 1].iter().collect();
        result.extend(chars[cursor..].iter());
        *prompt = result;
        *lock(&self.cursor) = cursor - 1;
        true
    }

    fn delete_after(&self) -> bool {
        let cursor = lock(&self.cursor).min(self.prompt().chars().count());
        let chars: Vec<char> = self.prompt().chars().collect();
        let count = chars.len();
        if cursor >= count {
            return true;
        }
        let mut prompt = lock(&self.prompt);
        let mut result: String = chars[..cursor].iter().collect();
        result.extend(chars[cursor + 1..].iter());
        *prompt = result;
        true
    }

    fn send(&self, command: UiCommand) {
        lock(&self.transport).send(command);
    }
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
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

#[cfg(test)]
mod tests {
    use super::*;
    use kimi_ui_core::{EntryContent, EntryKind};

    #[test]
    fn enter_steers_while_a_turn_is_running() {
        let transport = RecordingTransport::new();
        let log = transport.log();
        let state = UiState::new(transport);
        lock(&state.prompt).push_str("steer me");
        state.set_turn_running(true);

        state.handle_input(InputEvent::Key(KeyEvent::new(Key::Enter)));
        assert_eq!(*lock(&log), vec![UiCommand::Steer("steer me".into())]);

        lock(&state.prompt).push_str("next turn");
        state.set_turn_running(false);
        state.handle_input(InputEvent::Key(KeyEvent::new(Key::Enter)));
        assert_eq!(
            lock(&log).last(),
            Some(&UiCommand::Prompt("next turn".into()))
        );
    }

    #[test]
    fn a_steered_row_is_pending_until_the_engine_reads_it() {
        let state = UiState::new(RecordingTransport::new());
        let id = EntryId::engine("turn-0", 0, EntryKind::UserMsg);
        state.apply(EntryOp::Upsert {
            id: id.clone(),
            content: EntryContent::new(EntryKind::UserMsg, "steer me"),
        });
        state.steer_submitted(vec![id.clone()]);
        assert!(state.with_store(|store| store.is_pending(&id)));

        state.steer_receipt(
            std::slice::from_ref(&id),
            "prompt-1",
            SubmissionState::Running,
            true,
        );
        assert!(state.with_store(|store| store.is_pending(&id)));

        let prompt_id = "prompt-1".to_string();
        state.steer_acknowledge(std::slice::from_ref(&prompt_id));
        assert!(!state.with_store(|store| store.is_pending(&id)));
    }
}
