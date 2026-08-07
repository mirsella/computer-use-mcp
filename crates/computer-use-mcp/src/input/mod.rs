pub mod backend;
pub mod coordinates;
pub mod eis;
pub mod keyboard;
pub mod keyboard_input;
pub mod pointer;

use crate::validation::{KeyboardAction, KeyboardFocus, PointerAction};

#[derive(Debug, Clone, PartialEq)]
pub enum GeneratedInputAction {
    Pointer(PointerAction),
    Keyboard {
        focus: KeyboardFocus,
        action: KeyboardAction,
    },
}
