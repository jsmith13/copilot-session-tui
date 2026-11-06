use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

/// Encode a decoded key event back into the bytes a terminal would have sent.
///
/// Crossterm parses the terminal's byte stream into `KeyEvent`s; to forward a key to
/// a child process we have to reverse that translation. `application_cursor` selects
/// between the normal (`CSI A`) and application (`SS3 A`) cursor-key encodings, which
/// full-screen programs switch on via DECCKM.
pub fn encode(key: &KeyEvent, application_cursor: bool, block_ctrl_z: bool) -> Option<Vec<u8>> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    // Ctrl+Z suspends Copilot instead of undoing, leaving its pane unresponsive.
    if block_ctrl_z && ctrl && matches!(key.code, KeyCode::Char('z' | 'Z')) {
        return None;
    }
    let alt = key
        .modifiers
        .intersects(KeyModifiers::ALT | KeyModifiers::META | KeyModifiers::SUPER);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);

    let base: Vec<u8> = match key.code {
        KeyCode::Char(c) => {
            let mut bytes = if ctrl {
                vec![control_byte(c)?]
            } else {
                let mut buffer = [0u8; 4];
                c.encode_utf8(&mut buffer).as_bytes().to_vec()
            };
            if alt {
                let mut escaped = vec![0x1b];
                escaped.append(&mut bytes);
                bytes = escaped;
            }
            return Some(bytes);
        }
        // Conhost folds a bare LF into VK_RETURN with Ctrl set, so any terminal that
        // sends 0x0A for Ctrl+J — that is, anything without win32-input-mode, Alacritty
        // included — arrives here as Ctrl+Enter rather than Ctrl+J. Copilot reads LF as
        // "insert a newline" and CR as "submit", so collapsing this to CR turns Ctrl+J
        // into an accidental send.
        KeyCode::Enter if ctrl => vec![b'\n'],
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Tab => {
            if shift {
                b"\x1b[Z".to_vec()
            } else {
                vec![b'\t']
            }
        }
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        // DEL rather than BS: this is what modern terminals send and what readline expects.
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => cursor_key(b'A', key.modifiers, application_cursor),
        KeyCode::Down => cursor_key(b'B', key.modifiers, application_cursor),
        KeyCode::Right => cursor_key(b'C', key.modifiers, application_cursor),
        KeyCode::Left => cursor_key(b'D', key.modifiers, application_cursor),
        KeyCode::Home => cursor_key(b'H', key.modifiers, application_cursor),
        KeyCode::End => cursor_key(b'F', key.modifiers, application_cursor),
        KeyCode::Insert => tilde_key(2, key.modifiers),
        KeyCode::Delete => tilde_key(3, key.modifiers),
        KeyCode::PageUp => tilde_key(5, key.modifiers),
        KeyCode::PageDown => tilde_key(6, key.modifiers),
        KeyCode::F(n) => function_key(n, key.modifiers)?,
        KeyCode::Null => vec![0],
        _ => return None,
    };

    if alt && !base.starts_with(&[0x1b]) {
        let mut escaped = vec![0x1b];
        escaped.extend_from_slice(&base);
        return Some(escaped);
    }
    Some(base)
}

/// Map `Ctrl-<char>` to its C0 control byte (Ctrl-A = 0x01 … Ctrl-Z = 0x1a, plus the
/// symbolic controls). Returns `None` for combinations with no byte representation.
fn control_byte(c: char) -> Option<u8> {
    let lower = c.to_ascii_lowercase();
    match lower {
        'a'..='z' => Some(lower as u8 - b'a' + 1),
        ' ' | '@' => Some(0),
        '[' => Some(0x1b),
        '\\' => Some(0x1c),
        ']' => Some(0x1d),
        '^' => Some(0x1e),
        '_' | '/' => Some(0x1f),
        '?' => Some(0x7f),
        _ => None,
    }
}

/// Modifier parameter used by xterm: 1 + bitmask (shift 1, alt 2, ctrl 4).
fn modifier_param(modifiers: KeyModifiers) -> Option<u8> {
    let mut mask = 0;
    if modifiers.contains(KeyModifiers::SHIFT) {
        mask |= 1;
    }
    if modifiers.contains(KeyModifiers::ALT) {
        mask |= 2;
    }
    if modifiers.contains(KeyModifiers::CONTROL) {
        mask |= 4;
    }
    if mask == 0 {
        None
    } else {
        Some(mask + 1)
    }
}

fn cursor_key(final_byte: u8, modifiers: KeyModifiers, application_cursor: bool) -> Vec<u8> {
    match modifier_param(modifiers) {
        // Modified cursor keys always use the CSI form with a parameter.
        Some(param) => format!("\x1b[1;{}{}", param, final_byte as char).into_bytes(),
        None if application_cursor => vec![0x1b, b'O', final_byte],
        None => vec![0x1b, b'[', final_byte],
    }
}

fn tilde_key(number: u8, modifiers: KeyModifiers) -> Vec<u8> {
    match modifier_param(modifiers) {
        Some(param) => format!("\x1b[{};{}~", number, param).into_bytes(),
        None => format!("\x1b[{}~", number).into_bytes(),
    }
}

fn function_key(n: u8, modifiers: KeyModifiers) -> Option<Vec<u8>> {
    // F1-F4 use SS3 when unmodified; the rest use CSI <number> ~.
    let unmodified = modifier_param(modifiers).is_none();
    let bytes = match (n, unmodified) {
        (1, true) => b"\x1bOP".to_vec(),
        (2, true) => b"\x1bOQ".to_vec(),
        (3, true) => b"\x1bOR".to_vec(),
        (4, true) => b"\x1bOS".to_vec(),
        _ => {
            let number = function_key_number(n)?;
            match modifier_param(modifiers) {
                Some(param) => format!("\x1b[{};{}~", number, param).into_bytes(),
                None => format!("\x1b[{}~", number).into_bytes(),
            }
        }
    };
    Some(bytes)
}

fn function_key_number(n: u8) -> Option<u8> {
    match n {
        1 => Some(11),
        2 => Some(12),
        3 => Some(13),
        4 => Some(14),
        5 => Some(15),
        // 16 is deliberately skipped by the xterm scheme.
        6 => Some(17),
        7 => Some(18),
        8 => Some(19),
        9 => Some(20),
        10 => Some(21),
        // 22 is likewise skipped.
        11 => Some(23),
        12 => Some(24),
        _ => None,
    }
}

/// Wrap pasted text in bracketed-paste markers when the child has enabled the mode.
///
/// Without the markers the child cannot tell pasted text from typing, so newlines are
/// rewritten to `\r` — byte-for-byte what forwarding each Enter key used to produce.
/// A child that asked for bracketed paste gets the text verbatim, keeping `\n` line
/// breaks intact the way a Unix paste already delivers them.
pub fn encode_paste(text: &str, bracketed: bool) -> Vec<u8> {
    if text.is_empty() {
        return vec![0x16];
    }
    if !bracketed {
        return text.replace("\r\n", "\r").replace('\n', "\r").into_bytes();
    }
    let mut bytes = b"\x1b[200~".to_vec();
    bytes.extend_from_slice(text.as_bytes());
    bytes.extend_from_slice(b"\x1b[201~");
    bytes
}

pub fn encode_mouse(
    event: MouseEvent,
    coordinates: (u16, u16),
    mode: vt100::MouseProtocolMode,
    encoding: vt100::MouseProtocolEncoding,
) -> Option<Vec<u8>> {
    use vt100::MouseProtocolMode;

    let (button, release) = match event.kind {
        MouseEventKind::Down(button) => (mouse_button(button), false),
        MouseEventKind::Up(button) if mode != MouseProtocolMode::Press => {
            (mouse_button(button), true)
        }
        MouseEventKind::Drag(button)
            if matches!(
                mode,
                MouseProtocolMode::ButtonMotion | MouseProtocolMode::AnyMotion
            ) =>
        {
            (mouse_button(button) + 32, false)
        }
        MouseEventKind::Moved if mode == MouseProtocolMode::AnyMotion => (35, false),
        MouseEventKind::ScrollUp => (64, false),
        MouseEventKind::ScrollDown => (65, false),
        MouseEventKind::ScrollLeft => (66, false),
        MouseEventKind::ScrollRight => (67, false),
        _ => return None,
    };
    let modifiers = mouse_modifier_bits(event.modifiers);
    let button = button + modifiers;
    let (column, row) = coordinates;

    match encoding {
        vt100::MouseProtocolEncoding::Sgr => {
            let terminator = if release { 'm' } else { 'M' };
            Some(format!("\x1b[<{button};{column};{row}{terminator}").into_bytes())
        }
        vt100::MouseProtocolEncoding::Default => {
            let button = if release { 3 + modifiers } else { button };
            if column > 223 || row > 223 {
                return None;
            }
            Some(vec![
                0x1b,
                b'[',
                b'M',
                button + 32,
                column as u8 + 32,
                row as u8 + 32,
            ])
        }
        vt100::MouseProtocolEncoding::Utf8 => {
            let button = if release { 3 + modifiers } else { button };
            let mut sequence = b"\x1b[M".to_vec();
            for value in [
                u32::from(button) + 32,
                u32::from(column) + 32,
                u32::from(row) + 32,
            ] {
                sequence.extend(char::from_u32(value)?.to_string().as_bytes());
            }
            Some(sequence)
        }
    }
}

fn mouse_button(button: MouseButton) -> u8 {
    match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    }
}

fn mouse_modifier_bits(modifiers: KeyModifiers) -> u8 {
    4 * u8::from(modifiers.contains(KeyModifiers::SHIFT))
        + 8 * u8::from(
            modifiers.intersects(KeyModifiers::ALT | KeyModifiers::META | KeyModifiers::SUPER),
        )
        + 16 * u8::from(modifiers.contains(KeyModifiers::CONTROL))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn key_with(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn plain_characters_are_utf8() {
        assert_eq!(
            encode(&key(KeyCode::Char('a')), false, false).unwrap(),
            b"a"
        );
        assert_eq!(
            encode(&key(KeyCode::Char('é')), false, false).unwrap(),
            "é".as_bytes()
        );
    }

    #[test]
    fn control_characters_map_to_c0_bytes() {
        let ctrl_b = key_with(KeyCode::Char('b'), KeyModifiers::CONTROL);
        assert_eq!(encode(&ctrl_b, false, false).unwrap(), vec![0x02]);

        let ctrl_c = key_with(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(encode(&ctrl_c, false, false).unwrap(), vec![0x03]);

        let ctrl_v = key_with(KeyCode::Char('v'), KeyModifiers::CONTROL);
        assert_eq!(encode(&ctrl_v, false, false).unwrap(), vec![0x16]);

        let ctrl_space = key_with(KeyCode::Char(' '), KeyModifiers::CONTROL);
        assert_eq!(encode(&ctrl_space, false, false).unwrap(), vec![0x00]);
    }

    #[test]
    fn uppercase_control_matches_lowercase() {
        let upper = key_with(KeyCode::Char('B'), KeyModifiers::CONTROL);
        assert_eq!(encode(&upper, false, false).unwrap(), vec![0x02]);
    }

    #[test]
    fn control_z_is_blocked_only_when_the_user_opts_in_to_preventing_suspension() {
        for character in ['z', 'Z'] {
            for modifiers in [
                KeyModifiers::CONTROL,
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
                KeyModifiers::CONTROL | KeyModifiers::ALT,
                KeyModifiers::CONTROL | KeyModifiers::META,
                KeyModifiers::CONTROL | KeyModifiers::SUPER,
            ] {
                let key = key_with(KeyCode::Char(character), modifiers);
                assert_eq!(encode(&key, false, true), None);
                let expected = if modifiers
                    .intersects(KeyModifiers::ALT | KeyModifiers::META | KeyModifiers::SUPER)
                {
                    vec![0x1b, 0x1a]
                } else {
                    vec![0x1a]
                };
                assert_eq!(encode(&key, false, false), Some(expected));
            }
        }
    }

    #[test]
    fn z_without_control_still_reaches_copilot_for_typing_and_shortcuts() {
        assert_eq!(encode(&key(KeyCode::Char('z')), false, true).unwrap(), b"z");
        let upper = key_with(KeyCode::Char('Z'), KeyModifiers::SHIFT);
        assert_eq!(encode(&upper, false, true).unwrap(), b"Z");
        let alt_z = key_with(KeyCode::Char('z'), KeyModifiers::ALT);
        assert_eq!(encode(&alt_z, false, true).unwrap(), b"\x1bz");
    }

    #[test]
    fn alt_prefixes_escape() {
        let alt_x = key_with(KeyCode::Char('x'), KeyModifiers::ALT);
        assert_eq!(encode(&alt_x, false, false).unwrap(), vec![0x1b, b'x']);
    }

    #[test]
    fn cursor_keys_respect_application_mode() {
        assert_eq!(encode(&key(KeyCode::Up), false, false).unwrap(), b"\x1b[A");
        assert_eq!(encode(&key(KeyCode::Up), true, false).unwrap(), b"\x1bOA");
        assert_eq!(
            encode(&key(KeyCode::Left), false, false).unwrap(),
            b"\x1b[D"
        );
    }

    #[test]
    fn modified_cursor_keys_use_csi_with_parameters() {
        let ctrl_right = key_with(KeyCode::Right, KeyModifiers::CONTROL);
        // Application mode must not win over the modified form.
        assert_eq!(encode(&ctrl_right, true, false).unwrap(), b"\x1b[1;5C");

        let shift_up = key_with(KeyCode::Up, KeyModifiers::SHIFT);
        assert_eq!(encode(&shift_up, false, false).unwrap(), b"\x1b[1;2A");
    }

    #[test]
    fn editing_keys_use_tilde_sequences() {
        assert_eq!(
            encode(&key(KeyCode::Delete), false, false).unwrap(),
            b"\x1b[3~"
        );
        assert_eq!(
            encode(&key(KeyCode::PageUp), false, false).unwrap(),
            b"\x1b[5~"
        );
        let ctrl_delete = key_with(KeyCode::Delete, KeyModifiers::CONTROL);
        assert_eq!(encode(&ctrl_delete, false, false).unwrap(), b"\x1b[3;5~");
    }

    /// Both routes a terminal can take to Ctrl+J have to reach the child as LF.
    ///
    /// A terminal with win32-input-mode reports the real key; everything else sends the
    /// 0x0A byte, which conhost hands us as Ctrl+Enter. Either way Copilot must see a
    /// newline rather than the carriage return that submits the prompt.
    #[test]
    fn ctrl_j_inserts_a_newline_however_the_terminal_reports_it() {
        let as_char = key_with(KeyCode::Char('j'), KeyModifiers::CONTROL);
        assert_eq!(encode(&as_char, false, false).unwrap(), b"\n");

        let as_ctrl_enter = key_with(KeyCode::Enter, KeyModifiers::CONTROL);
        assert_eq!(encode(&as_ctrl_enter, false, false).unwrap(), b"\n");

        // A plain Enter must still submit.
        assert_eq!(encode(&key(KeyCode::Enter), false, false).unwrap(), b"\r");
    }

    #[test]
    fn enter_tab_backspace_and_escape() {
        assert_eq!(encode(&key(KeyCode::Enter), false, false).unwrap(), b"\r");
        assert_eq!(encode(&key(KeyCode::Tab), false, false).unwrap(), b"\t");
        assert_eq!(
            encode(&key(KeyCode::Backspace), false, false).unwrap(),
            vec![0x7f]
        );
        assert_eq!(
            encode(&key(KeyCode::Esc), false, false).unwrap(),
            vec![0x1b]
        );
        assert_eq!(
            encode(&key(KeyCode::BackTab), false, false).unwrap(),
            b"\x1b[Z"
        );
    }

    #[test]
    fn function_keys_split_between_ss3_and_csi() {
        assert_eq!(
            encode(&key(KeyCode::F(1)), false, false).unwrap(),
            b"\x1bOP"
        );
        assert_eq!(
            encode(&key(KeyCode::F(5)), false, false).unwrap(),
            b"\x1b[15~"
        );
        assert_eq!(
            encode(&key(KeyCode::F(12)), false, false).unwrap(),
            b"\x1b[24~"
        );
        let shift_f1 = key_with(KeyCode::F(1), KeyModifiers::SHIFT);
        assert_eq!(encode(&shift_f1, false, false).unwrap(), b"\x1b[11;2~");
    }

    #[test]
    fn unsupported_keys_are_dropped() {
        assert!(encode(&key(KeyCode::F(20)), false, false).is_none());
        let ctrl_digit = key_with(KeyCode::Char('5'), KeyModifiers::CONTROL);
        assert!(encode(&ctrl_digit, false, false).is_none());
    }

    #[test]
    fn paste_is_bracketed_only_when_enabled() {
        assert_eq!(encode_paste("", false), vec![0x16]);
        assert_eq!(encode_paste("hi", false), b"hi");
        assert_eq!(encode_paste("hi", true), b"\x1b[200~hi\x1b[201~");
    }

    #[test]
    fn paste_newlines_reach_a_bracketed_child_untouched() {
        assert_eq!(
            encode_paste("one\ntwo\n\nthree", true),
            b"\x1b[200~one\ntwo\n\nthree\x1b[201~"
        );
    }

    #[test]
    fn paste_newlines_become_carriage_returns_without_bracketing() {
        // A shell has no way to tell paste from typing, so it must see the same bytes
        // that forwarding each Enter keystroke produced.
        assert_eq!(encode_paste("one\ntwo", false), b"one\rtwo");
        assert_eq!(encode_paste("one\r\ntwo", false), b"one\rtwo");
    }

    #[test]
    fn sgr_mouse_encoding_forwards_scroll_drag_and_release() {
        use vt100::{MouseProtocolEncoding, MouseProtocolMode};

        let scroll = MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 9,
            row: 5,
            modifiers: KeyModifiers::NONE,
        };
        let drag = MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 9,
            row: 5,
            modifiers: KeyModifiers::CONTROL,
        };
        let release = MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 9,
            row: 5,
            modifiers: KeyModifiers::NONE,
        };

        assert_eq!(
            encode_mouse(
                scroll,
                (10, 6),
                MouseProtocolMode::PressRelease,
                MouseProtocolEncoding::Sgr,
            ),
            Some(b"\x1b[<64;10;6M".to_vec())
        );
        assert_eq!(
            encode_mouse(
                drag,
                (10, 6),
                MouseProtocolMode::ButtonMotion,
                MouseProtocolEncoding::Sgr,
            ),
            Some(b"\x1b[<48;10;6M".to_vec())
        );
        assert_eq!(
            encode_mouse(
                release,
                (10, 6),
                MouseProtocolMode::PressRelease,
                MouseProtocolEncoding::Sgr,
            ),
            Some(b"\x1b[<0;10;6m".to_vec())
        );
    }
}
