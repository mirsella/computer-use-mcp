pub mod backend;
pub mod coordinates;
pub mod eis;
pub mod keyboard;
pub mod keyboard_input;
pub mod pointer;

use crate::validation::{KeyboardEvent, KeyboardFocus, PointerAction};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    Pointer,
    FocusedKeyboard,
}

#[derive(Debug, Clone, PartialEq)]
pub enum GeneratedInputAction {
    Pointer(PointerAction),
    KeyboardTransaction {
        focus: KeyboardFocus,
        events: Vec<KeyboardEvent>,
    },
    Paste {
        focus: KeyboardFocus,
        text: String,
    },
}

impl GeneratedInputAction {
    pub(crate) fn semantic_focus(&self) -> Option<&str> {
        match self {
            Self::KeyboardTransaction {
                focus: KeyboardFocus::Semantic { element_id },
                ..
            }
            | Self::Paste {
                focus: KeyboardFocus::Semantic { element_id },
                ..
            } => Some(element_id),
            _ => None,
        }
    }

    /// Keyboard input that needs no pointer, focus click, or screenshot
    /// mapping: a verified semantic element or the active window's own focus.
    pub(crate) fn uses_focused_path(&self) -> bool {
        matches!(
            self,
            Self::KeyboardTransaction {
                focus: KeyboardFocus::Semantic { .. } | KeyboardFocus::Window,
                ..
            } | Self::Paste {
                focus: KeyboardFocus::Semantic { .. } | KeyboardFocus::Window,
                ..
            }
        )
    }
}
