//! Decode a terminal event into the neutral [`InputEvent`] the UI contract uses.
//!
//! This is the only place in the workspace that names a terminal library's
//! event type. Everything above it — the app, the backend, the components —
//! speaks [`InputEvent`], so swapping the terminal layer touches one file.
//!
//! What is dropped on purpose: key *release* events (a terminal reports them
//! only when asked, and the contract models presses), and the keys the
//! contract does not model yet (function keys, media keys). A dropped event is
//! not an error: the UI simply has no meaning for it.

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind};
use kimi_ui_core::{InputEvent, Key, KeyEvent, Modifiers, MouseEvent, MouseKind};

/// Decode one terminal event, or `None` when the contract has no meaning for it.
pub fn to_input_event(event: Event) -> Option<InputEvent> {
    match event {
        Event::Key(key) => to_key_event(key).map(InputEvent::Key),
        Event::Paste(text) => Some(InputEvent::Paste(text)),
        Event::Resize(columns, rows) => Some(InputEvent::Resize { columns, rows }),
        Event::Mouse(mouse) => to_mouse_event(mouse).map(InputEvent::Mouse),
        // Focus reporting is not enabled, and no other event carries input.
        _ => None,
    }
}

fn to_key_event(event: crossterm::event::KeyEvent) -> Option<KeyEvent> {
    // A release is the other half of a press the UI already acted on; a repeat
    // is a press, and the platform decides whether it is reported at all.
    if event.kind == KeyEventKind::Release {
        return None;
    }
    let key = match event.code {
        KeyCode::Char(character) => Key::Char(character),
        KeyCode::Esc => Key::Esc,
        KeyCode::Enter => Key::Enter,
        KeyCode::Tab => Key::Tab,
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Delete => Key::Delete,
        KeyCode::Left => Key::Left,
        KeyCode::Right => Key::Right,
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::Home => Key::Home,
        KeyCode::End => Key::End,
        KeyCode::PageUp => Key::PageUp,
        KeyCode::PageDown => Key::PageDown,
        _ => return None,
    };
    Some(KeyEvent {
        key,
        modifiers: to_modifiers(event.modifiers),
    })
}

fn to_modifiers(modifiers: KeyModifiers) -> Modifiers {
    Modifiers {
        ctrl: modifiers.contains(KeyModifiers::CONTROL),
        alt: modifiers.contains(KeyModifiers::ALT),
        shift: modifiers.contains(KeyModifiers::SHIFT),
    }
}

/// Wheel and the primary button, which is what a scrollable transcript uses.
/// Drags, motion and the other buttons belong to an editor that does not exist
/// yet, so they are reported as no event rather than as a wrong one.
fn to_mouse_event(event: crossterm::event::MouseEvent) -> Option<MouseEvent> {
    let kind = match event.kind {
        MouseEventKind::ScrollUp => MouseKind::ScrollUp,
        MouseEventKind::ScrollDown => MouseKind::ScrollDown,
        MouseEventKind::Down(MouseButton::Left) => MouseKind::Press,
        MouseEventKind::Up(MouseButton::Left) => MouseKind::Release,
        _ => return None,
    };
    Some(MouseEvent {
        kind,
        column: event.column,
        row: event.row,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent as CrosstermKeyEvent, MouseEvent as CrosstermMouseEvent};

    fn key(code: KeyCode, modifiers: KeyModifiers) -> InputEvent {
        to_input_event(Event::Key(CrosstermKeyEvent::new(code, modifiers)))
            .expect("a printable or named key decodes")
    }

    #[test]
    fn a_character_key_keeps_its_character_and_modifiers() {
        let event = key(KeyCode::Char('k'), KeyModifiers::CONTROL);
        let InputEvent::Key(key) = event else {
            panic!("expected a key event");
        };
        assert_eq!(key.key, Key::Char('k'));
        assert!(key.modifiers.ctrl);
        assert!(!key.modifiers.alt);
    }

    #[test]
    fn ctrl_c_is_recognised_as_the_cancel_chord() {
        let InputEvent::Key(key) = key(KeyCode::Char('c'), KeyModifiers::CONTROL) else {
            panic!("expected a key event");
        };
        assert!(key.is_cancel());
    }

    #[test]
    fn the_named_keys_the_contract_models_decode() {
        for (code, expected) in [
            (KeyCode::Esc, Key::Esc),
            (KeyCode::Enter, Key::Enter),
            (KeyCode::Tab, Key::Tab),
            (KeyCode::Backspace, Key::Backspace),
            (KeyCode::Delete, Key::Delete),
            (KeyCode::Left, Key::Left),
            (KeyCode::Home, Key::Home),
            (KeyCode::PageDown, Key::PageDown),
        ] {
            let InputEvent::Key(key) = key(code, KeyModifiers::NONE) else {
                panic!("expected a key event");
            };
            assert_eq!(key.key, expected);
        }
    }

    #[test]
    fn a_key_the_contract_does_not_model_decodes_to_nothing() {
        assert!(
            to_input_event(Event::Key(CrosstermKeyEvent::new(
                KeyCode::F(5),
                KeyModifiers::NONE
            )))
            .is_none()
        );
    }

    #[test]
    fn a_key_release_is_not_an_input_event() {
        let mut event = CrosstermKeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        event.kind = KeyEventKind::Release;
        assert!(to_input_event(Event::Key(event)).is_none());
    }

    #[test]
    fn paste_and_resize_decode_directly() {
        assert_eq!(
            to_input_event(Event::Paste("multi\nline".into())),
            Some(InputEvent::Paste("multi\nline".into()))
        );
        assert_eq!(
            to_input_event(Event::Resize(40, 12)),
            Some(InputEvent::Resize {
                columns: 40,
                rows: 12
            })
        );
    }

    #[test]
    fn the_wheel_and_the_primary_button_become_mouse_events() {
        let mouse = |kind| {
            to_input_event(Event::Mouse(CrosstermMouseEvent {
                kind,
                column: 3,
                row: 7,
                modifiers: KeyModifiers::NONE,
            }))
        };
        assert_eq!(
            mouse(MouseEventKind::ScrollUp),
            Some(InputEvent::Mouse(MouseEvent {
                kind: MouseKind::ScrollUp,
                column: 3,
                row: 7
            }))
        );
        assert_eq!(
            mouse(MouseEventKind::Down(MouseButton::Left)),
            Some(InputEvent::Mouse(MouseEvent {
                kind: MouseKind::Press,
                column: 3,
                row: 7
            }))
        );
        assert!(
            mouse(MouseEventKind::Down(MouseButton::Right)).is_none(),
            "only the primary button is modelled"
        );
    }
}
