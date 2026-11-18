#![allow(unused_imports)]
//! Encoding key and mouse events into terminal byte sequences.
use super::*;

pub(crate) fn is_copy_key(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
        && key
            .modifiers
            .intersects(KeyModifiers::SUPER | KeyModifiers::META)
        && !key.modifiers.contains(KeyModifiers::CONTROL)
}

pub(crate) fn terminal_bytes_for_key(key: KeyEvent) -> Option<Vec<u8>> {
    if is_copy_key(key) {
        return None;
    }

    let bytes = match key.code {
        KeyCode::Char(ch) => {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                control_char_bytes(ch)?
            } else if key.modifiers.contains(KeyModifiers::SUPER) {
                return None;
            } else if key
                .modifiers
                .intersects(KeyModifiers::ALT | KeyModifiers::META)
            {
                let mut bytes = vec![0x1b];
                bytes.extend(ch.to_string().into_bytes());
                bytes
            } else {
                ch.to_string().into_bytes()
            }
        }
        // Ctrl+Enter is a different key from Enter to the agents (Claude Code:
        // "send now", which sends a queued message mid-turn). As a bare `\r` it
        // arrived as Enter and only queued, so it goes out kitty-style, with
        // Shift/Alt kept, like Shift+Enter below.
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::CONTROL) => {
            let code = modified_arrow("u", key.modifiers);
            // modified_arrow gives `ESC[1;<mods>u`; CSI-u wants the key's codepoint.
            String::from_utf8_lossy(&code).replacen("[1;", "[13;", 1).into_bytes()
        }
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => b"\x1b[13;2u".to_vec(),
        KeyCode::Enter => b"\r".to_vec(),
        KeyCode::Tab => b"\t".to_vec(),
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Backspace => {
            if key
                .modifiers
                .intersects(KeyModifiers::SUPER | KeyModifiers::META)
            {
                vec![0x15]
            } else if key.modifiers.contains(KeyModifiers::ALT) {
                b"\x1b\x7f".to_vec()
            } else if key.modifiers.contains(KeyModifiers::CONTROL) {
                vec![0x17]
            } else {
                vec![0x7f]
            }
        }
        KeyCode::Esc => vec![0x1b],
        // Bare Alt+arrow is readline word motion; every other modifier mix goes
        // out xterm-style so Shift survives (Codex answers questions on Shift+Left).
        KeyCode::Left if key.modifiers == KeyModifiers::ALT => b"\x1bb".to_vec(),
        KeyCode::Right if key.modifiers == KeyModifiers::ALT => b"\x1bf".to_vec(),
        KeyCode::Left => modified_arrow("D", key.modifiers),
        KeyCode::Right => modified_arrow("C", key.modifiers),
        KeyCode::Up => modified_arrow("A", key.modifiers),
        KeyCode::Down => modified_arrow("B", key.modifiers),
        // Ctrl+Home/Ctrl+End are how Claude Code's fullscreen renderer jumps to the
        // start of the conversation and to the latest message (re-enabling
        // auto-follow) from the normal prompt view, not just inside its transcript
        // mode. Reuse the same SGR modifier encoding as the arrow keys above, or the
        // Ctrl modifier is silently dropped and the child sees a bare Home/End.
        KeyCode::Home => modified_arrow("H", key.modifiers),
        KeyCode::End => modified_arrow("F", key.modifiers),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        // Codex's "f2 to view" (and any agent's F-key binding) needs these.
        KeyCode::F(number @ 1..=12) => function_key_bytes(number, key.modifiers),
        _ => return None,
    };
    Some(bytes)
}

pub(crate) fn bracketed_paste_bytes(text: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(text.len() + "\x1b[200~\x1b[201~".len());
    bytes.extend_from_slice(b"\x1b[200~");
    bytes.extend_from_slice(text.as_bytes());
    bytes.extend_from_slice(b"\x1b[201~");
    bytes
}

pub(crate) fn mouse_event_to_sgr(mouse: MouseEvent, area: Rect) -> Option<Vec<u8>> {
    if !rect_contains(area, mouse.column, mouse.row) {
        return None;
    }

    let x = mouse.column.saturating_sub(area.x).saturating_add(1);
    let y = mouse.row.saturating_sub(area.y).saturating_add(1);
    let (button, press) = match mouse.kind {
        MouseEventKind::Down(button) => (mouse_button_code(button), true),
        MouseEventKind::Up(_) => (3, false),
        MouseEventKind::Drag(button) => (mouse_button_code(button) + 32, true),
        MouseEventKind::Moved => (35, true),
        MouseEventKind::ScrollUp => (64, true),
        MouseEventKind::ScrollDown => (65, true),
        MouseEventKind::ScrollLeft => (66, true),
        MouseEventKind::ScrollRight => (67, true),
    };
    let button = button + mouse_modifier_code(mouse.modifiers);

    Some(
        format!(
            "\x1b[<{};{};{}{}",
            button,
            x,
            y,
            if press { "M" } else { "m" }
        )
        .into_bytes(),
    )
}

pub(crate) fn mouse_button_code(button: MouseButton) -> u16 {
    match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    }
}

pub(crate) fn mouse_modifier_code(modifiers: KeyModifiers) -> u16 {
    let mut code = 0;
    if modifiers.contains(KeyModifiers::SHIFT) {
        code += 4;
    }
    if modifiers.intersects(KeyModifiers::ALT | KeyModifiers::META) {
        code += 8;
    }
    if modifiers.contains(KeyModifiers::CONTROL) {
        code += 16;
    }
    code
}

pub(crate) fn control_char_bytes(ch: char) -> Option<Vec<u8>> {
    let lower = ch.to_ascii_lowercase();
    if lower.is_ascii_lowercase() {
        return Some(vec![(lower as u8) - b'a' + 1]);
    }
    match ch {
        '[' => Some(vec![0x1b]),
        '\\' => Some(vec![0x1c]),
        ']' => Some(vec![0x1d]),
        '^' => Some(vec![0x1e]),
        '_' => Some(vec![0x1f]),
        '?' => Some(vec![0x7f]),
        _ => None,
    }
}

/// xterm's modifier parameter: 1 + Shift(1) + Alt(2) + Ctrl(4).
fn xterm_modifier_code(modifiers: KeyModifiers) -> u8 {
    let mut code = 1;
    if modifiers.contains(KeyModifiers::SHIFT) {
        code += 1;
    }
    if modifiers.intersects(KeyModifiers::ALT | KeyModifiers::META) {
        code += 2;
    }
    if modifiers.contains(KeyModifiers::CONTROL) {
        code += 4;
    }
    code
}

pub(crate) fn modified_arrow(final_byte: &str, modifiers: KeyModifiers) -> Vec<u8> {
    // The parameter is omitted when no modifier is held.
    let code = xterm_modifier_code(modifiers);
    if code == 1 {
        format!("\x1b[{final_byte}").into_bytes()
    } else {
        format!("\x1b[1;{code}{final_byte}").into_bytes()
    }
}

/// xterm function keys: F1-F4 are `ESC O P`..`ESC O S` (`ESC [1;<mods>P`.. when
/// modified), F5-F12 are `ESC [<n>~` (`ESC [<n>;<mods>~` when modified).
fn function_key_bytes(number: u8, modifiers: KeyModifiers) -> Vec<u8> {
    let code = xterm_modifier_code(modifiers);
    if number <= 4 {
        let final_byte = ["P", "Q", "R", "S"][usize::from(number - 1)];
        return if code == 1 {
            format!("\x1bO{final_byte}").into_bytes()
        } else {
            modified_arrow(final_byte, modifiers)
        };
    }
    let n = [15, 17, 18, 19, 20, 21, 23, 24][usize::from(number - 5)];
    if code == 1 {
        format!("\x1b[{n}~").into_bytes()
    } else {
        format!("\x1b[{n};{code}~").into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn function_keys_reach_the_pane_as_xterm_sequences() {
        let key = |number, modifiers| terminal_bytes_for_key(KeyEvent::new(KeyCode::F(number), modifiers));
        let none = KeyModifiers::empty();
        assert_eq!(key(1, none), Some(b"\x1bOP".to_vec()));
        assert_eq!(key(2, none), Some(b"\x1bOQ".to_vec()));
        assert_eq!(key(4, none), Some(b"\x1bOS".to_vec()));
        assert_eq!(key(5, none), Some(b"\x1b[15~".to_vec()));
        assert_eq!(key(10, none), Some(b"\x1b[21~".to_vec()));
        assert_eq!(key(11, none), Some(b"\x1b[23~".to_vec()));
        assert_eq!(key(12, none), Some(b"\x1b[24~".to_vec()));
        assert_eq!(key(2, KeyModifiers::SHIFT), Some(b"\x1b[1;2Q".to_vec()));
        assert_eq!(key(5, KeyModifiers::CONTROL), Some(b"\x1b[15;5~".to_vec()));
        assert_eq!(key(13, none), None);
    }
}
