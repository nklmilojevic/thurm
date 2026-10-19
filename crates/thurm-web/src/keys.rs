//! DOM `KeyboardEvent`s to the protocol's [`KeyEvent`], which `thurm-term` encodes like the
//! apps' keys. The page passes `code` (the physical key), `key` (what the layout produced, with
//! Shift applied) and the modifier bits it decided on (Option composing text on a Mac is not
//! Alt).

use thurm_proto::{Key, KeyAction, KeyEvent, NamedKey, mods};

/// `None` for keys the page leaves to the browser's text input: dead keys, IME processing and
/// keys without a meaning here.
pub fn key_event(code: &str, key: &str, m: u8, action: KeyAction) -> Option<KeyEvent> {
    if let Some(named) = named(code, key) {
        // Keypad keys keep their text (digits, operators) for the legacy encoding.
        let text = match named {
            NamedKey::KpEnter => String::new(),
            _ if code.starts_with("Numpad") && single_char(key).is_some() => key.to_owned(),
            _ => String::new(),
        };
        return Some(KeyEvent {
            key: Key::Named(named),
            mods: m,
            action,
            text: if action == KeyAction::Release {
                String::new()
            } else {
                text
            },
            shifted: None,
            base_layout: None,
        });
    }
    let k = single_char(key).filter(|c| !c.is_control())?;
    let us = us_layout(code);
    let unshifted = if k.is_alphabetic() {
        lower(k)
    } else if let Some((plain, _)) = us.filter(|_| m & (mods::SHIFT | mods::CTRL) != 0) {
        plain
    } else {
        k
    };
    let shifted = if k.is_alphabetic() {
        upper(unshifted)
    } else {
        us.filter(|(plain, _)| *plain == unshifted)
            .map(|(_, shifted)| shifted)
    };
    let base_layout = us.map(|(plain, _)| plain).filter(|&b| b != unshifted);
    // Ctrl, Alt and Super combinations carry no text: the encoder uses the key.
    let text = if action == KeyAction::Release || m & (mods::CTRL | mods::ALT | mods::SUPER) != 0 {
        String::new()
    } else {
        k.to_string()
    };
    Some(KeyEvent {
        key: Key::Char(unshifted),
        mods: m,
        action,
        text,
        shifted: shifted.filter(|&s| s != unshifted),
        base_layout,
    })
}

fn single_char(s: &str) -> Option<char> {
    let mut chars = s.chars();
    let c = chars.next()?;
    chars.next().is_none().then_some(c)
}

fn lower(c: char) -> char {
    single_char(&c.to_lowercase().to_string()).unwrap_or(c)
}

fn upper(c: char) -> Option<char> {
    single_char(&c.to_uppercase().to_string())
}

fn named(code: &str, key: &str) -> Option<NamedKey> {
    use NamedKey::*;
    if code.starts_with("Numpad") {
        let kp = match key {
            "0" => Kp0,
            "1" => Kp1,
            "2" => Kp2,
            "3" => Kp3,
            "4" => Kp4,
            "5" => Kp5,
            "6" => Kp6,
            "7" => Kp7,
            "8" => Kp8,
            "9" => Kp9,
            "." | "," if code == "NumpadDecimal" => KpDecimal,
            "," => KpSeparator,
            "/" => KpDivide,
            "*" => KpMultiply,
            "-" => KpSubtract,
            "+" => KpAdd,
            "=" => KpEqual,
            "Enter" => KpEnter,
            "ArrowLeft" => KpLeft,
            "ArrowRight" => KpRight,
            "ArrowUp" => KpUp,
            "ArrowDown" => KpDown,
            "PageUp" => KpPageUp,
            "PageDown" => KpPageDown,
            "Home" => KpHome,
            "End" => KpEnd,
            "Insert" => KpInsert,
            "Delete" => KpDelete,
            "Clear" => KpBegin,
            _ => return None,
        };
        return Some(kp);
    }
    Some(match key {
        "Escape" => Escape,
        "Enter" => Enter,
        "Tab" => Tab,
        "Backspace" => Backspace,
        "Insert" => Insert,
        "Delete" => Delete,
        "ArrowLeft" => Left,
        "ArrowRight" => Right,
        "ArrowUp" => Up,
        "ArrowDown" => Down,
        "PageUp" => PageUp,
        "PageDown" => PageDown,
        "Home" => Home,
        "End" => End,
        "CapsLock" => CapsLock,
        "ScrollLock" => ScrollLock,
        "NumLock" => NumLock,
        "PrintScreen" => PrintScreen,
        "Pause" => Pause,
        "ContextMenu" => Menu,
        "MediaPlay" => MediaPlay,
        "MediaPause" => MediaPause,
        "MediaPlayPause" => MediaPlayPause,
        "MediaStop" => MediaStop,
        "MediaTrackNext" => MediaNext,
        "MediaTrackPrevious" => MediaPrev,
        "AudioVolumeDown" => VolumeDown,
        "AudioVolumeUp" => VolumeUp,
        "AudioVolumeMute" => VolumeMute,
        "Shift" if code == "ShiftRight" => RightShift,
        "Shift" => LeftShift,
        "Control" if code == "ControlRight" => RightControl,
        "Control" => LeftControl,
        "Alt" | "AltGraph" if code == "AltRight" => RightAlt,
        "Alt" => LeftAlt,
        "Meta" if code == "MetaRight" => RightSuper,
        "Meta" => LeftSuper,
        f if f.len() > 1 && f.starts_with('F') => match f[1..].parse::<u8>() {
            Ok(n @ 1..=35) => F(n),
            _ => return None,
        },
        _ => return None,
    })
}

/// The US layout's characters for a physical key, without and with Shift: kitty's base-layout
/// key, and what Shift turns a digit or symbol into.
fn us_layout(code: &str) -> Option<(char, char)> {
    if let Some(letter) = code.strip_prefix("Key") {
        let c = single_char(letter)?.to_ascii_lowercase();
        return c.is_ascii_lowercase().then(|| (c, c.to_ascii_uppercase()));
    }
    Some(match code {
        "Digit1" => ('1', '!'),
        "Digit2" => ('2', '@'),
        "Digit3" => ('3', '#'),
        "Digit4" => ('4', '$'),
        "Digit5" => ('5', '%'),
        "Digit6" => ('6', '^'),
        "Digit7" => ('7', '&'),
        "Digit8" => ('8', '*'),
        "Digit9" => ('9', '('),
        "Digit0" => ('0', ')'),
        "Minus" => ('-', '_'),
        "Equal" => ('=', '+'),
        "BracketLeft" => ('[', '{'),
        "BracketRight" => (']', '}'),
        "Backslash" | "IntlBackslash" => ('\\', '|'),
        "Semicolon" => (';', ':'),
        "Quote" => ('\'', '"'),
        "Backquote" => ('`', '~'),
        "Comma" => (',', '<'),
        "Period" => ('.', '>'),
        "Slash" => ('/', '?'),
        "Space" => (' ', ' '),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use thurm_term::{EngineConfig, Terminal};

    fn press(code: &str, key: &str, m: u8) -> KeyEvent {
        key_event(code, key, m, KeyAction::Press).unwrap()
    }

    fn bytes(ev: &KeyEvent, setup: &[u8]) -> Vec<u8> {
        let size = thurm_proto::PaneSize {
            cols: 80,
            rows: 24,
            cell_width: 8,
            cell_height: 16,
        };
        let mut term = Terminal::new(size, EngineConfig::default());
        term.advance(setup);
        term.key(ev)
    }

    #[test]
    fn letters_digits_and_symbols() {
        let a = press("KeyA", "a", 0);
        assert_eq!(
            (a.key, a.text.as_str(), a.shifted),
            (Key::Char('a'), "a", Some('A'))
        );
        let shift_a = press("KeyA", "A", mods::SHIFT);
        assert_eq!((shift_a.key, shift_a.text.as_str()), (Key::Char('a'), "A"));
        let bang = press("Digit1", "!", mods::SHIFT);
        assert_eq!(
            (bang.key, bang.text.as_str(), bang.shifted),
            (Key::Char('1'), "!", Some('!'))
        );
        let ctrl_c = press("KeyC", "c", mods::CTRL);
        assert_eq!((ctrl_c.key, ctrl_c.text.as_str()), (Key::Char('c'), ""));
        assert_eq!(press("Space", " ", 0).key, Key::Char(' '));
    }

    #[test]
    fn other_layouts_keep_the_us_key_as_base() {
        // Russian: the A key types "ф".
        let ef = press("KeyA", "ф", 0);
        assert_eq!((ef.key, ef.base_layout), (Key::Char('ф'), Some('a')));
        // German: the Y key types "z"; the encoder's base is the US key.
        let z = press("KeyY", "z", 0);
        assert_eq!((z.key, z.base_layout), (Key::Char('z'), Some('y')));
        assert_eq!(press("KeyA", "a", 0).base_layout, None);
    }

    #[test]
    fn named_and_keypad_keys() {
        assert_eq!(press("Enter", "Enter", 0).key, Key::Named(NamedKey::Enter));
        assert_eq!(press("ArrowUp", "ArrowUp", 0).key, Key::Named(NamedKey::Up));
        assert_eq!(press("F5", "F5", 0).key, Key::Named(NamedKey::F(5)));
        assert_eq!(press("F13", "F13", 0).key, Key::Named(NamedKey::F(13)));
        let kp = press("Numpad5", "5", 0);
        assert_eq!((kp.key, kp.text.as_str()), (Key::Named(NamedKey::Kp5), "5"));
        assert_eq!(
            press("NumpadEnter", "Enter", 0).key,
            Key::Named(NamedKey::KpEnter)
        );
        assert_eq!(
            press("Numpad4", "ArrowLeft", 0).key,
            Key::Named(NamedKey::KpLeft)
        );
        assert_eq!(
            press("ShiftRight", "Shift", mods::SHIFT).key,
            Key::Named(NamedKey::RightShift)
        );
    }

    #[test]
    fn left_to_the_browser() {
        for key in ["Dead", "Process", "Unidentified", "", "ab"] {
            assert!(
                key_event("KeyE", key, 0, KeyAction::Press).is_none(),
                "{key}"
            );
        }
    }

    #[test]
    fn encodes_like_the_apps() {
        assert_eq!(bytes(&press("KeyA", "a", 0), b""), b"a");
        assert_eq!(bytes(&press("KeyA", "A", mods::SHIFT), b""), b"A");
        assert_eq!(bytes(&press("KeyC", "c", mods::CTRL), b""), b"\x03");
        assert_eq!(bytes(&press("KeyX", "x", mods::ALT), b""), b"\x1bx");
        assert_eq!(bytes(&press("Enter", "Enter", 0), b""), b"\r");
        assert_eq!(bytes(&press("ArrowUp", "ArrowUp", 0), b""), b"\x1b[A");
        // Application cursor keys (DECCKM).
        assert_eq!(
            bytes(&press("ArrowUp", "ArrowUp", 0), b"\x1b[?1h"),
            b"\x1bOA"
        );
        assert_eq!(bytes(&press("Digit1", "!", mods::SHIFT), b""), b"!");
        assert_eq!(bytes(&press("KeyA", "ф", 0), b""), "ф".as_bytes());
        // Kitty keyboard protocol, disambiguate: Ctrl+C becomes CSI u.
        assert_eq!(
            bytes(&press("KeyC", "c", mods::CTRL), b"\x1b[>1u"),
            b"\x1b[99;5u"
        );
        assert_eq!(
            bytes(&press("Escape", "Escape", 0), b"\x1b[>1u"),
            b"\x1b[27u"
        );
    }
}
