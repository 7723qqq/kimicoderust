//! The UI half of the vertical slice, on the real widget stack.
//!
//! `textual-rs` mounts the screen into a test terminal and hands back the
//! rendered `Buffer`, so these assertions are on painted cells — the same thing
//! the terminal would show — rather than on an intermediate row model. What is
//! pinned here:
//!
//! - a recorded run of `EntryOp`s paints the transcript, gutter and wrapping
//!   included;
//! - typing reaches the shared state and Enter submits it as a `UiCommand`;
//! - Esc and Ctrl-C cancel instead of typing;
//! - a control chord never inserts a character.

use std::sync::{Arc, Mutex};

use crossterm::event::{KeyCode, KeyEvent as CrosstermKey, KeyModifiers};
use kimi_tui_app::{AppScreen, RecordingTransport, UiCommand, UiState};
use kimi_ui_core::{EntryContent, EntryId, EntryKind, EntryOp};
use ratatui::buffer::Buffer;
use textual_rs::AppEvent;
use textual_rs::testing::TestApp;

const WIDTH: u16 = 40;
const HEIGHT: u16 = 6;

fn upsert(kind: EntryKind, text: &str) -> EntryOp {
    let id = EntryId::engine("t1", 0, kind);
    EntryOp::Upsert {
        id,
        content: EntryContent::new(kind, text),
    }
}

/// A state plus a handle on what the UI asked the session to do.
fn state() -> (Arc<UiState>, Arc<Mutex<Vec<UiCommand>>>) {
    let transport = RecordingTransport::new();
    let log = transport.log();
    (UiState::new(transport), log)
}

fn mount(state: &Arc<UiState>) -> TestApp {
    let screen_state = Arc::clone(state);
    TestApp::new_styled(WIDTH, HEIGHT, AppScreen::CSS, move || {
        Box::new(AppScreen::new(Arc::clone(&screen_state)))
    })
}

/// The buffer's rows as text, trailing blanks trimmed (they are cells, not
/// content).
fn rows(buffer: &Buffer) -> Vec<String> {
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}

#[test]
fn a_recorded_entry_stream_paints_the_transcript() {
    let (state, _log) = state();
    state.apply(upsert(EntryKind::UserMsg, "explain the build"));
    state.apply(upsert(EntryKind::AssistantMsg, "the build is two steps."));

    let mut app = mount(&state);
    app.process_event(AppEvent::RenderRequest);

    let painted = rows(app.buffer());
    assert_eq!(
        painted[0], ">| explain the build",
        "the user entry carries its kind's gutter: {painted:?}"
    );
    assert_eq!(
        painted[1], " | the build is two steps.",
        "the assistant entry is one row above the prompt: {painted:?}"
    );
    assert_eq!(
        painted[HEIGHT as usize - 1],
        ">",
        "the prompt owns the last row: {painted:?}"
    );
}

#[test]
fn a_long_entry_wraps_inside_the_transcript_rows() {
    let (state, _log) = state();
    state.apply(upsert(
        EntryKind::AssistantMsg,
        "alpha beta gamma delta epsilon zeta eta theta iota",
    ));

    let mut app = mount(&state);
    app.process_event(AppEvent::RenderRequest);

    let painted = rows(app.buffer());
    assert!(
        painted[0].starts_with(" | alpha") && painted[1].starts_with("   "),
        "the text wraps and the continuation aligns under the body: {painted:?}"
    );
    for row in &painted[..HEIGHT as usize - 1] {
        assert!(
            row.chars().count() <= WIDTH as usize,
            "a transcript row never exceeds the terminal: {row:?}"
        );
    }
}

#[test]
fn typing_reaches_the_prompt_and_enter_submits_it() {
    let (state, log) = state();
    let mut app = mount(&state);

    for character in "ship it".chars() {
        app.process_event(AppEvent::Key(CrosstermKey::new(
            KeyCode::Char(character),
            KeyModifiers::NONE,
        )));
    }
    app.process_event(AppEvent::RenderRequest);
    assert_eq!(
        rows(app.buffer())[HEIGHT as usize - 1],
        "> ship it",
        "the line being typed is painted"
    );

    app.process_event(AppEvent::Key(CrosstermKey::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )));
    assert_eq!(
        log.lock().unwrap().as_slice(),
        &[UiCommand::Prompt("ship it".to_string())],
        "Enter hands the prompt to the session"
    );
    assert_eq!(state.prompt(), "", "the prompt line is cleared on submit");
}

#[test]
fn escape_and_ctrl_c_cancel_instead_of_typing() {
    for (code, modifiers) in [
        (KeyCode::Esc, KeyModifiers::NONE),
        (KeyCode::Char('c'), KeyModifiers::CONTROL),
    ] {
        let (state, log) = state();
        let mut app = mount(&state);
        app.process_event(AppEvent::Key(CrosstermKey::new(code, modifiers)));
        assert_eq!(
            log.lock().unwrap().as_slice(),
            &[UiCommand::Cancel],
            "{code:?} must cancel the turn"
        );
        assert_eq!(state.prompt(), "", "a cancel chord never inserts text");
    }
}

#[test]
fn a_control_chord_never_inserts_a_character() {
    let (state, _log) = state();
    let mut app = mount(&state);
    app.process_event(AppEvent::Key(CrosstermKey::new(
        KeyCode::Char('b'),
        KeyModifiers::ALT,
    )));
    assert_eq!(state.prompt(), "", "Alt-b is a chord, not text");
}

/// The same content at two widths: wrapping moves words onto other rows, it
/// never loses them, and no row exceeds the terminal.
#[test]
fn narrowing_the_terminal_wraps_the_same_content() {
    const TEXT: &str = "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu";

    let painted = |width: u16| {
        let (state, _log) = state();
        state.apply(upsert(EntryKind::AssistantMsg, TEXT));
        let screen_state = Arc::clone(&state);
        let app = TestApp::new_styled(width, 20, AppScreen::CSS, move || {
            Box::new(AppScreen::new(Arc::clone(&screen_state)))
        });
        rows(app.buffer())
    };

    let wide = painted(80);
    let narrow = painted(20);
    assert!(
        narrow.iter().filter(|row| !row.is_empty()).count()
            > wide.iter().filter(|row| !row.is_empty()).count(),
        "the narrow layout needs more rows: {narrow:?} vs {wide:?}"
    );
    for row in &narrow {
        assert!(
            row.chars().count() <= 20,
            "a row never exceeds the terminal: {row:?}"
        );
    }
    let squeezed = |rows: &[String]| rows.join("").replace(' ', "");
    for needle in ["alphabetagammadelta", "iotakappalambdamu"] {
        assert!(squeezed(&wide).contains(needle), "wide lost {needle}");
        assert!(squeezed(&narrow).contains(needle), "narrow lost {needle}");
    }
}
