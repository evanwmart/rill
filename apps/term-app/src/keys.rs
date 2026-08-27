//! Keystrokes in, bytes out.
//!
//! The viewport hands over a key name, the text it produced (if any), and the
//! modifiers. A terminal wants none of that — it wants the bytes a keyboard
//! would have put on the wire — so this is where the translation lives, and
//! it is the only place in the app that knows what a key is called.

/// The bytes a key press sends to the shell. Empty means "nothing to send",
/// which is the right answer for a modifier held on its own.
/// `app_cursor` is DECCKM: an application that set it (vim, less, htop)
/// expects its arrows as `ESC O A` and will read the normal `ESC [ A` as
/// Escape followed by letters — which is arrows typing text into a TUI.
/// Modified arrows keep the CSI form either way, as xterm does.
pub fn to_bytes(key: &str, text: Option<&str>, ctrl: bool, alt: bool, app_cursor: bool) -> Vec<u8> {
    let ss3 = app_cursor && !ctrl && !alt;
    let mut out = match key {
        "enter" => vec![b'\r'],
        "backspace" => vec![0x7f],
        "tab" => vec![b'\t'],
        "escape" => vec![0x1b],
        "up" if ss3 => b"\x1bOA".to_vec(),
        "down" if ss3 => b"\x1bOB".to_vec(),
        "right" if ss3 => b"\x1bOC".to_vec(),
        "left" if ss3 => b"\x1bOD".to_vec(),
        "home" if ss3 => b"\x1bOH".to_vec(),
        "end" if ss3 => b"\x1bOF".to_vec(),
        "up" => b"\x1b[A".to_vec(),
        "down" => b"\x1b[B".to_vec(),
        "right" => b"\x1b[C".to_vec(),
        "left" => b"\x1b[D".to_vec(),
        "home" => b"\x1b[H".to_vec(),
        "end" => b"\x1b[F".to_vec(),
        "insert" => b"\x1b[2~".to_vec(),
        "delete" => b"\x1b[3~".to_vec(),
        "pageup" => b"\x1b[5~".to_vec(),
        "pagedown" => b"\x1b[6~".to_vec(),
        "f1" => b"\x1bOP".to_vec(),
        "f2" => b"\x1bOQ".to_vec(),
        "f3" => b"\x1bOR".to_vec(),
        "f4" => b"\x1bOS".to_vec(),
        "f5" => b"\x1b[15~".to_vec(),
        "f6" => b"\x1b[17~".to_vec(),
        "f7" => b"\x1b[18~".to_vec(),
        "f8" => b"\x1b[19~".to_vec(),
        "f9" => b"\x1b[20~".to_vec(),
        "f10" => b"\x1b[21~".to_vec(),
        "f11" => b"\x1b[23~".to_vec(),
        "f12" => b"\x1b[24~".to_vec(),
        // Ctrl+letter is a control code, and the host names the letter rather
        // than sending the code as text — which is what makes Ctrl+C reach
        // the shell as an interrupt instead of as the letter c.
        k if ctrl && k.len() == 1 => {
            let c = k.as_bytes()[0].to_ascii_lowercase();
            match c {
                b'a'..=b'z' => vec![c - b'a' + 1],
                b'[' => vec![0x1b],
                b'\\' => vec![0x1c],
                b']' => vec![0x1d],
                b'?' => vec![0x7f],
                b' ' | b'@' => vec![0],
                _ => text.map(|t| t.as_bytes().to_vec()).unwrap_or_default(),
            }
        }
        _ => text.map(|t| t.as_bytes().to_vec()).unwrap_or_default(),
    };
    // Meta is an escape prefix — the convention every shell already speaks.
    if alt && !out.is_empty() {
        out.insert(0, 0x1b);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctrl_c_is_an_interrupt_not_a_letter() {
        assert_eq!(to_bytes("c", None, true, false, false), vec![3]);
        assert_eq!(to_bytes("c", Some("c"), false, false, false), b"c".to_vec());
    }

    #[test]
    fn the_arrows_and_the_function_row_speak_ansi() {
        assert_eq!(to_bytes("up", None, false, false, false), b"\x1b[A".to_vec());
        assert_eq!(to_bytes("f5", None, false, false, false), b"\x1b[15~".to_vec());
        assert_eq!(to_bytes("pagedown", None, false, false, false), b"\x1b[6~".to_vec());
    }

    #[test]
    fn alt_prefixes_an_escape() {
        assert_eq!(to_bytes("b", Some("b"), false, true, false), vec![0x1b, b'b']);
        // Nothing to send stays nothing: a bare modifier must not emit ESC.
        assert!(to_bytes("shift", None, false, true, false).is_empty());
    }

    #[test]
    fn enter_sends_carriage_return() {
        // Not \n: the line discipline turns \r into the newline, and a shell
        // sent \n directly will not execute the line.
        assert_eq!(to_bytes("enter", None, false, false, false), vec![b'\r']);
    }
}
