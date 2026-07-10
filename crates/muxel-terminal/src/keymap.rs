//! Translate UI key events into the byte sequences a PTY expects.
//!
//! Adapted from okena's `okena-terminal::input` (MIT). Text-producing keys
//! return `None` so the gpui `InputHandler` (IME/text) path handles them,
//! avoiding double input.

/// Keyboard modifiers, framework-agnostic.
#[derive(Clone, Debug, Default)]
pub struct KeyModifiers {
    pub control: bool,
    pub shift: bool,
    pub alt: bool,
    /// Platform key (Cmd on macOS, Super on Linux/Windows).
    pub platform: bool,
}

/// Convert a key press into terminal input bytes.
///
/// `app_cursor_mode` (DECCKM): when true, unmodified arrow keys send SS3
/// (`ESC O A`) instead of CSI (`ESC [ A`) — used by vim, less, htop, etc.
pub fn key_to_bytes(
    key: &str,
    key_char: Option<&str>,
    mods: &KeyModifiers,
    app_cursor_mode: bool,
) -> Option<Vec<u8>> {
    // Ctrl+<letter> -> control character (Ctrl+V = 0x16 — Grok image paste).
    if mods.control
        && !mods.shift
        && !mods.alt
        && !mods.platform
        && let Some(c) = key.chars().next()
        && key.len() == 1
        && c.is_ascii_alphabetic()
    {
        let ctrl = (c.to_ascii_lowercase() as u8) - b'a' + 1;
        return Some(vec![ctrl]);
    }

    // Alt+letter → ESC + letter. Claude Code on Windows uses Alt+V for image
    // paste; without this, Alt is dropped and the agent never sees the chord.
    if mods.alt
        && !mods.control
        && !mods.platform
        && key.len() == 1
        && let Some(c) = key.chars().next()
        && c.is_ascii_alphabetic()
    {
        return Some(vec![0x1b, c.to_ascii_lowercase() as u8]);
    }

    if key == "tab" {
        if mods.shift {
            return Some(b"\x1b[Z".to_vec());
        }
        return Some(b"\t".to_vec());
    }

    match key {
        "enter" | "return" | "kp_enter" => {
            // Shift+Enter -> newline (multi-line input in TUIs like Claude Code).
            if mods.shift {
                return Some(b"\n".to_vec());
            }
            return Some(b"\r".to_vec());
        }
        _ => {}
    }

    // Modifier code for CSI sequences (1=none, +1 shift, +2 alt, +4 ctrl).
    let modifier_code = 1
        + (if mods.shift { 1 } else { 0 })
        + (if mods.alt { 2 } else { 0 })
        + (if mods.control { 4 } else { 0 });

    match key {
        "up" | "down" | "right" | "left" => {
            let c = match key {
                "up" => 'A',
                "down" => 'B',
                "right" => 'C',
                "left" => 'D',
                _ => unreachable!(),
            };
            if modifier_code > 1 {
                return Some(format!("\x1b[1;{modifier_code}{c}").into_bytes());
            }
            if app_cursor_mode {
                return Some(format!("\x1bO{c}").into_bytes());
            }
            return Some(format!("\x1b[{c}").into_bytes());
        }
        _ => {}
    }

    // Let the InputHandler emit anything that produces a character.
    if key_char.is_some() {
        return None;
    }

    match key {
        "backspace" => return Some(b"\x7f".to_vec()),
        "escape" => return Some(b"\x1b".to_vec()),
        "home" => {
            if modifier_code > 1 {
                return Some(format!("\x1b[1;{modifier_code}H").into_bytes());
            }
            return Some(b"\x1b[H".to_vec());
        }
        "end" => {
            if modifier_code > 1 {
                return Some(format!("\x1b[1;{modifier_code}F").into_bytes());
            }
            return Some(b"\x1b[F".to_vec());
        }
        "pageup" => {
            if modifier_code > 1 {
                return Some(format!("\x1b[5;{modifier_code}~").into_bytes());
            }
            return Some(b"\x1b[5~".to_vec());
        }
        "pagedown" => {
            if modifier_code > 1 {
                return Some(format!("\x1b[6;{modifier_code}~").into_bytes());
            }
            return Some(b"\x1b[6~".to_vec());
        }
        "delete" => {
            if modifier_code > 1 {
                return Some(format!("\x1b[3;{modifier_code}~").into_bytes());
            }
            return Some(b"\x1b[3~".to_vec());
        }
        // Plain Insert; Shift+Insert / Ctrl+Insert handled in the view (paste/copy).
        "insert" => {
            if modifier_code > 1 {
                return Some(format!("\x1b[2;{modifier_code}~").into_bytes());
            }
            return Some(b"\x1b[2~".to_vec());
        }
        "f1" => return Some(b"\x1bOP".to_vec()),
        "f2" => return Some(b"\x1bOQ".to_vec()),
        "f3" => return Some(b"\x1bOR".to_vec()),
        "f4" => return Some(b"\x1bOS".to_vec()),
        "f5" => return Some(b"\x1b[15~".to_vec()),
        "f6" => return Some(b"\x1b[17~".to_vec()),
        "f7" => return Some(b"\x1b[18~".to_vec()),
        "f8" => return Some(b"\x1b[19~".to_vec()),
        "f9" => return Some(b"\x1b[20~".to_vec()),
        "f10" => return Some(b"\x1b[21~".to_vec()),
        "f11" => return Some(b"\x1b[23~".to_vec()),
        "f12" => return Some(b"\x1b[24~".to_vec()),
        _ => {}
    }

    if key.len() == 1 {
        return Some(key.as_bytes().to_vec());
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mods(shift: bool, alt: bool, control: bool) -> KeyModifiers {
        KeyModifiers {
            control,
            shift,
            alt,
            platform: false,
        }
    }

    #[test]
    fn alt_v_is_esc_v() {
        assert_eq!(
            key_to_bytes("v", None, &mods(false, true, false), false),
            Some(b"\x1bv".to_vec())
        );
    }

    #[test]
    fn ctrl_v_is_c0() {
        assert_eq!(
            key_to_bytes("v", None, &mods(false, false, true), false),
            Some(vec![0x16])
        );
    }

    #[test]
    fn shift_insert_csi() {
        assert_eq!(
            key_to_bytes("insert", None, &mods(true, false, false), false),
            Some(b"\x1b[2;2~".to_vec())
        );
    }
}
