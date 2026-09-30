//! Maps browser `KeyboardEvent.code` values to X11 keysyms.
//!
//! Keys are mapped by physical position (US layout), the way a real keyboard
//! plugged into the workstation would behave. Shift, Ctrl, Alt are forwarded as
//! their own key presses, so X applies them exactly as for local typing.

pub const SHIFT_L: u32 = 0xffe1;

/// Keysym for a physical key code, or None if unknown.
/// `cmd_as_ctrl` sends the Mac Command key as Control.
pub fn code_to_keysym(code: &str, cmd_as_ctrl: bool) -> Option<u32> {
    let ks = match code {
        "ShiftLeft" => 0xffe1,
        "ShiftRight" => 0xffe2,
        "ControlLeft" => 0xffe3,
        "ControlRight" => 0xffe4,
        "CapsLock" => 0xffe5,
        "AltLeft" => 0xffe9,
        "AltRight" => 0xfe03, // ISO_Level3_Shift (AltGr)
        "MetaLeft" | "OSLeft" => if cmd_as_ctrl { 0xffe3 } else { 0xffeb },
        "MetaRight" | "OSRight" => if cmd_as_ctrl { 0xffe4 } else { 0xffec },
        "ContextMenu" => 0xff67,
        "Backspace" => 0xff08,
        "Tab" => 0xff09,
        "Enter" => 0xff0d,
        "Escape" => 0xff1b,
        "Delete" => 0xffff,
        "Home" => 0xff50,
        "ArrowLeft" => 0xff51,
        "ArrowUp" => 0xff52,
        "ArrowRight" => 0xff53,
        "ArrowDown" => 0xff54,
        "PageUp" => 0xff55,
        "PageDown" => 0xff56,
        "End" => 0xff57,
        "Insert" | "Help" => 0xff63,
        "PrintScreen" => 0xff61,
        "ScrollLock" => 0xff14,
        "Pause" => 0xff13,
        "NumLock" => 0xff7f,
        "NumpadEnter" => 0xff8d,
        "NumpadDecimal" => 0xffae,
        "NumpadAdd" => 0xffab,
        "NumpadSubtract" => 0xffad,
        "NumpadMultiply" => 0xffaa,
        "NumpadDivide" => 0xffaf,
        "NumpadEqual" => 0xffbd,
        "Space" => 0x20,
        "Minus" => '-' as u32,
        "Equal" => '=' as u32,
        "BracketLeft" => '[' as u32,
        "BracketRight" => ']' as u32,
        "Backslash" => '\\' as u32,
        "Semicolon" => ';' as u32,
        "Quote" => '\'' as u32,
        "Backquote" => '`' as u32,
        "Comma" => ',' as u32,
        "Period" => '.' as u32,
        "Slash" => '/' as u32,
        "IntlBackslash" => '<' as u32,
        _ => {
            if let Some(l) = code.strip_prefix("Key") {
                let c = l.chars().next()?;
                if l.len() == 1 && c.is_ascii_uppercase() {
                    return Some(c.to_ascii_lowercase() as u32);
                }
                return None;
            }
            if let Some(d) = code.strip_prefix("Digit") {
                let c = d.chars().next()?;
                if d.len() == 1 && c.is_ascii_digit() {
                    return Some(c as u32);
                }
                return None;
            }
            if let Some(d) = code.strip_prefix("Numpad") {
                let c = d.chars().next()?;
                if d.len() == 1 && c.is_ascii_digit() {
                    return Some(0xffb0 + (c as u32 - '0' as u32));
                }
                return None;
            }
            if let Some(n) = code.strip_prefix('F') {
                if let Ok(n) = n.parse::<u32>() {
                    if (1..=24).contains(&n) {
                        return Some(0xffbe + n - 1);
                    }
                }
            }
            return None;
        }
    };
    Some(ks)
}

/// Keysym for a Unicode character.
pub fn char_to_keysym(c: char) -> u32 {
    let cp = c as u32;
    match cp {
        0x0a | 0x0d => 0xff0d,
        0x09 => 0xff09,
        0x08 => 0xff08,
        0x20..=0x7e | 0xa0..=0xff => cp,
        _ => 0x0100_0000 + cp,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mapping() {
        assert_eq!(code_to_keysym("KeyA", true), Some('a' as u32));
        assert_eq!(code_to_keysym("Digit7", true), Some('7' as u32));
        assert_eq!(code_to_keysym("MetaLeft", true), Some(0xffe3));
        assert_eq!(code_to_keysym("MetaLeft", false), Some(0xffeb));
        assert_eq!(code_to_keysym("F12", true), Some(0xffc9));
        assert_eq!(code_to_keysym("Numpad5", true), Some(0xffb5));
        assert_eq!(code_to_keysym("Nonsense", true), None);
        assert_eq!(char_to_keysym('é'), 0xe9);
        assert_eq!(char_to_keysym('€'), 0x0100_20ac);
    }
}
