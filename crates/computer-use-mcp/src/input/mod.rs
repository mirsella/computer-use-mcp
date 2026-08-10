pub mod backend;
pub mod coordinates;
pub mod eis;
pub mod keyboard;
pub mod keyboard_input;
pub mod pointer;

use crate::validation::{KeyboardEvent, KeyboardPoint, PointerAction};

#[derive(Debug, Clone, PartialEq)]
pub enum GeneratedInputAction {
    Pointer(PointerAction),
    KeyboardTransaction {
        focus: KeyboardPoint,
        events: Vec<KeyboardEvent>,
    },
}
