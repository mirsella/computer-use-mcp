use xkbcommon::xkb;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyChord {
    pub modifiers: Vec<u32>,
    pub key: u32,
}

pub fn parse_chord(value: &str) -> Result<KeyChord, String> {
    let parts = value.split('+').map(str::trim).collect::<Vec<_>>();
    if parts.iter().any(|part| part.is_empty()) {
        return Err("key chord contains an empty key name".into());
    }
    let mut tokens = parts
        .iter()
        .map(|part| parse_token(part))
        .collect::<Result<Vec<_>, _>>()?;
    let (key, key_is_modifier) = tokens
        .pop()
        .expect("split always yields at least one nonempty token");
    if key_is_modifier {
        return Err("key chord must end with a non-modifier key".into());
    }
    if let Some((index, _)) = tokens
        .iter()
        .enumerate()
        .find(|(_, (_, modifier))| !modifier)
    {
        return Err(format!(
            "non-modifier key {:?} appears before the end of the chord",
            parts[index]
        ));
    }
    Ok(KeyChord {
        modifiers: tokens.into_iter().map(|(keysym, _)| keysym).collect(),
        key,
    })
}

pub fn unicode_keysym(character: char) -> Result<u32, String> {
    if character == '\0' {
        return Err("NUL text cannot be represented as keyboard input".into());
    }
    let keysym = xkb::utf32_to_keysym(character.into()).raw();
    if keysym == 0 {
        return Err(format!(
            "Unicode character U+{:04X} has no XKB keysym",
            u32::from(character)
        ));
    }
    Ok(keysym)
}

fn parse_token(value: &str) -> Result<(u32, bool), String> {
    let normalized = value.to_ascii_lowercase();
    let (xkb_name, modifier) = match normalized.as_str() {
        "ctrl" | "control" => ("Control_L", true),
        "alt" => ("Alt_L", true),
        "shift" => ("Shift_L", true),
        "super" | "meta" | "win" | "windows" => ("Super_L", true),
        "return" | "enter" => ("Return", false),
        "tab" => ("Tab", false),
        "escape" | "esc" => ("Escape", false),
        "space" => ("space", false),
        "backspace" => ("BackSpace", false),
        "delete" | "del" => ("Delete", false),
        "insert" | "ins" => ("Insert", false),
        "home" => ("Home", false),
        "end" => ("End", false),
        "pageup" | "page_up" | "prior" => ("Page_Up", false),
        "pagedown" | "page_down" | "next" => ("Page_Down", false),
        "left" | "arrowleft" => ("Left", false),
        "right" | "arrowright" => ("Right", false),
        "up" | "arrowup" => ("Up", false),
        "down" | "arrowdown" => ("Down", false),
        "kp_0" | "kp0" => ("KP_0", false),
        "kp_1" | "kp1" => ("KP_1", false),
        "kp_2" | "kp2" => ("KP_2", false),
        "kp_3" | "kp3" => ("KP_3", false),
        "kp_4" | "kp4" => ("KP_4", false),
        "kp_5" | "kp5" => ("KP_5", false),
        "kp_6" | "kp6" => ("KP_6", false),
        "kp_7" | "kp7" => ("KP_7", false),
        "kp_8" | "kp8" => ("KP_8", false),
        "kp_9" | "kp9" => ("KP_9", false),
        "kp_enter" => ("KP_Enter", false),
        "kp_add" | "kp_plus" => ("KP_Add", false),
        "kp_subtract" | "kp_minus" => ("KP_Subtract", false),
        "kp_multiply" => ("KP_Multiply", false),
        "kp_divide" => ("KP_Divide", false),
        "kp_decimal" => ("KP_Decimal", false),
        name if function_key(name).is_some() => (function_key(name).expect("checked above"), false),
        _ => {
            let mut characters = value.chars();
            let Some(character) = characters.next() else {
                return Err("key name is empty".into());
            };
            if characters.next().is_none() {
                // Chord key names are case-insensitive. Literal text preserves case through
                // `unicode_keysym` directly and must request Shift when the keymap requires it.
                return unicode_keysym(character.to_ascii_lowercase())
                    .map(|keysym| (keysym, false));
            }
            return Err(format!("unknown key name {value:?}"));
        }
    };
    let keysym = xkb::keysym_from_name(xkb_name, xkb::KEYSYM_CASE_INSENSITIVE).raw();
    if keysym == 0 {
        return Err(format!("XKB does not recognize key name {xkb_name:?}"));
    }
    Ok((keysym, modifier))
}

fn function_key(name: &str) -> Option<&'static str> {
    Some(match name {
        "f1" => "F1",
        "f2" => "F2",
        "f3" => "F3",
        "f4" => "F4",
        "f5" => "F5",
        "f6" => "F6",
        "f7" => "F7",
        "f8" => "F8",
        "f9" => "F9",
        "f10" => "F10",
        "f11" => "F11",
        "f12" => "F12",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_aliases_keypad_and_modifier_order() {
        let chord = parse_chord("alt+Control+KP_0").unwrap();
        assert_eq!(chord.modifiers[0], xkb::Keysym::Alt_L.raw());
        assert_eq!(chord.modifiers[1], xkb::Keysym::Control_L.raw());
        assert_eq!(chord.key, xkb::Keysym::KP_0.raw());
        assert!(parse_chord("ctrl+wat").unwrap_err().contains("unknown"));
        assert_eq!(parse_chord("Ctrl+L").unwrap().key, xkb::Keysym::l.raw());
        for direction in ["Left", "Right", "Up", "Down"] {
            let alias = format!("Arrow{direction}");
            assert_eq!(
                parse_chord(&alias).unwrap(),
                parse_chord(direction).unwrap()
            );
        }
        assert!(parse_chord("a+ctrl").is_err());
        for name in [
            "Return",
            "Tab",
            "Escape",
            "Space",
            "F1",
            "F12",
            "Left",
            "Home",
            "End",
            "PageUp",
            "PageDown",
            "Insert",
            "Delete",
            "Backspace",
            "KP_Enter",
        ] {
            parse_chord(name).unwrap();
        }
    }

    #[test]
    fn unicode_tokens_are_xkb_keysyms() {
        assert_ne!(unicode_keysym('é').unwrap(), 0);
        assert_ne!(unicode_keysym('🙂').unwrap(), 0);
        assert!(unicode_keysym('\0').is_err());
    }
}
