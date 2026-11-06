//! The neutral input vocabulary: what a terminal event is once it has been
//! decoded, before any component decides what it means.
//!
//! The app decodes a terminal library's event into these types
//! (`kimi-tui-app::input`), the prompt reads them, and no crate in between has
//! to know crossterm exists. Swapping the terminal layer therefore touches one
//! file rather than the widget tree.

use serde::{Deserialize, Serialize};

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
    /// Whether this key moves the cursor rather than editing text. A consumer
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

    /// v2 / readline `Ctrl+C`. Kept as one predicate so every consumer agrees
    /// on which chord cancels: a consumer that spelled it inline once already
    /// differed from the other.
    pub fn is_cancel(self) -> bool {
        self.modifiers.ctrl && matches!(self.key, Key::Char('c' | 'C'))
    }
}

/// Mouse gesture kinds the UI models.
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
    /// Terminal resized; the layout re-measures on the next frame.
    Resize {
        columns: u16,
        rows: u16,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctrl_c_is_the_only_cancel_chord() {
        assert!(
            KeyEvent {
                key: Key::Char('c'),
                modifiers: Modifiers::ctrl()
            }
            .is_cancel()
        );
        assert!(
            !KeyEvent {
                key: Key::Char('c'),
                modifiers: Modifiers::default()
            }
            .is_cancel(),
            "a plain c is text"
        );
        assert!(
            !KeyEvent {
                key: Key::Char('d'),
                modifiers: Modifiers::ctrl()
            }
            .is_cancel(),
            "other chords are not the cancel chord"
        );
    }

    #[test]
    fn navigation_keys_are_told_apart_from_editing_keys() {
        assert!(Key::Left.is_navigation() && Key::PageDown.is_navigation());
        assert!(!Key::Char('a').is_navigation() && !Key::Enter.is_navigation());
    }
}
