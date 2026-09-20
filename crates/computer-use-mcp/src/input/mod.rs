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
